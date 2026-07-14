// SPDX-License-Identifier: Apache-2.0
//! The WebTransport **server** driver: one thread owning a UDP socket and a
//! table of QUIC connections, each running a server-role [`WtSession`]. It
//! accepts Extended CONNECTs, surfaces each established session (and its stream
//! / datagram events) to JS tagged with a session id, and routes per-session
//! commands back down.
//!
//! Like the client driver, the whole thing runs inside a `catch_unwind` panic
//! boundary and honours the shared in-flight backpressure counter so a hostile
//! peer cannot balloon the neon event queue.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll};

use crate::config::build_server_config;
use crate::driver::{DriverShared, SOCKET_TOKEN};
use crate::session::{Ev, WtSession};

const LOCAL_CONN_ID_LEN: usize = 16;
const MAX_INFLIGHT_BATCHES: i64 = 128;

/// Hard cap on concurrent QUIC connections so a hostile client (or spoofed
/// source addresses) cannot grow the connection table without bound.
const MAX_CONNECTIONS: usize = 16_384;

/// Maximum UDP packets read per event-loop wake before yielding to the rest of
/// the loop (timeouts, commands, reaping). Bounds per-wake work so a sustained
/// inbound flood cannot starve `Shutdown` handling or idle-connection reaping.
const MAX_RECV_PER_WAKE: usize = 1024;

/// Everything the server thread needs to start listening.
pub struct ServerSetup {
    pub host: String,
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
    /// Bind with `SO_REUSEPORT` (Unix) so several processes (e.g. Node `cluster`
    /// workers) can share one listening port and have the kernel load-balance
    /// connections across them.
    pub reuse_port: bool,
}

/// Bind a non-blocking UDP socket, optionally with `SO_REUSEPORT` for clustering.
fn bind_udp(addr: SocketAddr, reuse_port: bool) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    sock.set_nonblocking(true)?;
    #[cfg(unix)]
    if reuse_port {
        sock.set_reuse_port(true)?;
    }
    let _ = reuse_port; // SO_REUSEPORT is Unix-only
    sock.bind(&addr.into())?;
    Ok(UdpSocket::from_std(sock.into()))
}

/// Commands from JS into the server thread. All session-scoped commands carry the
/// session id assigned when the session was established.
pub enum ServerCommand {
    OpenStream {
        session: u64,
        request_id: u64,
        bidi: bool,
    },
    Write {
        session: u64,
        id: u64,
        data: Vec<u8>,
        request_id: u64,
    },
    Fin {
        session: u64,
        id: u64,
    },
    ResetStream {
        session: u64,
        id: u64,
        code: u64,
    },
    StopSending {
        session: u64,
        id: u64,
        code: u64,
    },
    SetPaused {
        session: u64,
        id: u64,
        paused: bool,
    },
    SendDatagram {
        session: u64,
        data: Vec<u8>,
        request_id: u64,
    },
    CloseSession {
        session: u64,
        code: u32,
        reason: Vec<u8>,
    },
    /// Send a DRAIN_WEBTRANSPORT_SESSION capsule to one session's peer.
    Drain {
        session: u64,
    },
    /// Snapshot one session's connection stats; result arrives as a `stats` event.
    GetStats {
        session: u64,
        request_id: u64,
    },
    /// Export TLS keying material (RFC 5705) for one session; result arrives as
    /// a `keyingMaterial` event.
    ExportKeyingMaterial {
        session: u64,
        request_id: u64,
        label: Vec<u8>,
        context: Vec<u8>,
        length: u32,
    },
    /// Tear the whole server down.
    Shutdown,
}

/// Messages from the server thread out to JS.
pub enum ServerMsg {
    /// The server is bound and listening on this UDP port.
    Listening(u16),
    /// A fatal server-level error (setup failure).
    ServerError(String),
    /// The server stopped.
    ServerClosed,
    /// A session-scoped event (`ev`) for session `session`.
    Session(u64, Ev),
}

/// Sink implemented by the neon layer to deliver [`ServerMsg`]s to JS.
pub trait ServerEventSink: Send {
    fn emit(&self, messages: Vec<ServerMsg>);
}

struct ServerConn {
    conn: quiche::Connection,
    session: WtSession,
    session_id: u64,
    closed_emitted: bool,
}

/// Server thread entry point, wrapped in a panic boundary.
pub fn run(
    setup: ServerSetup,
    mut poll: Poll,
    rx: Receiver<ServerCommand>,
    sink: Box<dyn ServerEventSink>,
    shared: Arc<DriverShared>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_inner(setup, &mut poll, rx, sink.as_ref(), &shared);
    }));
    shared.closed.store(true, Ordering::Relaxed);
    if let Err(payload) = result {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        sink.emit(vec![
            ServerMsg::ServerError(format!("rwebtransport server panic: {msg}")),
            ServerMsg::ServerClosed,
        ]);
    } else {
        sink.emit(vec![ServerMsg::ServerClosed]);
    }
}

/// Length of the authentication tag appended to a Retry token.
const TOKEN_TAG_LEN: usize = 16;

/// A serialized source-address identity (IP + port) used as the address the
/// token is bound to. A spoofed source can never receive the Retry, and the tag
/// is keyed by a per-process secret it does not know, so it cannot forge one.
fn addr_identity(src: &SocketAddr) -> Vec<u8> {
    let mut id = match src.ip() {
        std::net::IpAddr::V4(a) => a.octets().to_vec(),
        std::net::IpAddr::V6(a) => a.octets().to_vec(),
    };
    id.extend_from_slice(&src.port().to_be_bytes());
    id
}

/// HMAC-SHA256 (RFC 2104) over `data` keyed by `secret`, built on BoringSSL's
/// SHA-256. Infallible: `boring::sha::sha256` cannot fail.
fn hmac_sha256(secret: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&boring::sha::sha256(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key[i];
        opad[i] ^= key[i];
    }
    let mut inner = Vec::with_capacity(BLOCK + data.len());
    inner.extend_from_slice(&ipad);
    inner.extend_from_slice(data);
    let inner_hash = boring::sha::sha256(&inner);
    let mut outer = Vec::with_capacity(BLOCK + 32);
    outer.extend_from_slice(&opad);
    outer.extend_from_slice(&inner_hash);
    boring::sha::sha256(&outer)
}

/// Constant-time byte comparison so token validation does not leak the tag via
/// timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Mint a stateless Retry token that authenticates the client's source address
/// and carries the original destination connection id. The token is
/// `b"rwt1" || orig_dcid || HMAC(secret, addr || orig_dcid)[..16]`. Because the
/// tag is keyed by a secret known only to this process, a spoofed source cannot
/// forge a token it never received, which is what makes the Retry actually
/// prevent source-address spoofing (reflection) and state exhaustion rather than
/// just being a formality.
fn mint_token(secret: &[u8], orig_dcid: &[u8], src: &SocketAddr) -> Vec<u8> {
    let mut mac_input = addr_identity(src);
    mac_input.extend_from_slice(orig_dcid);
    let tag = hmac_sha256(secret, &mac_input);
    let mut token = Vec::with_capacity(4 + orig_dcid.len() + TOKEN_TAG_LEN);
    token.extend_from_slice(b"rwt1");
    token.extend_from_slice(orig_dcid);
    token.extend_from_slice(&tag[..TOKEN_TAG_LEN]);
    token
}

/// Validate a Retry token against `src` and recover the original destination
/// connection id. Fails closed on any mismatch (forged/replayed/truncated).
fn validate_token<'a>(
    secret: &[u8],
    src: &SocketAddr,
    token: &'a [u8],
) -> Option<quiche::ConnectionId<'a>> {
    let rest = token.strip_prefix(b"rwt1")?;
    if rest.len() < TOKEN_TAG_LEN {
        return None;
    }
    let (orig_dcid, tag) = rest.split_at(rest.len() - TOKEN_TAG_LEN);
    let mut mac_input = addr_identity(src);
    mac_input.extend_from_slice(orig_dcid);
    let expected = hmac_sha256(secret, &mac_input);
    if !constant_time_eq(&expected[..TOKEN_TAG_LEN], tag) {
        return None;
    }
    Some(quiche::ConnectionId::from_ref(orig_dcid))
}

/// A per-process random secret keying the Retry-token HMAC. Regenerated on every
/// start, so tokens are unforgeable and do not survive a restart.
fn random_secret() -> [u8; 32] {
    let mut b = [0u8; 32];
    if getrandom::getrandom(&mut b).is_err() {
        let seed: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let s = seed.to_le_bytes();
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = s[i % s.len()];
        }
    }
    b
}

fn random_scid() -> [u8; LOCAL_CONN_ID_LEN] {
    let mut b = [0u8; LOCAL_CONN_ID_LEN];
    if getrandom::getrandom(&mut b).is_err() {
        let seed: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&seed.to_le_bytes());
    }
    b
}

fn run_inner(
    setup: ServerSetup,
    poll: &mut Poll,
    rx: Receiver<ServerCommand>,
    sink: &dyn ServerEventSink,
    shared: &Arc<DriverShared>,
) {
    let bind: SocketAddr = match format!("{}:{}", setup.host, setup.port).parse() {
        Ok(a) => a,
        Err(e) => {
            sink.emit(vec![ServerMsg::ServerError(format!(
                "invalid bind addr: {e}"
            ))]);
            return;
        }
    };
    let mut socket = match bind_udp(bind, setup.reuse_port) {
        Ok(s) => s,
        Err(e) => {
            sink.emit(vec![ServerMsg::ServerError(format!("bind failed: {e}"))]);
            return;
        }
    };
    let local_addr = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => {
            sink.emit(vec![ServerMsg::ServerError(format!("local addr: {e}"))]);
            return;
        }
    };
    let mut config = match build_server_config(&setup.cert_path, &setup.key_path) {
        Ok(c) => c,
        Err(e) => {
            sink.emit(vec![ServerMsg::ServerError(e)]);
            return;
        }
    };
    if let Err(e) = poll
        .registry()
        .register(&mut socket, SOCKET_TOKEN, Interest::READABLE)
    {
        sink.emit(vec![ServerMsg::ServerError(format!("register: {e}"))]);
        return;
    }

    sink.emit(vec![ServerMsg::Listening(local_addr.port())]);

    let mut events = Events::with_capacity(1024);
    let mut recv_buf = vec![0u8; 65_535];
    let mut send_buf = vec![0u8; 1350];

    let mut clients: HashMap<Vec<u8>, ServerConn> = HashMap::new();
    let mut routing: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut sessions: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut next_session_id: u64 = 1;

    // Per-process secret keying the stateless Retry token (see mint_token).
    let token_secret = random_secret();

    loop {
        let timeout = clients.values().filter_map(|c| c.conn.timeout()).min();
        if poll.poll(&mut events, timeout).is_err() {
            continue;
        }

        let mut out: Vec<ServerMsg> = Vec::new();

        // 1. Read datagrams and route them, up to a per-wake budget so a flood
        // cannot starve the rest of the loop (commands, timeouts, reaping).
        let mut got_packet = false;
        let mut budget = MAX_RECV_PER_WAKE;
        loop {
            if budget == 0 {
                break;
            }
            budget -= 1;
            let (len, from) = match socket.recv_from(&mut recv_buf) {
                Ok(v) => v,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            };
            got_packet = true;
            let pkt = &mut recv_buf[..len];
            let hdr = match quiche::Header::from_slice(pkt, LOCAL_CONN_ID_LEN) {
                Ok(h) => h,
                Err(_) => continue,
            };

            let scid = match routing.get(&hdr.dcid.to_vec()).cloned() {
                Some(s) => s,
                None => {
                    if hdr.ty != quiche::Type::Initial {
                        continue;
                    }
                    // Cap concurrent connections to bound memory under a flood.
                    if clients.len() >= MAX_CONNECTIONS {
                        continue;
                    }

                    let token = hdr.token.as_deref().unwrap_or(&[]);
                    if token.is_empty() {
                        // No address validation yet: reply with a stateless Retry.
                        // We create no connection state (and do not amplify) for a
                        // source that has not proven it owns its address.
                        let new_scid = random_scid();
                        let new_scid_id = quiche::ConnectionId::from_ref(&new_scid);
                        let retry_token = mint_token(&token_secret, &hdr.dcid, &from);
                        if let Ok(len) = quiche::retry(
                            &hdr.scid,
                            &hdr.dcid,
                            &new_scid_id,
                            &retry_token,
                            hdr.version,
                            &mut send_buf,
                        ) {
                            let _ = socket.send_to(&send_buf[..len], from);
                        }
                        continue;
                    }

                    // The client echoed a Retry token: validate it before we
                    // allocate any connection state.
                    let Some(odcid) = validate_token(&token_secret, &from, token) else {
                        continue;
                    };
                    // Our Retry SCID, echoed back as this Initial's DCID.
                    let scid_bytes = hdr.dcid.to_vec();
                    let scid_id = quiche::ConnectionId::from_ref(&scid_bytes);
                    let conn =
                        match quiche::accept(&scid_id, Some(&odcid), local_addr, from, &mut config)
                        {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                    let key = scid_bytes;
                    let session_id = next_session_id;
                    next_session_id += 1;
                    routing.insert(key.clone(), key.clone());
                    sessions.insert(session_id, key.clone());
                    clients.insert(
                        key.clone(),
                        ServerConn {
                            conn,
                            session: WtSession::new_server(from),
                            session_id,
                            closed_emitted: false,
                        },
                    );
                    key
                }
            };

            if let Some(server) = clients.get_mut(&scid) {
                let info = quiche::RecvInfo {
                    from,
                    to: local_addr,
                };
                let _ = server.conn.recv(pkt, info);
            }
        }

        // 2. Fire elapsed timeouts for every connection. quiche's on_timeout is a
        // no-op unless a timer is actually due, and it must run even while packets
        // keep arriving so idle/half-open connections are reaped under a flood.
        let _ = got_packet;
        for server in clients.values_mut() {
            server.conn.on_timeout();
        }

        // 3. Drain commands.
        let mut shutdown = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, ServerCommand::Shutdown) {
                shutdown = true;
                break;
            }
            apply_command(&mut clients, &sessions, cmd, &mut out);
        }
        if shutdown {
            for server in clients.values_mut() {
                let _ = server.conn.close(true, 0, b"server shutdown");
                flush_send(&mut server.conn, &socket, &mut send_buf);
            }
            return;
        }

        // 4. Drive every connection.
        let backpressured = shared.inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT_BATCHES;
        for server in clients.values_mut() {
            let mut evs: Vec<Ev> = Vec::new();
            if server.conn.is_established() {
                server.session.on_established();
            }
            if !backpressured {
                server.session.on_datagrams(&mut server.conn, &mut evs);
            }
            let readable: Vec<u64> = server.conn.readable().collect();
            for id in readable {
                server
                    .session
                    .on_readable(&mut server.conn, id, backpressured, &mut evs);
            }
            server.session.flush(&mut server.conn, &mut evs);

            // If the session ended (app close, or a rejected/malformed CONNECT),
            // close the QUIC connection so it drains and gets reaped instead of
            // lingering as a zombie until the idle timeout.
            if server.session.is_closed() && !server.conn.is_closed() && !server.conn.is_draining()
            {
                let _ = server.conn.close(true, 0, b"session closed");
            }

            if let Some(sz) = server.conn.dgram_max_writable_len() {
                shared
                    .max_datagram_size
                    .store(sz.saturating_sub(1), Ordering::Relaxed);
            }

            for ev in evs {
                out.push(ServerMsg::Session(server.session_id, ev));
            }

            flush_send(&mut server.conn, &socket, &mut send_buf);
        }

        // 5. Reap closed connections (emit a final Closed for the session).
        for server in clients.values_mut() {
            if server.conn.is_closed() && !server.closed_emitted {
                server.closed_emitted = true;
                if !server.session.is_closed() {
                    let (code, reason, remote) = closure_info(&server.conn);
                    let mut evs = Vec::new();
                    server.session.mark_closed(&mut evs, code, reason, remote);
                    for ev in evs {
                        out.push(ServerMsg::Session(server.session_id, ev));
                    }
                }
            }
        }
        clients.retain(|_, s| {
            if s.conn.is_closed() {
                sessions.remove(&s.session_id);
                false
            } else {
                true
            }
        });
        routing.retain(|_, scid| clients.contains_key(scid));

        if !out.is_empty() {
            // The sink increments `inflight` before scheduling and the JS
            // callback decrements it, so every emit path stays balanced.
            sink.emit(out);
        }
    }
}

fn apply_command(
    clients: &mut HashMap<Vec<u8>, ServerConn>,
    sessions: &HashMap<u64, Vec<u8>>,
    cmd: ServerCommand,
    out: &mut Vec<ServerMsg>,
) {
    let session_id = match &cmd {
        ServerCommand::OpenStream { session, .. }
        | ServerCommand::Write { session, .. }
        | ServerCommand::Fin { session, .. }
        | ServerCommand::ResetStream { session, .. }
        | ServerCommand::StopSending { session, .. }
        | ServerCommand::SetPaused { session, .. }
        | ServerCommand::SendDatagram { session, .. }
        | ServerCommand::CloseSession { session, .. }
        | ServerCommand::Drain { session }
        | ServerCommand::GetStats { session, .. }
        | ServerCommand::ExportKeyingMaterial { session, .. } => *session,
        ServerCommand::Shutdown => return,
    };
    let Some(scid) = sessions.get(&session_id) else {
        return;
    };
    let Some(server) = clients.get_mut(scid) else {
        return;
    };
    // Session-scoped events produced by commands are surfaced on the next drive
    // iteration via the normal event path; here we just mutate session state.
    let mut evs: Vec<Ev> = Vec::new();
    match cmd {
        ServerCommand::OpenStream {
            request_id, bidi, ..
        } => server.session.open_stream(bidi, request_id, &mut evs),
        ServerCommand::Write {
            id,
            data,
            request_id,
            ..
        } => server.session.stream_write(id, data, request_id, &mut evs),
        ServerCommand::Fin { id, .. } => server.session.stream_fin(id),
        ServerCommand::ResetStream { id, code, .. } => {
            server.session.stream_reset(&mut server.conn, id, code)
        }
        ServerCommand::StopSending { id, code, .. } => {
            server
                .session
                .stream_stop_sending(&mut server.conn, id, code)
        }
        ServerCommand::SetPaused { id, paused, .. } => server.session.set_paused(id, paused),
        ServerCommand::SendDatagram {
            data, request_id, ..
        } => server
            .session
            .send_datagram(&mut server.conn, &data, request_id, &mut evs),
        ServerCommand::CloseSession { code, reason, .. } => server.session.close(code, reason),
        ServerCommand::Drain { .. } => server.session.send_drain(),
        ServerCommand::GetStats { request_id, .. } => {
            evs.push(crate::session::build_stats(&server.conn, request_id))
        }
        ServerCommand::ExportKeyingMaterial {
            request_id,
            label,
            context,
            length,
            ..
        } => evs.push(crate::session::build_keying_material(
            &mut server.conn,
            request_id,
            &label,
            &context,
            length,
        )),
        ServerCommand::Shutdown => {}
    }
    // Surface events produced synchronously by the command (StreamOpened,
    // WriteAck, DatagramAck), tagged with the session id.
    for ev in evs {
        out.push(ServerMsg::Session(session_id, ev));
    }
}

fn flush_send(conn: &mut quiche::Connection, socket: &UdpSocket, out: &mut [u8]) {
    loop {
        match conn.send(out) {
            Ok((write, info)) => match socket.send_to(&out[..write], info.to) {
                Ok(_) => {}
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Kernel send buffer is full. Stop draining rather than
                    // discarding packets quiche already accounted as sent; its
                    // loss recovery retransmits the frames on a later flush.
                    return;
                }
                Err(_) => return,
            },
            Err(quiche::Error::Done) => return,
            Err(_) => return,
        }
    }
}

fn closure_info(conn: &quiche::Connection) -> (u32, Vec<u8>, bool) {
    if let Some(err) = conn.peer_error() {
        return (err.error_code as u32, err.reason.clone(), true);
    }
    if let Some(err) = conn.local_error() {
        return (err.error_code as u32, err.reason.clone(), false);
    }
    (0, Vec::new(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("addr")
    }

    #[test]
    fn token_roundtrip_recovers_dcid() {
        let secret = [7u8; 32];
        let dcid = b"\x11\x22\x33\x44\x55\x66\x77\x88";
        let src = addr("192.0.2.10:4433");
        let token = mint_token(&secret, dcid, &src);
        let recovered = validate_token(&secret, &src, &token).expect("valid token");
        assert_eq!(recovered.as_ref(), &dcid[..]);
    }

    #[test]
    fn token_rejected_for_different_source_address() {
        let secret = [7u8; 32];
        let dcid = b"abcdefghij";
        let token = mint_token(&secret, dcid, &addr("192.0.2.10:4433"));
        // Spoofed source IP: the whole point of the Retry.
        assert!(validate_token(&secret, &addr("192.0.2.11:4433"), &token).is_none());
        // Same IP, different port: still not the address the token was minted for.
        assert!(validate_token(&secret, &addr("192.0.2.10:5555"), &token).is_none());
    }

    #[test]
    fn token_rejected_under_a_different_secret() {
        let dcid = b"abcdefghij";
        let src = addr("198.51.100.7:443");
        let token = mint_token(&[1u8; 32], dcid, &src);
        // A server that restarted (new secret) rejects the stale token.
        assert!(validate_token(&[2u8; 32], &src, &token).is_none());
    }

    #[test]
    fn forged_and_truncated_tokens_are_rejected() {
        let secret = [9u8; 32];
        let src = addr("198.51.100.7:443");
        // Attacker who does not know the secret guesses the layout with a zero tag.
        let mut forged = Vec::new();
        forged.extend_from_slice(b"rwt1");
        forged.extend_from_slice(b"deadbeefdcid");
        forged.extend_from_slice(&[0u8; TOKEN_TAG_LEN]);
        assert!(validate_token(&secret, &src, &forged).is_none());
        // Too short to hold a tag, wrong prefix, empty.
        assert!(validate_token(&secret, &src, b"rwt1short").is_none());
        assert!(validate_token(&secret, &src, b"nope").is_none());
        assert!(validate_token(&secret, &src, b"").is_none());
        // A valid token with a single tag byte flipped.
        let mut token = mint_token(&secret, b"abcdefghij", &src);
        let last = token.len() - 1;
        token[last] ^= 0xff;
        assert!(validate_token(&secret, &src, &token).is_none());
    }

    #[test]
    fn hmac_is_deterministic_and_key_dependent() {
        assert_eq!(
            hmac_sha256(&[0u8; 32], b"msg"),
            hmac_sha256(&[0u8; 32], b"msg")
        );
        assert_ne!(
            hmac_sha256(&[0u8; 32], b"msg"),
            hmac_sha256(&[1u8; 32], b"msg")
        );
        // Known RFC 4231 test case 1: key=0x0b*20, data="Hi There".
        let key = [0x0bu8; 20];
        let tag = hmac_sha256(&key, b"Hi There");
        let expected =
            hex_to_bytes("b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7");
        assert_eq!(&tag[..], &expected[..]);
    }

    fn hex_to_bytes(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
