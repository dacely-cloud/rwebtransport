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
use std::time::{Duration, Instant};

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll};

use wtcore::h3;

use crate::config::build_server_config;
use crate::driver::{
    is_udp_port_unreachable, DriverShared, MAX_INFLIGHT_BATCHES, MAX_RECV_PER_WAKE,
    MAX_RECV_UDP_PAYLOAD, SOCKET_TOKEN,
};
use crate::parser_memory::ParserMemory;
use crate::session::{Ev, ReceiveBudget, WtSession};

const LOCAL_CONN_ID_LEN: usize = 16;

/// Cap live transport state, including incomplete handshakes and draining connections.
const MAX_CONNECTIONS: usize = 256;
const SESSION_SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Aggregate allocation capacity of HTTP/3, classification and capsule parser
/// buffers across all connections, including handshakes and draining sessions.
const MAX_PARSER_MEMORY: usize = 64 * 1024 * 1024;

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
    /// WebTransport subprotocols the server supports (preference order).
    pub supported_protocols: Vec<String>,
    /// If `Some`, only Extended CONNECTs whose `origin` is in this list are
    /// accepted; others are rejected with 403. `None` allows any origin.
    pub allowed_origins: Option<Vec<String>>,
    /// Extra headers to add to every 200 CONNECT response.
    pub response_headers: Vec<(String, String)>,
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
    setup_deadline: Option<Instant>,
    max_datagram_size: usize,
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
    let mut id = Vec::with_capacity(19);
    match src.ip() {
        std::net::IpAddr::V4(a) => {
            id.push(4);
            id.extend_from_slice(&a.octets());
        }
        std::net::IpAddr::V6(a) => {
            id.push(6);
            id.extend_from_slice(&a.octets());
        }
    }
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

struct RetryTokens {
    secret: [u8; 32],
    started: Instant,
}

impl RetryTokens {
    const TTL: Duration = Duration::from_secs(10);

    /// Authenticate the timestamp and original DCID, binding the token to both
    /// the source address and the DCID that this Retry issued.
    fn tag(&self, src: &SocketAddr, retry_scid: &[u8], body: &[u8]) -> [u8; 32] {
        let mut input = addr_identity(src);
        input.extend_from_slice(retry_scid);
        input.extend_from_slice(body);
        hmac_sha256(&self.secret, &input)
    }

    fn mint(&self, src: &SocketAddr, retry_scid: &[u8], orig_dcid: &[u8]) -> Vec<u8> {
        let mut token = self.started.elapsed().as_secs().to_be_bytes().to_vec();
        token.extend_from_slice(orig_dcid);
        token.extend_from_slice(&self.tag(src, retry_scid, &token)[..TOKEN_TAG_LEN]);
        token
    }

    fn validate<'a>(
        &self,
        src: &SocketAddr,
        retry_scid: &[u8],
        token: &'a [u8],
    ) -> Option<quiche::ConnectionId<'a>> {
        const TIMESTAMP_LEN: usize = size_of::<u64>();
        if retry_scid.len() != LOCAL_CONN_ID_LEN
            || !(TIMESTAMP_LEN + 8 + TOKEN_TAG_LEN
                ..=TIMESTAMP_LEN + quiche::MAX_CONN_ID_LEN + TOKEN_TAG_LEN)
                .contains(&token.len())
        {
            return None;
        }
        let (body, tag) = token.split_at(token.len() - TOKEN_TAG_LEN);
        let (issued, orig_dcid) = body.split_at(TIMESTAMP_LEN);
        let issued = u64::from_be_bytes(issued.try_into().ok()?);
        let age = self.started.elapsed().as_secs().checked_sub(issued)?;
        if age > Self::TTL.as_secs()
            || !boring::memcmp::eq(&self.tag(src, retry_scid, body)[..TOKEN_TAG_LEN], tag)
        {
            return None;
        }
        Some(quiche::ConnectionId::from_ref(orig_dcid))
    }
}

fn run_inner(
    setup: ServerSetup,
    poll: &mut Poll,
    rx: Receiver<ServerCommand>,
    sink: &dyn ServerEventSink,
    shared: &Arc<DriverShared>,
) {
    let bind: SocketAddr = match setup
        .host
        .trim_matches(['[', ']'])
        .parse::<std::net::IpAddr>()
        .map(|ip| SocketAddr::new(ip, setup.port))
    {
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

    let mut secret = [0; 32];
    if let Err(e) = getrandom::fill(&mut secret) {
        sink.emit(vec![ServerMsg::ServerError(format!(
            "Retry key generation: {e}"
        ))]);
        return;
    }
    let tokens = RetryTokens {
        secret,
        started: Instant::now(),
    };
    sink.emit(vec![ServerMsg::Listening(local_addr.port())]);

    let mut events = Events::with_capacity(1024);
    let mut recv_buf = vec![0u8; 65_535];
    let mut send_buf = vec![0u8; 1350];

    let mut clients: HashMap<Vec<u8>, ServerConn> = HashMap::new();
    let mut routing: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut sessions: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut next_session_id: u64 = 1;
    let parser_memory = ParserMemory::new(MAX_PARSER_MEMORY);

    let mut udp_pending = false;
    let mut read_pending = false;
    let mut next_connection: usize = 0;

    loop {
        let can_read = shared.inflight.load(Ordering::Relaxed) < MAX_INFLIGHT_BATCHES;
        let timeout = if udp_pending || (read_pending && can_read) {
            Some(Duration::ZERO)
        } else {
            let now = Instant::now();
            clients
                .values()
                .flat_map(|c| {
                    [
                        c.conn.timeout(),
                        c.setup_deadline.map(|d| d.saturating_duration_since(now)),
                    ]
                })
                .flatten()
                .min()
        };
        if let Err(e) = poll.poll(&mut events, timeout) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            sink.emit(vec![ServerMsg::ServerError(format!("poll error: {e}"))]);
            return;
        }

        let mut out: Vec<ServerMsg> = Vec::new();

        // 1. Read datagrams and route them, up to a per-wake budget so a flood
        // cannot starve the rest of the loop (commands, timeouts, reaping).
        let mut budget = MAX_RECV_PER_WAKE;
        while budget > 0 {
            budget -= 1;
            let (len, from) = match socket.recv_from(&mut recv_buf) {
                Ok(v) => v,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(ref e) if is_udp_port_unreachable(e) => continue,
                Err(_) => break,
            };
            if len > MAX_RECV_UDP_PAYLOAD {
                continue;
            }
            let pkt = &mut recv_buf[..len];
            let hdr = match quiche::Header::from_slice(pkt, LOCAL_CONN_ID_LEN) {
                Ok(h) => h,
                Err(_) => continue,
            };

            let scid = match routing.get(&hdr.dcid.to_vec()).cloned() {
                Some(s) => s,
                None => {
                    if hdr.ty != quiche::Type::Initial
                        || len < 1200
                        || !quiche::version_is_supported(hdr.version)
                        || hdr.dcid.len() < 8
                    {
                        continue;
                    }
                    // Cap concurrent connections to bound memory under a flood.
                    if clients.len() >= MAX_CONNECTIONS
                        || shared.inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT_BATCHES
                    {
                        continue;
                    }

                    let token = hdr.token.as_deref().unwrap_or(&[]);
                    if token.is_empty() {
                        // No address validation yet: reply with a stateless Retry.
                        // We create no connection state (and do not amplify) for a
                        // source that has not proven it owns its address.
                        let mut new_scid = [0; LOCAL_CONN_ID_LEN];
                        if getrandom::fill(&mut new_scid).is_err() {
                            continue;
                        }
                        let new_scid_id = quiche::ConnectionId::from_ref(&new_scid);
                        let retry_token = tokens.mint(&from, &new_scid, &hdr.dcid);
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
                    let Some(odcid) = tokens.validate(&from, &hdr.dcid, token) else {
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
                            session: WtSession::new_server(
                                from,
                                setup.supported_protocols.clone(),
                                setup.allowed_origins.clone(),
                                setup.response_headers.clone(),
                                parser_memory.clone(),
                            ),
                            session_id,
                            setup_deadline: Some(Instant::now() + SESSION_SETUP_TIMEOUT),
                            max_datagram_size: 0,
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
        udp_pending = budget == 0;

        // 2. Fire elapsed timeouts for every connection. quiche's on_timeout is a
        // no-op unless a timer is actually due, and it must run even while packets
        // keep arriving so idle/half-open connections are reaped under a flood.
        let now = Instant::now();
        for server in clients.values_mut() {
            server.conn.on_timeout();
            if server
                .setup_deadline
                .is_some_and(|deadline| now >= deadline)
            {
                // The deadline covers both QUIC handshake and Extended CONNECT.
                server.setup_deadline = None;
                let _ = server
                    .conn
                    .close(true, h3::H3_NO_ERROR, b"session setup timed out");
            }
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
        let can_read = shared.inflight.load(Ordering::Relaxed) < MAX_INFLIGHT_BATCHES;
        let mut budget = ReceiveBudget::new();
        let mut ordered: Vec<_> = clients.values_mut().collect();
        let start = next_connection.checked_rem(ordered.len()).unwrap_or(0);
        ordered.rotate_left(start);
        read_pending = false;
        for (index, server) in ordered.into_iter().enumerate() {
            if server.conn.is_closed() || server.conn.local_error().is_some() {
                flush_send(&mut server.conn, &socket, &mut send_buf);
                continue;
            }
            let mut evs: Vec<Ev> = Vec::new();
            if server.conn.is_established() {
                server
                    .session
                    .on_established(server.conn.reliable_stream_reset_negotiated());
            }
            if can_read && !budget.exhausted() {
                server
                    .session
                    .read_pending(&mut server.conn, &mut budget, &mut evs);
                if budget.exhausted() {
                    next_connection = start + index + 1;
                    read_pending = true;
                }
            }
            if server.session.is_ready() {
                server.setup_deadline = None;
            }
            server.session.flush(&mut server.conn, &mut evs);

            // If the session ended (app close, or a rejected/malformed CONNECT),
            // close the QUIC connection so it drains and gets reaped instead of
            // lingering as a zombie until the idle timeout.
            if server.session.is_closed() && !server.conn.is_closed() && !server.conn.is_draining()
            {
                let _ = server.conn.close(true, h3::H3_NO_ERROR, b"");
            }

            if server.session.is_ready() && !server.session.is_closed() {
                let size = server.session.max_datagram_size(&server.conn);
                if size != server.max_datagram_size {
                    server.max_datagram_size = size;
                    evs.push(Ev::DatagramSize(size));
                }
            }

            for ev in evs {
                if std::env::var_os("RWT_DEBUG").is_some() && matches!(ev, Ev::Error(_)) {
                    eprintln!("[rwt-server] {ev:?}");
                }
                out.push(ServerMsg::Session(server.session_id, ev));
            }

            flush_send(&mut server.conn, &socket, &mut send_buf);
        }

        // 5. Reap closed connections (emit a final Closed for the session).
        for server in clients.values_mut() {
            if server.conn.is_closed() && server.session.is_ready() && !server.session.is_closed() {
                let mut evs = Vec::new();
                if let Some((code, reason)) = server.session.local_close_info() {
                    // Locally initiated graceful WebTransport close: surface the
                    // WT code/reason (the wire close used H3_NO_ERROR).
                    server.session.mark_closed(&mut evs, code, reason, false);
                } else {
                    let (code, reason, remote, err) = closure_info(&server.conn);
                    if let Some(msg) = err {
                        evs.push(Ev::Error(msg));
                    }
                    server.session.mark_closed(&mut evs, code, reason, remote);
                }
                for ev in evs {
                    out.push(ServerMsg::Session(server.session_id, ev));
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
        } => evs.push(server.session.build_keying_material(
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

/// Extract a close code/reason from a terminated connection. The fourth element
/// is `Some(message)` for an abnormal transport-level termination (idle timeout,
/// transport CONNECTION_CLOSE with `is_app == false`, stateless reset), which is
/// surfaced as an `Ev::Error` so the JS `closed` promise rejects; a clean
/// application close keeps its WebTransport code/reason and returns `None`.
fn closure_info(conn: &quiche::Connection) -> (u32, Vec<u8>, bool, Option<String>) {
    if let Some(err) = conn.peer_error() {
        if err.is_app {
            return (err.error_code as u32, err.reason.clone(), true, None);
        }
        return (
            0,
            Vec::new(),
            true,
            Some(format!(
                "connection terminated by peer transport error {:#x}",
                err.error_code
            )),
        );
    }
    if let Some(err) = conn.local_error() {
        if err.is_app {
            return (err.error_code as u32, err.reason.clone(), false, None);
        }
        return (
            0,
            Vec::new(),
            false,
            Some(format!(
                "connection closed by transport error {:#x}",
                err.error_code
            )),
        );
    }
    if conn.is_timed_out() {
        return (
            0,
            Vec::new(),
            false,
            Some("connection timed out".to_string()),
        );
    }
    (
        0,
        Vec::new(),
        true,
        Some("connection closed abnormally".to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().expect("addr")
    }

    #[test]
    fn retry_token_authenticates_address_and_connection() {
        let mut tokens = RetryTokens {
            secret: [7; 32],
            started: Instant::now(),
        };
        let src = addr("192.0.2.10:4433");
        let dcid = b"original id";
        let scid = [42; LOCAL_CONN_ID_LEN];
        let token = tokens.mint(&src, &scid, dcid);
        assert_eq!(tokens.validate(&src, &scid, &token).unwrap().as_ref(), dcid);
        assert!(tokens
            .validate(&addr("192.0.2.11:4433"), &scid, &token)
            .is_none());
        assert!(tokens
            .validate(&addr("192.0.2.10:1234"), &scid, &token)
            .is_none());
        assert!(tokens
            .validate(&src, &[41; LOCAL_CONN_ID_LEN], &token)
            .is_none());
        assert!(tokens.validate(&src, &scid, &token[..7]).is_none());
        let mut altered = token.clone();
        *altered.last_mut().unwrap() ^= 1;
        assert!(tokens.validate(&src, &scid, &altered).is_none());
        tokens.started -= RetryTokens::TTL + Duration::from_secs(1);
        assert!(tokens.validate(&src, &scid, &token).is_none());
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
