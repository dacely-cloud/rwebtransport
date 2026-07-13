// SPDX-License-Identifier: Apache-2.0
//! The per-session driver thread: a mio event loop that owns the UDP socket and
//! the quiche connection, feeds packets to [`crate::session::WtSession`], and
//! ferries commands in and events out.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Instant;

use mio::net::UdpSocket;
use mio::{Events, Poll, Token};

use crate::config::{build_config, ClientConfigParams};
use crate::session::{Ev, WtSession};

pub const SOCKET_TOKEN: Token = Token(0);
pub const WAKE_TOKEN: Token = Token(1);

/// Max number of event batches that may be in flight to the JS event loop before
/// the driver stops pulling high-volume data (datagrams + WebTransport stream
/// bytes) out of quiche. This bounds the neon Channel / libuv queue so a hostile
/// server flooding datagrams or stream data cannot grow it without limit (OOM).
/// When exceeded, unread stream data stays in quiche (flow-controlling the peer)
/// and excess datagrams are dropped by quiche's bounded recv queue.
const MAX_INFLIGHT_BATCHES: i64 = 128;

/// State shared between the driver thread and the JS-facing handle so that
/// synchronous property reads (e.g. `datagrams.maxDatagramSize`) don't require a
/// round trip through the command channel.
#[derive(Default)]
pub struct DriverShared {
    pub max_datagram_size: AtomicUsize,
    pub ready: AtomicBool,
    pub closed: AtomicBool,
    /// Event batches emitted but not yet processed by the JS thread. Incremented
    /// by the sink before `Channel::send`, decremented inside the JS callback.
    pub inflight: AtomicI64,
}

/// Commands sent from JS (via the neon layer) into the driver thread.
pub enum Command {
    OpenStream {
        request_id: u64,
        bidi: bool,
    },
    Write {
        id: u64,
        data: Vec<u8>,
        request_id: u64,
    },
    Fin {
        id: u64,
    },
    ResetStream {
        id: u64,
        code: u64,
    },
    StopSending {
        id: u64,
        code: u64,
    },
    SetPaused {
        id: u64,
        paused: bool,
    },
    SendDatagram {
        data: Vec<u8>,
        request_id: u64,
    },
    Close {
        code: u32,
        reason: Vec<u8>,
    },
    /// Tear the driver down (the JS handle was closed/GC'd).
    Shutdown,
}

/// Sink for events produced by the driver. Implemented by the neon layer using a
/// `neon::event::Channel`.
pub trait EventSink: Send {
    fn emit(&self, events: Vec<Ev>);
}

/// Everything the driver thread needs to establish a session. DNS resolution,
/// socket bind, TLS/QUIC config, and the quiche handshake all happen on the
/// driver thread (never on the JS event loop).
pub struct SessionSetup {
    pub host: String,
    pub port: u16,
    /// SNI / verification host (None for IP literals).
    pub sni: Option<String>,
    pub authority: String,
    pub path: String,
    pub origin: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub config: ClientConfigParams,
}

/// Entry point for the driver thread. Wraps setup + the event loop in
/// `catch_unwind` so that a panic anywhere (DNS, bind, quiche, the loop) can
/// NEVER crash the Node process and can NEVER silently hang the session: on
/// panic (or a setup failure) we emit an `error` event followed by `closed`, so
/// the JS `ready`/`closed` promises reject.
pub fn run(
    setup: SessionSetup,
    poll: Poll,
    rx: Receiver<Command>,
    sink: Box<dyn EventSink>,
    shared: Arc<DriverShared>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        setup_and_run(setup, poll, rx, sink.as_ref(), &shared);
    }));
    if let Err(payload) = result {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown panic".to_string());
        shared.closed.store(true, Ordering::Relaxed);
        sink.emit(vec![
            Ev::Error(format!("rwebtransport internal driver error: {msg}")),
            Ev::Closed {
                code: 0,
                reason: b"driver panic".to_vec(),
                remote: false,
            },
        ]);
    }
}

/// Emit a fatal setup error to JS and mark the session closed.
fn emit_fatal(sink: &dyn EventSink, shared: &Arc<DriverShared>, message: String) {
    shared.closed.store(true, Ordering::Relaxed);
    sink.emit(vec![
        Ev::Error(message),
        Ev::Closed {
            code: 0,
            reason: b"setup failed".to_vec(),
            remote: false,
        },
    ]);
}

fn random_scid() -> [u8; 16] {
    let mut b = [0u8; 16];
    if getrandom::getrandom(&mut b).is_err() {
        let seed: u128 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b.copy_from_slice(&seed.to_le_bytes());
    }
    b
}

/// Resolve DNS, bind the UDP socket, build the QUIC config, perform the handshake
/// handshake, then run the event loop. Any setup failure is surfaced to JS.
fn setup_and_run(
    setup: SessionSetup,
    poll: Poll,
    rx: Receiver<Command>,
    sink: &dyn EventSink,
    shared: &Arc<DriverShared>,
) {
    // DNS resolution — potentially blocking, so it runs here (driver thread),
    // never on the JS event loop.
    let peer = match (setup.host.as_str(), setup.port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut it| it.next())
    {
        Some(p) => p,
        None => {
            emit_fatal(
                sink,
                shared,
                format!("failed to resolve {}:{}", setup.host, setup.port),
            );
            return;
        }
    };

    let bind: SocketAddr = if peer.is_ipv4() {
        "0.0.0.0:0".parse().unwrap()
    } else {
        "[::]:0".parse().unwrap()
    };
    let socket = match UdpSocket::bind(bind) {
        Ok(s) => s,
        Err(e) => {
            emit_fatal(sink, shared, format!("failed to bind UDP socket: {e}"));
            return;
        }
    };
    let local_addr = match socket.local_addr() {
        Ok(a) => a,
        Err(e) => {
            emit_fatal(sink, shared, format!("failed to read local addr: {e}"));
            return;
        }
    };

    let mut config = match build_config(&setup.config) {
        Ok(c) => c,
        Err(e) => {
            emit_fatal(sink, shared, e);
            return;
        }
    };

    let scid = random_scid();
    let scid = quiche::ConnectionId::from_ref(&scid);
    let conn = match quiche::connect(setup.sni.as_deref(), &scid, local_addr, peer, &mut config) {
        Ok(c) => c,
        Err(e) => {
            emit_fatal(sink, shared, format!("quiche connect: {e:?}"));
            return;
        }
    };

    let session = WtSession::new(
        setup.authority,
        setup.path,
        setup.origin,
        setup.extra_headers,
    );

    run_inner(poll, socket, local_addr, conn, session, rx, sink, shared);
}

/// The actual event loop. Runs until the connection closes or a fatal error
/// occurs. Never call this directly from the spawned thread — go through
/// [`run`], which contains the panic boundary.
#[allow(clippy::too_many_arguments)]
fn run_inner(
    mut poll: Poll,
    mut socket: UdpSocket,
    local_addr: std::net::SocketAddr,
    mut conn: quiche::Connection,
    mut session: WtSession,
    rx: Receiver<Command>,
    sink: &dyn EventSink,
    shared: &Arc<DriverShared>,
) {
    if let Err(e) = poll
        .registry()
        .register(&mut socket, SOCKET_TOKEN, mio::Interest::READABLE)
    {
        sink.emit(vec![Ev::Error(format!("failed to register socket: {e}"))]);
        return;
    }

    let mut events = Events::with_capacity(1024);
    let mut recv_buf = vec![0u8; 65_535];
    let mut send_buf = vec![0u8; 1350];

    let debug = std::env::var("RWT_DEBUG").is_ok();
    // Read once per driver thread (each session spawns a fresh thread), so tests
    // can arm/disarm it per-connection via the environment.
    let test_panic = std::env::var("RWT_TEST_PANIC").is_ok();
    let mut established_logged = false;
    if debug {
        eprintln!("[rwt-driver] start local={local_addr}");
    }

    // Prime the handshake (send the client Initial).
    flush_send(&mut conn, &socket, &mut send_buf, debug);

    loop {
        let timeout = conn.timeout();
        if let Err(e) = poll.poll(&mut events, timeout) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            sink.emit(vec![Ev::Error(format!("poll error: {e}"))]);
            return;
        }

        let mut evs: Vec<Ev> = Vec::new();
        let mut shutdown = false;

        // 1. Read all datagrams the socket has.
        let mut got_packet = false;
        loop {
            match socket.recv_from(&mut recv_buf) {
                Ok((len, from)) => {
                    got_packet = true;
                    if debug {
                        eprintln!("[rwt-driver] recv {len} bytes from {from}");
                    }
                    let info = quiche::RecvInfo {
                        from,
                        to: local_addr,
                    };
                    match conn.recv(&mut recv_buf[..len], info) {
                        Ok(_) => {}
                        Err(quiche::Error::Done) => {}
                        Err(e) => {
                            if debug {
                                eprintln!("[rwt-driver] recv err {e:?}");
                            }
                            evs.push(Ev::Error(format!("quic recv: {e:?}")));
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    evs.push(Ev::Error(format!("socket recv: {e}")));
                    break;
                }
            }
        }

        // 2. Fire the timeout if nothing else woke us.
        if !got_packet && events.is_empty() {
            conn.on_timeout();
        }

        // 3. Drain commands.
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::OpenStream { request_id, bidi } => {
                    session.open_stream(bidi, request_id, &mut evs)
                }
                Command::Write {
                    id,
                    data,
                    request_id,
                } => session.stream_write(id, data, request_id, &mut evs),
                Command::Fin { id } => session.stream_fin(id),
                Command::ResetStream { id, code } => session.stream_reset(&mut conn, id, code),
                Command::StopSending { id, code } => {
                    session.stream_stop_sending(&mut conn, id, code)
                }
                Command::SetPaused { id, paused } => session.set_paused(id, paused),
                Command::SendDatagram { data, request_id } => {
                    session.send_datagram(&mut conn, &data, request_id, &mut evs)
                }
                Command::Close { code, reason } => session.close(code, reason),
                Command::Shutdown => {
                    shutdown = true;
                    break;
                }
            }
        }

        if shutdown {
            let _ = conn.close(true, 0, b"");
            flush_send(&mut conn, &socket, &mut send_buf, debug);
            return;
        }

        // 4. Drive the session forward.
        if conn.is_established() {
            // Test-only hook: prove that a panic on the driver thread is
            // contained (rejects ready/closed, never crashes Node). Gated behind
            // an env var so it is inert in production.
            if test_panic {
                panic!("RWT_TEST_PANIC: intentional driver-thread panic for containment testing");
            }
            if debug && !established_logged {
                established_logged = true;
                eprintln!(
                    "[rwt-driver] QUIC established, alpn={:?}",
                    conn.application_proto()
                );
            }
            session.on_established(&mut evs);
        }

        // Backpressure: if the JS thread is behind on processing events, stop
        // pulling high-volume data out of quiche this iteration. Unread stream
        // bytes stay flow-controlled in quiche and excess datagrams are dropped
        // by quiche's bounded recv queue, so the event queue cannot grow without
        // bound (hostile-flood OOM protection). Control-plane processing (the
        // CONNECT response, SETTINGS, session close) always runs.
        let backpressured = shared.inflight.load(Ordering::Relaxed) >= MAX_INFLIGHT_BATCHES;

        if !backpressured {
            session.on_datagrams(&mut conn, &mut evs);
        }

        let readable: Vec<u64> = conn.readable().collect();
        for id in readable {
            session.on_readable(&mut conn, id, backpressured, &mut evs);
        }

        session.flush(&mut conn, &mut evs);

        // 5. Send everything queued.
        flush_send(&mut conn, &socket, &mut send_buf, debug);

        // Publish synchronously-readable state.
        shared
            .max_datagram_size
            .store(session.max_datagram_size(&conn), Ordering::Relaxed);
        shared.ready.store(session.is_ready(), Ordering::Relaxed);

        // 6. Terminal state?
        if conn.is_closed() {
            if !session.is_closed() {
                let (code, reason, remote) = closure_info(&conn);
                session.mark_closed(&mut evs, code, reason, remote);
            }
            shared.closed.store(true, Ordering::Relaxed);
            if !evs.is_empty() {
                sink.emit(evs);
            }
            return;
        }

        if !evs.is_empty() {
            sink.emit(evs);
        }
    }
}

/// Drain quiche's send queue to the socket.
fn flush_send(conn: &mut quiche::Connection, socket: &UdpSocket, out: &mut [u8], debug: bool) {
    loop {
        match conn.send(out) {
            Ok((write, info)) => {
                // Ignore pacing (`info.at`); send immediately.
                let _ = info.at;
                if debug {
                    eprintln!("[rwt-driver] send {write} bytes to {}", info.to);
                }
                if let Err(e) = socket.send_to(&out[..write], info.to) {
                    if debug {
                        eprintln!("[rwt-driver] send_to err {e}");
                    }
                    if e.kind() == std::io::ErrorKind::WouldBlock {
                        // Socket buffer full; try again on the next wake.
                        return;
                    }
                }
            }
            Err(quiche::Error::Done) => return,
            Err(e) => {
                if debug {
                    eprintln!("[rwt-driver] send err {e:?}");
                }
                return;
            }
        }
    }
}

/// Extract a close code/reason from a terminated connection.
fn closure_info(conn: &quiche::Connection) -> (u32, Vec<u8>, bool) {
    if let Some(err) = conn.peer_error() {
        return (err.error_code as u32, err.reason.clone(), true);
    }
    if let Some(err) = conn.local_error() {
        return (err.error_code as u32, err.reason.clone(), false);
    }
    // Idle timeout / no explicit error.
    let _ = Instant::now();
    (0, Vec::new(), true)
}
