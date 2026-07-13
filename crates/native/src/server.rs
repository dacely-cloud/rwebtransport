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

/// Everything the server thread needs to start listening.
pub struct ServerSetup {
    pub host: String,
    pub port: u16,
    pub cert_path: String,
    pub key_path: String,
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
    let mut socket = match UdpSocket::bind(bind) {
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

    loop {
        let timeout = clients.values().filter_map(|c| c.conn.timeout()).min();
        if poll.poll(&mut events, timeout).is_err() {
            continue;
        }

        let mut out: Vec<ServerMsg> = Vec::new();

        // 1. Read all datagrams and route them.
        let mut got_packet = false;
        loop {
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
                    let scid_bytes = random_scid();
                    let scid_id = quiche::ConnectionId::from_ref(&scid_bytes);
                    let conn = match quiche::accept(&scid_id, None, local_addr, from, &mut config) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let key = scid_bytes.to_vec();
                    let session_id = next_session_id;
                    next_session_id += 1;
                    routing.insert(hdr.dcid.to_vec(), key.clone());
                    routing.insert(key.clone(), key.clone());
                    sessions.insert(session_id, key.clone());
                    clients.insert(
                        key.clone(),
                        ServerConn {
                            conn,
                            session: WtSession::new_server(),
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

        // 2. Fire elapsed timeouts.
        if !got_packet {
            for server in clients.values_mut() {
                server.conn.on_timeout();
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
        let backpressured = shared.inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT_BATCHES;
        for server in clients.values_mut() {
            let mut evs: Vec<Ev> = Vec::new();
            if server.conn.is_established() {
                server.session.on_established(&mut evs);
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
            shared.inflight.fetch_add(1, Ordering::Relaxed);
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
        | ServerCommand::CloseSession { session, .. } => *session,
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
            Ok((write, info)) => {
                let _ = socket.send_to(&out[..write], info.to);
            }
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
