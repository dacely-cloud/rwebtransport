// SPDX-License-Identifier: Apache-2.0
//! The per-session driver thread: a mio event loop that owns the UDP socket and
//! the quiche connection, feeds packets to [`crate::session::WtSession`], and
//! ferries commands in and events out.

use std::collections::{HashSet, VecDeque};
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
///
/// `pub` so the neon sinks can wake the driver the moment a batch is dequeued and the counter crosses
/// back below this cap: back-pressure is set on the driver thread but cleared on the JS thread, so
/// without that wake the driver would keep withholding data (including a terminal frame) until an
/// unrelated event happened to run another loop iteration.
pub const MAX_INFLIGHT_BATCHES: i64 = 16;

/// Yield to commands and timers even if the UDP socket stays readable.
pub const MAX_RECV_PER_WAKE: usize = 1024;
/// Allow Initial packets larger than our advertised 1350-byte payload without
/// handing oversized datagrams to QUIC and its receive queues.
pub const MAX_RECV_UDP_PAYLOAD: usize = 2048;
const ADDRESS_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RESOLVED_ADDRESSES: usize = 16;

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
    /// Send a DRAIN_WEBTRANSPORT_SESSION capsule to the peer.
    Drain,
    /// Snapshot connection stats; the result arrives as a `stats` event.
    GetStats {
        request_id: u64,
    },
    /// Export TLS keying material (RFC 5705); the result arrives as a
    /// `keyingMaterial` event.
    ExportKeyingMaterial {
        request_id: u64,
        label: Vec<u8>,
        context: Vec<u8>,
        length: u32,
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
    /// WebTransport subprotocols the client offers (preference order).
    pub protocols: Vec<String>,
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

/// Resolve DNS, bind the UDP socket, build the QUIC config, perform the handshake
/// handshake, then run the event loop. Any setup failure is surfaced to JS.
fn setup_and_run(
    setup: SessionSetup,
    poll: Poll,
    rx: Receiver<Command>,
    sink: &dyn EventSink,
    shared: &Arc<DriverShared>,
) {
    let mut seen = HashSet::new();
    let peers: Vec<_> = (setup.host.as_str(), setup.port)
        .to_socket_addrs()
        .into_iter()
        .flatten()
        .filter(|address| seen.insert(*address))
        .take(MAX_RESOLVED_ADDRESSES)
        .collect();
    if peers.is_empty() {
        emit_fatal(
            sink,
            shared,
            format!("failed to resolve {}:{}", setup.host, setup.port),
        );
        return;
    }

    let mut config = match build_config(&setup.config) {
        Ok(c) => c,
        Err(e) => {
            emit_fatal(sink, shared, e);
            return;
        }
    };

    let mut poll = poll;
    let mut pending = VecDeque::new();
    let mut bind_error = String::new();
    for (index, peer) in peers.iter().copied().enumerate() {
        let (mut socket, local_addr, mut conn) =
            match dial_peer(peer, setup.sni.as_deref(), &mut config) {
                Ok(connection) => connection,
                Err(e) => {
                    bind_error = e;
                    continue;
                }
            };
        if index + 1 < peers.len() {
            match probe_address(
                &mut poll,
                &mut socket,
                local_addr,
                &mut conn,
                &rx,
                &mut pending,
            ) {
                Ok(Probe::Established) => {}
                Ok(Probe::NextAddress) => continue,
                Ok(Probe::Closed) => {
                    shared.closed.store(true, Ordering::Relaxed);
                    sink.emit(vec![Ev::Closed {
                        code: 0,
                        reason: Vec::new(),
                        remote: false,
                    }]);
                    return;
                }
                Err(e) => {
                    bind_error = format!("probing {peer}: {e}");
                    continue;
                }
            }
        }

        let session = WtSession::new_client(
            setup.authority,
            setup.path,
            setup.origin,
            setup.extra_headers,
            setup.protocols,
        );
        run_inner(
            poll, socket, local_addr, conn, session, rx, pending, sink, shared,
        );
        return;
    }

    emit_fatal(
        sink,
        shared,
        format!(
            "could not connect to any address for {}: {bind_error}",
            setup.host
        ),
    );
}

fn dial_peer(
    peer: SocketAddr,
    sni: Option<&str>,
    config: &mut quiche::Config,
) -> Result<(UdpSocket, SocketAddr, quiche::Connection), String> {
    let bind = if peer.is_ipv4() {
        SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, 0))
    } else {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, 0))
    };
    let socket = UdpSocket::bind(bind).map_err(|e| format!("binding for {peer}: {e}"))?;
    let local = socket
        .local_addr()
        .map_err(|e| format!("local address: {e}"))?;
    let mut scid = [0; 16];
    getrandom::fill(&mut scid).map_err(|e| format!("generating QUIC connection ID: {e}"))?;
    let scid = quiche::ConnectionId::from_ref(&scid);
    let conn = quiche::connect(sni, &scid, local, peer, config)
        .map_err(|e| format!("QUIC connect to {peer}: {e:?}"))?;
    Ok((socket, local, conn))
}

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Established,
    NextAddress,
    Closed,
}

fn probe_address(
    poll: &mut Poll,
    socket: &mut UdpSocket,
    local: SocketAddr,
    conn: &mut quiche::Connection,
    rx: &Receiver<Command>,
    pending: &mut VecDeque<Command>,
) -> std::io::Result<Probe> {
    poll.registry()
        .register(socket, SOCKET_TOKEN, mio::Interest::READABLE)?;
    let result = probe_registered(poll, socket, local, conn, rx, pending);
    poll.registry().deregister(socket)?;
    result
}

fn probe_registered(
    poll: &mut Poll,
    socket: &UdpSocket,
    local: SocketAddr,
    conn: &mut quiche::Connection,
    rx: &Receiver<Command>,
    pending: &mut VecDeque<Command>,
) -> std::io::Result<Probe> {
    let deadline = Instant::now() + ADDRESS_ATTEMPT_TIMEOUT;
    let mut events = Events::with_capacity(8);
    let mut packet = vec![0; 65_535];
    let mut out = [0; 1350];
    loop {
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, Command::Close { .. } | Command::Shutdown) {
                return Ok(Probe::Closed);
            }
            pending.push_back(cmd);
        }
        if conn.is_established() && conn.local_error().is_none() && !conn.is_draining() {
            return Ok(Probe::Established);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero()
            || conn.is_closed()
            || conn.local_error().is_some()
            || conn.is_draining()
        {
            return Ok(Probe::NextAddress);
        }
        conn.on_timeout();
        flush_send(conn, socket, &mut out, false);
        let mut drained = false;
        let mut received = false;
        for _ in 0..MAX_RECV_PER_WAKE {
            match socket.recv_from(&mut packet) {
                Ok((len, from)) if len != 0 && len <= MAX_RECV_UDP_PAYLOAD => {
                    let _ = conn.recv(&mut packet[..len], quiche::RecvInfo { from, to: local });
                    received = true;
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    drained = true;
                    break;
                }
                Err(e) if is_udp_port_unreachable(&e) => return Ok(Probe::NextAddress),
                Err(e) => return Err(e),
            }
        }
        if received || !drained {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let timeout = conn
            .timeout()
            .map_or(remaining, |timeout| timeout.min(remaining));
        if let Err(e) = poll.poll(&mut events, Some(timeout)) {
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// The actual event loop. Runs until the connection closes or a fatal error
/// occurs. Never call this directly from the spawned thread; go through
/// [`run`], which contains the panic boundary.
#[allow(clippy::too_many_arguments)]
fn run_inner(
    mut poll: Poll,
    mut socket: UdpSocket,
    local_addr: std::net::SocketAddr,
    mut conn: quiche::Connection,
    mut session: WtSession,
    rx: Receiver<Command>,
    mut pending: VecDeque<Command>,
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

    let mut udp_pending = conn.is_established() || !pending.is_empty();
    let mut read_pending = false;
    loop {
        let can_read = shared.inflight.load(Ordering::Relaxed) < MAX_INFLIGHT_BATCHES;
        let timeout = if udp_pending || (read_pending && can_read) {
            Some(std::time::Duration::ZERO)
        } else {
            conn.timeout()
        };
        if let Err(e) = poll.poll(&mut events, timeout) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            sink.emit(vec![Ev::Error(format!("poll error: {e}"))]);
            return;
        }

        let mut evs: Vec<Ev> = Vec::new();
        let mut shutdown = false;

        // mio may use edge-triggered readiness. If we stop before WouldBlock,
        // poll again without waiting for a new readiness edge.
        let mut udp_budget = MAX_RECV_PER_WAKE;
        while udp_budget > 0 {
            udp_budget -= 1;
            match socket.recv_from(&mut recv_buf) {
                Ok((len, from)) => {
                    // UDP permits empty datagrams. They carry no QUIC packet
                    // and must not become fatal errors before authentication.
                    if len == 0 || len > MAX_RECV_UDP_PAYLOAD {
                        continue;
                    }
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
                            // quiche records fatal transport errors on the
                            // connection. Invalid UDP input is otherwise dropped.
                        }
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(ref e) if is_udp_port_unreachable(e) => {
                    if debug {
                        eprintln!("[rwt-driver] UDP port unreachable: {e}");
                    }
                    continue;
                }
                Err(e) => {
                    evs.push(Ev::Error(format!("socket recv: {e}")));
                    break;
                }
            }
        }
        udp_pending = udp_budget == 0;

        // quiche checks whether a timer is due.
        conn.on_timeout();

        // 3. Drain commands.
        while let Some(cmd) = pending.pop_front().or_else(|| rx.try_recv().ok()) {
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
                Command::Drain => session.send_drain(),
                Command::GetStats { request_id } => {
                    evs.push(crate::session::build_stats(&conn, request_id))
                }
                Command::ExportKeyingMaterial {
                    request_id,
                    label,
                    context,
                    length,
                } => evs.push(
                    session.build_keying_material(&mut conn, request_id, &label, &context, length),
                ),
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
            session.on_established(conn.reliable_stream_reset_negotiated());
        }

        // Backpressure: if the JS thread is behind on processing events, stop
        // pulling high-volume data out of quiche this iteration. Unread stream
        // bytes stay flow-controlled in quiche and excess datagrams are dropped
        // by quiche's bounded recv queue, so the event queue cannot grow without
        // bound. QUIC packet processing and closure continue under pressure.
        if shared.inflight.load(Ordering::Relaxed) < MAX_INFLIGHT_BATCHES {
            let mut budget = crate::session::ReceiveBudget::new();
            session.read_pending(&mut conn, &mut budget, &mut evs);
            read_pending = budget.exhausted();
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
                if let Some((code, reason)) = session.local_close_info() {
                    // We initiated a graceful WebTransport close: surface the WT
                    // code/reason (the wire close used H3_NO_ERROR), a clean local
                    // close, not an error.
                    session.mark_closed(&mut evs, code, reason, false);
                } else {
                    let (code, reason, remote, err) = closure_info(&conn);
                    if let Some(msg) = err {
                        evs.push(Ev::Error(msg));
                    }
                    session.mark_closed(&mut evs, code, reason, remote);
                }
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

/// Winsock may report an ICMP Port Unreachable for an earlier UDP send. Let QUIC
/// determine connection liveness after loss rather than retiring the session from recv_from.
pub(crate) fn is_udp_port_unreachable(error: &std::io::Error) -> bool {
    const WSAECONNRESET: i32 = 10054;
    cfg!(windows) && error.raw_os_error() == Some(WSAECONNRESET)
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

/// Extract a close code/reason from a terminated connection. The fourth element
/// is `Some(message)` when the termination was an abnormal transport-level event
/// (idle timeout, a transport CONNECTION_CLOSE with `is_app == false`, or a
/// stateless reset) rather than a clean WebTransport/HTTP-3 close; the driver
/// emits an `Ev::Error` for it so the JS `closed` promise rejects. A clean
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
    // No explicit error and not a timeout (for example a stateless reset): still
    // an abnormal loss of the connection.
    (
        0,
        Vec::new(),
        true,
        Some("connection closed abnormally".to_string()),
    )
}

#[cfg(test)]
mod receive_error_tests {
    use super::is_udp_port_unreachable;
    use std::io::Error;

    #[test]
    fn the_winsock_udp_notification_keeps_the_quic_session_alive() {
        assert_eq!(
            is_udp_port_unreachable(&Error::from_raw_os_error(10054)),
            cfg!(windows)
        );
        assert!(!is_udp_port_unreachable(&Error::from_raw_os_error(10053)));
        assert!(!is_udp_port_unreachable(&Error::from_raw_os_error(10038)));
    }
}
