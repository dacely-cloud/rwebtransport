// SPDX-License-Identifier: Apache-2.0
//! WebTransport client session state machine over a single quiche connection.
//!
//! One `WtSession` drives exactly one WebTransport session (one HTTP/3 Extended
//! CONNECT) over one QUIC connection. It owns no I/O: the [`crate::driver`]
//! event loop feeds it readable-stream / datagram / timer notifications and asks
//! it to flush. Everything the session wants to tell JS is returned as [`Ev`]s.
//!
//! Stream-id layout (client-initiated):
//! * bidi `0` is the CONNECT stream (this is also the WebTransport session id).
//! * uni `2`, `6`, `10` are HTTP/3 control, QPACK encoder, QPACK decoder.
//! * bidi `4, 8, ...` are WebTransport bidirectional streams.
//! * uni `14, 18, ...` are WebTransport unidirectional streams.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use crate::parser_memory::{ParserBuffer, ParserMemory};

use wtcore::h3;

fn dbg_on() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("RWT_DEBUG").is_ok())
}

/// The session id / CONNECT stream id (always the client-initiated bidi 0).
const CONNECT_ID: u64 = 0;

// Client-role local stream ids (client-initiated).
const CLIENT_CONTROL_ID: u64 = 2;
const CLIENT_QPACK_ENC_ID: u64 = 6;
const CLIENT_QPACK_DEC_ID: u64 = 10;
const CLIENT_FIRST_WT_BIDI_ID: u64 = 4;
const CLIENT_FIRST_WT_UNI_ID: u64 = 14;

// Server-role local stream ids (server-initiated).
const SERVER_CONTROL_ID: u64 = 3;
const SERVER_QPACK_ENC_ID: u64 = 7;
const SERVER_QPACK_DEC_ID: u64 = 11;
const SERVER_FIRST_WT_BIDI_ID: u64 = 1;
const SERVER_FIRST_WT_UNI_ID: u64 = 15;

/// Cap on a single buffered HTTP/3 control/CONNECT frame (protects against a
/// hostile peer advertising a huge length).
const MAX_CONTROL_FRAME: usize = 1 << 20;

/// Bound all parser allocations on a connection as well as the shared server
/// total, so opening many control streams cannot consume the entire budget.
const MAX_SESSION_PARSER_MEMORY: usize = 8 * 1024 * 1024;

/// Per-session flow-control credit we grant the peer once established. Large,
/// because QUIC's connection flow control is the real limit (one session per
/// connection); this only keeps a strict peer from stalling on our advertised
/// per-session limits while QUIC does the actual enforcement.
const WT_GRANT_MAX_DATA: u64 = 1 << 34;
const WT_GRANT_MAX_STREAMS: u64 = 1 << 20;

/// Events the session emits for the driver to forward to JS.
#[derive(Debug)]
pub enum Ev {
    /// The WebTransport session is established (client role: CONNECT got a 2xx).
    /// Carries the negotiated subprotocol (if any) and the response headers.
    Ready {
        protocol: Option<String>,
        headers: Vec<(String, String)>,
    },
    /// The peer sent DRAIN_WEBTRANSPORT_SESSION: it intends to close soon, but
    /// the session and its streams stay usable until an actual close.
    Draining,
    /// A server-role session was established: a valid Extended CONNECT arrived
    /// and we answered 200. Carries the request details for the application.
    ServerReady {
        authority: String,
        path: String,
        origin: Option<String>,
        headers: Vec<(String, String)>,
        /// The subprotocols the client offered (in `wt-available-protocols`).
        requested_protocols: Vec<String>,
        /// The subprotocol the server selected (echoed in `wt-protocol`), if any.
        protocol: Option<String>,
        /// The client's remote IP (as a string) at session establishment.
        remote_addr: String,
        /// The client's remote UDP port at session establishment.
        remote_port: u16,
    },
    /// The session ended. `remote` is true if the peer initiated it.
    Closed {
        code: u32,
        reason: Vec<u8>,
        remote: bool,
    },
    /// A fatal error before or during the session.
    Error(String),
    /// A WebTransport datagram was received.
    Datagram(Vec<u8>),
    /// The peer opened a WebTransport stream.
    IncomingStream { id: u64, bidi: bool },
    /// Application data arrived on a WebTransport stream.
    StreamData { id: u64, data: Vec<u8> },
    /// The peer finished (FIN) its side of a stream.
    StreamFinished { id: u64 },
    /// The peer reset a stream.
    StreamReset { id: u64, code: u64 },
    /// The peer sent STOP_SENDING for a stream we write to.
    StreamStopSending { id: u64, code: u64 },
    /// A locally requested stream open completed; `id` is the QUIC stream id.
    StreamOpened { request_id: u64, id: u64 },
    /// A write's bytes were fully handed to quiche (backpressure signal).
    WriteAck { request_id: u64 },
    /// A datagram send request was processed (sent or intentionally dropped).
    DatagramAck { request_id: u64, sent: bool },
    /// Result of a getStats() request.
    Stats {
        request_id: u64,
        bytes_sent: u64,
        bytes_received: u64,
        packets_sent: u64,
        packets_received: u64,
        packets_lost: u64,
        smoothed_rtt_ms: f64,
        rtt_variation_ms: f64,
        min_rtt_ms: f64,
    },
    /// Result of an exportKeyingMaterial() request: the exported bytes, or
    /// `None` if the TLS export failed (for example, before the handshake).
    KeyingMaterial {
        request_id: u64,
        data: Option<Vec<u8>>,
    },
}

/// Snapshot a quiche connection's stats into a [`Ev::Stats`] for `request_id`.
pub fn build_stats(conn: &quiche::Connection, request_id: u64) -> Ev {
    let s = conn.stats();
    let to_ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let (rtt, min_rtt, rttvar) = match conn.path_stats().next() {
        Some(p) => (p.rtt, p.min_rtt.unwrap_or(p.rtt), p.rttvar),
        None => (
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        ),
    };
    Ev::Stats {
        request_id,
        bytes_sent: s.sent_bytes,
        bytes_received: s.recv_bytes,
        packets_sent: s.sent as u64,
        packets_received: s.recv as u64,
        packets_lost: s.lost as u64,
        smoothed_rtt_ms: to_ms(rtt),
        rtt_variation_ms: to_ms(rttvar),
        min_rtt_ms: to_ms(min_rtt),
    }
}

/// Export `length` bytes of TLS keying material (RFC 5705) into a
/// [`Ev::KeyingMaterial`] for `request_id`. `data` is `None` if the export
/// failed. WebTransport always supplies a context, so it is passed through even
/// when empty (an empty context differs from no context per RFC 5705).
pub fn build_keying_material(
    conn: &mut quiche::Connection,
    request_id: u64,
    label: &[u8],
    context: &[u8],
    length: u32,
) -> Ev {
    if !(1..=4096).contains(&length) {
        return Ev::KeyingMaterial {
            request_id,
            data: None,
        };
    }
    let mut out = vec![0u8; length as usize];
    let data = match conn.export_keying_material(&mut out, label, Some(context)) {
        Ok(()) => Some(out),
        Err(_) => None,
    };
    Ev::KeyingMaterial { request_id, data }
}

/// One queued outbound chunk on a stream.
struct OutChunk {
    data: Vec<u8>,
    off: usize,
    request_id: Option<u64>,
}

/// Per-stream classification / role.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Our send-only HTTP/3 control / QPACK uni streams. Never read.
    LocalControlPlane,
    /// The CONNECT (session) bidi stream.
    Connect,
    /// The peer's HTTP/3 control stream: parse SETTINGS/frames.
    PeerControl,
    /// A peer uni stream we drain and discard (QPACK/push/unknown).
    Ignored,
    PeerQpack {
        encoder: bool,
    },
    /// A resolved WebTransport data stream.
    WtData {
        bidi: bool,
    },
    /// A peer-initiated uni stream whose type varint isn't fully read yet.
    PendingUni,
    /// A peer-initiated bidi stream whose WT signal isn't fully read yet.
    PendingBidi,
}

struct Stream {
    role: Role,
    /// Classification accumulator (pending server streams) / control byte carry.
    class_buf: ParserBuffer,
    /// HTTP/3 frame parser (Connect + PeerControl).
    frames: FrameParser,
    /// CONNECT stream: response HEADERS already parsed.
    headers_done: bool,
    /// Read backpressure (WtData only).
    paused: bool,
    /// Outbound queue.
    out: VecDeque<OutChunk>,
    /// Application asked to FIN once `out` drains.
    fin_queued: bool,
    fin_sent: bool,
    /// Peer already reset/finished, stop touching recv.
    recv_dead: bool,
    recv_payload: u64,
    recv_prefix: u64,
    send_payload: u64,
    send_prefix: u64,
}

impl Stream {
    fn new(role: Role, memory: Arc<ParserMemory>) -> Self {
        Self {
            role,
            class_buf: ParserBuffer::new(memory.clone()),
            frames: FrameParser::new(memory),
            headers_done: false,
            paused: false,
            out: VecDeque::new(),
            fin_queued: false,
            fin_sent: false,
            recv_dead: false,
            recv_payload: 0,
            recv_prefix: 0,
            send_payload: 0,
            send_prefix: 0,
        }
    }

    fn with_prefix(role: Role, prefix: Vec<u8>, memory: Arc<ParserMemory>) -> Self {
        let mut s = Self::new(role, memory);
        if matches!(role, Role::WtData { .. }) {
            s.send_prefix = prefix.len() as u64;
        }
        if !prefix.is_empty() {
            s.out.push_back(OutChunk {
                data: prefix,
                off: 0,
                request_id: None,
            });
        }
        s
    }
}

/// Incremental HTTP/3 frame reassembler.
struct FrameParser {
    buf: ParserBuffer,
    error: bool,
}

impl FrameParser {
    fn new(memory: Arc<ParserMemory>) -> Self {
        Self {
            buf: ParserBuffer::new(memory),
            error: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if self.error {
            return;
        }
        if self.buf.extend_from_slice(data).is_err() {
            self.error = true;
        }
    }

    /// Pop the next complete `(type, payload)` frame, or `None` if incomplete.
    fn next_frame(&mut self) -> Option<(u64, ParserBuffer)> {
        if self.error {
            return None;
        }
        let (ty, n1) = h3::read_varint(&self.buf)?;
        let (len, n2) = h3::read_varint(&self.buf[n1..])?;
        if len > MAX_CONTROL_FRAME as u64 {
            self.error = true;
            return None;
        }
        let len = len as usize;
        let header = n1 + n2;
        let total = header + len;
        if self.buf.len() < total {
            return None;
        }
        let payload = match self.buf.copy_range(header..total) {
            Ok(payload) => payload,
            Err(()) => {
                self.error = true;
                return None;
            }
        };
        self.buf.consume(total);
        Some((ty, payload))
    }
}

/// Deferred graceful close.
struct CloseReq {
    code: u32,
    reason: Vec<u8>,
    capsule_queued: bool,
}

pub struct WtSession {
    is_server: bool,

    // Client role only: the Extended CONNECT request we send.
    authority: String,
    path: String,
    origin: Option<String>,
    extra_headers: Vec<(String, String)>,
    /// Client role: WebTransport subprotocols we offer (preference order).
    protocols: Vec<String>,
    /// Server role: subprotocols we support (preference order); the first that
    /// the client also offers is selected. Empty means no negotiation.
    supported_protocols: Vec<String>,
    /// Server role: if `Some`, only Extended CONNECTs whose `origin` is in this
    /// list are accepted (others are rejected with 403). `None` allows any.
    allowed_origins: Option<Vec<String>>,
    /// Server role: extra headers to add to the 200 CONNECT response.
    response_headers: Vec<(String, String)>,

    next_bidi: u64,
    next_uni: u64,
    pending_opens: VecDeque<(bool, u64)>,

    setup_sent: bool,
    ready: bool,
    closed: bool,
    /// Client role only: the Extended CONNECT is built but deferred until the
    /// server's SETTINGS confirm WebTransport support (see parse_control_frames).
    connect_pending: bool,
    /// Set once a DRAIN_WEBTRANSPORT_SESSION capsule has been surfaced, so the
    /// `Draining` event is emitted at most once.
    drain_emitted: bool,
    /// Set once we have queued an outbound DRAIN capsule, so we send at most one.
    drain_sent: bool,
    /// Set once we granted the peer per-session flow-control credit.
    fc_credit_granted: bool,
    /// Peer's send-side session limits (from SETTINGS_WT_INITIAL_* and WT_MAX_*
    /// capsules), enforced in addition to QUIC's transport-level limits.
    fc_max_data_remote: u64,
    fc_max_streams_bidi_remote: u64,
    fc_max_streams_uni_remote: u64,
    /// Cumulative WebTransport stream payload bytes handed to quiche, compared
    /// against `fc_max_data_remote` to detect a WT_DATA_BLOCKED condition.
    fc_data_sent: u64,
    fc_data_received: u64,
    fc_max_data_local: u64,
    fc_max_streams_local: [u64; 2],
    fc_streams_received: [u64; 2],
    terminal_error: Option<(u64, Vec<u8>)>,
    /// The WT_MAX_DATA / WT_MAX_STREAMS_* limit value we last announced as blocked
    /// at (None = no signal yet), so a BLOCKED capsule is re-sent only when the limit changes.
    data_blocked_at: Option<u64>,
    streams_bidi_blocked_at: Option<u64>,
    streams_uni_blocked_at: Option<u64>,
    /// Reassembly buffer for capsules on the session (CONNECT) stream: RFC 9297
    /// lets a capsule span multiple HTTP/3 DATA frames, so bytes are accumulated
    /// here and only whole capsules are consumed.
    capsule_buf: ParserBuffer,
    parser_memory: Arc<ParserMemory>,

    peer_settings: Option<h3::PeerSettings>,

    streams: HashMap<u64, Stream>,

    close_req: Option<CloseReq>,
    /// Set once the QUIC connection should be closed (after connect FIN flush).
    want_quic_close: Option<(u32, Vec<u8>)>,
    /// True once the QUIC close has been deferred one flush pass, so the queued
    /// CLOSE_WEBTRANSPORT_SESSION capsule + FIN are serialized to the wire before
    /// the CONNECTION_CLOSE frame supersedes all stream frames.
    close_deferred_once: bool,
    /// True once we have issued the graceful QUIC close, so it happens once.
    quic_close_issued: bool,
    /// Server role only: the client's remote address, captured when the
    /// connection was accepted and surfaced on the `serverReady` event.
    remote_addr: Option<std::net::SocketAddr>,
}

impl WtSession {
    /// A client-role session that will send an Extended CONNECT.
    pub fn new_client(
        authority: String,
        path: String,
        origin: Option<String>,
        extra_headers: Vec<(String, String)>,
        protocols: Vec<String>,
    ) -> Self {
        let parser_memory = ParserMemory::new(MAX_SESSION_PARSER_MEMORY);
        Self {
            is_server: false,
            authority,
            path,
            origin,
            extra_headers,
            protocols,
            supported_protocols: Vec::new(),
            allowed_origins: None,
            response_headers: Vec::new(),
            next_bidi: CLIENT_FIRST_WT_BIDI_ID,
            next_uni: CLIENT_FIRST_WT_UNI_ID,
            pending_opens: VecDeque::new(),
            setup_sent: false,
            ready: false,
            closed: false,
            connect_pending: false,
            drain_emitted: false,
            drain_sent: false,
            fc_credit_granted: false,
            fc_max_data_remote: 0,
            fc_max_streams_bidi_remote: 0,
            fc_max_streams_uni_remote: 0,
            fc_data_sent: 0,
            fc_data_received: 0,
            fc_max_data_local: h3::WT_INITIAL_MAX_DATA,
            fc_max_streams_local: [h3::WT_INITIAL_MAX_STREAMS; 2],
            fc_streams_received: [0; 2],
            terminal_error: None,
            data_blocked_at: None,
            streams_bidi_blocked_at: None,
            streams_uni_blocked_at: None,
            capsule_buf: ParserBuffer::new(parser_memory.clone()),
            parser_memory,
            peer_settings: None,
            streams: HashMap::new(),
            close_req: None,
            want_quic_close: None,
            close_deferred_once: false,
            quic_close_issued: false,
            remote_addr: None,
        }
    }

    /// A server-role session that will accept an incoming Extended CONNECT.
    /// `remote` is the client's address, surfaced to the application via the
    /// `serverReady` event.
    pub fn new_server(
        remote: std::net::SocketAddr,
        supported_protocols: Vec<String>,
        allowed_origins: Option<Vec<String>>,
        response_headers: Vec<(String, String)>,
        parser_memory: Arc<ParserMemory>,
    ) -> Self {
        let parser_memory = ParserMemory::child(parser_memory, MAX_SESSION_PARSER_MEMORY);
        Self {
            is_server: true,
            authority: String::new(),
            path: String::new(),
            origin: None,
            extra_headers: Vec::new(),
            protocols: Vec::new(),
            supported_protocols,
            allowed_origins,
            response_headers,
            next_bidi: SERVER_FIRST_WT_BIDI_ID,
            next_uni: SERVER_FIRST_WT_UNI_ID,
            pending_opens: VecDeque::new(),
            setup_sent: false,
            ready: false,
            closed: false,
            connect_pending: false,
            drain_emitted: false,
            drain_sent: false,
            fc_credit_granted: false,
            fc_max_data_remote: 0,
            fc_max_streams_bidi_remote: 0,
            fc_max_streams_uni_remote: 0,
            fc_data_sent: 0,
            fc_data_received: 0,
            fc_max_data_local: h3::WT_INITIAL_MAX_DATA,
            fc_max_streams_local: [h3::WT_INITIAL_MAX_STREAMS; 2],
            fc_streams_received: [0; 2],
            terminal_error: None,
            data_blocked_at: None,
            streams_bidi_blocked_at: None,
            streams_uni_blocked_at: None,
            capsule_buf: ParserBuffer::new(parser_memory.clone()),
            parser_memory,
            peer_settings: None,
            streams: HashMap::new(),
            close_req: None,
            want_quic_close: None,
            close_deferred_once: false,
            quic_close_issued: false,
            remote_addr: Some(remote),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Called once the QUIC connection is established: open the HTTP/3
    /// control-plane streams and advertise SETTINGS. A client waits for the
    /// server's SETTINGS before sending CONNECT; a server waits for the client's
    /// CONNECT on bidi stream 0.
    pub fn on_established(&mut self) {
        if self.setup_sent {
            return;
        }
        self.setup_sent = true;

        let (control_id, qpack_enc_id, qpack_dec_id) = if self.is_server {
            (SERVER_CONTROL_ID, SERVER_QPACK_ENC_ID, SERVER_QPACK_DEC_ID)
        } else {
            (CLIENT_CONTROL_ID, CLIENT_QPACK_ENC_ID, CLIENT_QPACK_DEC_ID)
        };

        // HTTP/3 control stream: type prefix + SETTINGS.
        self.streams.insert(
            control_id,
            Stream::with_prefix(
                Role::LocalControlPlane,
                h3::control_stream_prefix(),
                self.parser_memory.clone(),
            ),
        );
        // QPACK encoder / decoder streams (dynamic table disabled → type only).
        let mut enc = Vec::new();
        h3::put_varint(h3::QPACK_ENCODER_STREAM_TYPE, &mut enc);
        self.streams.insert(
            qpack_enc_id,
            Stream::with_prefix(Role::LocalControlPlane, enc, self.parser_memory.clone()),
        );
        let mut dec = Vec::new();
        h3::put_varint(h3::QPACK_DECODER_STREAM_TYPE, &mut dec);
        self.streams.insert(
            qpack_dec_id,
            Stream::with_prefix(Role::LocalControlPlane, dec, self.parser_memory.clone()),
        );

        if !self.is_server {
            // Per draft-ietf-webtrans-http3 §3.1 the client MUST NOT send the
            // Extended CONNECT until it has received the server's SETTINGS and
            // confirmed WebTransport support. Defer it to parse_control_frames.
            self.connect_pending = true;
        }
        if dbg_on() {
            eprintln!(
                "[rwt-session] on_established (server={}): queued setup, {} streams",
                self.is_server,
                self.streams.len()
            );
        }
    }

    /// Process a readable QUIC stream. `backpressured` is a global signal that
    /// the JS thread is behind; when set, resolved WebTransport data streams are
    /// left unread (flow-controlling the peer) while control-plane streams still
    /// drain.
    pub fn on_readable(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        backpressured: bool,
        ev: &mut Vec<Ev>,
    ) {
        if self.closed {
            return;
        }
        let is_server = self.is_server;
        self.streams.entry(id).or_insert_with(|| {
            Stream::new(classify_new(id, is_server), self.parser_memory.clone())
        });

        // Respect per-stream and global read backpressure for resolved data
        // streams (leave the bytes in quiche so the peer is flow-controlled).
        let (paused, is_data, recv_dead) = {
            let st = &self.streams[&id];
            (
                st.paused,
                matches!(st.role, Role::WtData { .. }),
                st.recv_dead,
            )
        };
        if recv_dead {
            return;
        }
        if (paused || backpressured) && is_data {
            return;
        }

        // Pending bidi streams are application data. Under global pressure,
        // classify uni stream types one byte at a time, then leave WT headers
        // and payload in QUIC while allowing critical streams to drain.
        if backpressured && matches!(self.streams[&id].role, Role::PendingBidi) {
            return;
        }

        // Drain everything quiche currently has for this stream.
        let mut reset: Option<u64> = None;
        let mut buf = [0u8; 16 * 1024];
        loop {
            let st = &self.streams[&id];
            if backpressured
                && (matches!(st.role, Role::WtData { .. })
                    || (matches!(st.role, Role::PendingUni)
                        && h3::read_varint(&st.class_buf)
                            .is_some_and(|(ty, _)| ty == h3::WT_UNI_STREAM_TYPE)))
            {
                return;
            }
            let read_len = if backpressured && matches!(st.role, Role::PendingUni) {
                1
            } else {
                buf.len()
            };
            let final_size = conn.stream_recv_final_size(id);
            match conn.stream_recv(id, &mut buf[..read_len]) {
                Ok((n, f)) => {
                    // Parse bounded chunks immediately. Do not build an entire
                    // stream-sized scratch allocation outside the parser budget.
                    self.process_recv(conn, id, buf[..n].to_vec(), f, ev);
                    if self.closed {
                        return;
                    }
                    if f {
                        break;
                    }
                    if n == 0 {
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(quiche::Error::StreamReset(code)) => {
                    if let Some(size) = final_size {
                        let st = &self.streams[&id];
                        if matches!(st.role, Role::WtData { .. }) {
                            let missing = size
                                .saturating_sub(st.recv_prefix)
                                .saturating_sub(st.recv_payload);
                            if !self.account_receive(id, missing, ev) {
                                return;
                            }
                        }
                    }
                    reset = Some(code);
                    break;
                }
                Err(_) => break,
            }
        }

        if let Some(code) = reset {
            let role = self.streams.get(&id).map(|s| s.role);
            if let Some(st) = self.streams.get_mut(&id) {
                st.recv_dead = true;
            }
            match role {
                Some(Role::WtData { .. }) => {
                    // Map the HTTP/3 reset code back to the WebTransport
                    // application code; pass unmapped codes through unchanged.
                    let app = h3::http_code_to_webtransport_code(code).unwrap_or(code);
                    ev.push(Ev::StreamReset { id, code: app })
                }
                Some(Role::PeerControl | Role::PeerQpack { .. }) => {
                    self.protocol_error(conn, ev, 0x104, "peer reset a critical HTTP/3 stream");
                }
                Some(Role::Connect) if !self.closed => {
                    // A reset of the session (CONNECT) stream ends the session;
                    // if we swallowed it, ready/closed would hang forever.
                    if !self.ready {
                        ev.push(Ev::Error(format!(
                            "server reset the WebTransport session stream (code {code})"
                        )));
                    }
                    self.mark_closed(ev, code as u32, Vec::new(), true);
                }
                _ => {}
            }
            return;
        }
    }

    fn process_recv(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        chunk: Vec<u8>,
        fin: bool,
        ev: &mut Vec<Ev>,
    ) {
        let role = self.streams.get(&id).map(|s| s.role);
        if dbg_on() {
            eprintln!(
                "[rwt-session] readable id={id} role={} chunk={} fin={fin}",
                role.map(role_name).unwrap_or("?"),
                chunk.len()
            );
        }
        match role {
            Some(Role::LocalControlPlane) | None => {
                // Our send-only control-plane streams / unknown: discard reads.
            }
            Some(Role::PeerQpack { .. }) => {
                if fin {
                    self.protocol_error(conn, ev, 0x104, "peer closed a critical QPACK stream");
                }
            }
            Some(Role::Ignored) => {
                // A drained peer stream (QPACK/push/unknown): mark it finished so
                // it can be pruned, otherwise stream churn grows the map.
                if fin {
                    if let Some(st) = self.streams.get_mut(&id) {
                        st.recv_dead = true;
                    }
                }
            }
            Some(Role::PeerControl) => {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.frames.push(&chunk);
                }
                self.parse_control_frames(conn, id, ev);
                if fin && !self.closed {
                    self.protocol_error(conn, ev, 0x104, "peer closed the HTTP/3 control stream");
                }
                // A malformed control stream is a fatal H3 error; stop reading it
                // so a hostile server cannot keep us draining (and re-crediting)
                // an oversized frame into an unbounded buffer.
                if self.frame_error(id) {
                    self.kill_recv(conn, id);
                    if !self.closed {
                        ev.push(Ev::Error("malformed HTTP/3 control stream".to_string()));
                        self.mark_closed(ev, 0, Vec::new(), true);
                        let _ = conn.close(true, 0, b"h3 control error");
                    }
                }
            }
            Some(Role::Connect) => {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.frames.push(&chunk);
                }
                if self.is_server {
                    self.parse_server_connect_frames(conn, id, ev);
                } else {
                    self.parse_connect_frames(conn, id, ev);
                }
                if self.frame_error(id) && !self.closed {
                    self.kill_recv(conn, id);
                    ev.push(Ev::Error("malformed HTTP/3 CONNECT stream".to_string()));
                    self.mark_closed(ev, 0, Vec::new(), true);
                    let _ = conn.close(true, 0, b"h3 connect error");
                }
                if fin && !self.closed {
                    if self
                        .streams
                        .get(&id)
                        .is_some_and(|st| !st.frames.buf.is_empty())
                        || !self.capsule_buf.is_empty()
                    {
                        self.protocol_error(conn, ev, 0x106, "truncated CONNECT frame or capsule");
                    } else {
                        self.mark_closed(ev, 0, Vec::new(), true);
                    }
                }
            }
            Some(Role::WtData { .. }) => {
                if !self.account_receive(id, chunk.len() as u64, ev) {
                    return;
                }
                if !chunk.is_empty() {
                    ev.push(Ev::StreamData { id, data: chunk });
                }
                if fin {
                    if let Some(st) = self.streams.get_mut(&id) {
                        st.recv_dead = true;
                    }
                    ev.push(Ev::StreamFinished { id });
                }
            }
            Some(Role::PendingUni) => {
                self.resolve_pending_uni(conn, id, chunk, fin, ev);
            }
            Some(Role::PendingBidi) => {
                self.resolve_pending_bidi(conn, id, chunk, fin, ev);
            }
        }
    }

    fn parse_control_frames(&mut self, conn: &mut quiche::Connection, id: u64, ev: &mut Vec<Ev>) {
        while !self.closed {
            let frame = self
                .streams
                .get_mut(&id)
                .and_then(|s| s.frames.next_frame());
            let Some((ty, payload)) = frame else { break };
            let first = !self.streams[&id].headers_done;
            if first && ty != h3::FRAME_SETTINGS {
                self.protocol_error(conn, ev, 0x10a, "SETTINGS must be the first control frame");
                return;
            }
            if ty == h3::FRAME_SETTINGS {
                if !first {
                    self.protocol_error(conn, ev, 0x105, "repeated SETTINGS frame");
                    return;
                }
                self.streams.get_mut(&id).unwrap().headers_done = true;
                let settings = match h3::parse_settings(&payload) {
                    Ok(settings) => settings,
                    Err(message) => {
                        self.protocol_error(conn, ev, 0x109, &message);
                        return;
                    }
                };
                if dbg_on() {
                    eprintln!("[rwt-session] peer settings={settings:?}");
                }
                self.peer_settings = Some(settings);
                // Seed the peer's per-session flow-control limits from its SETTINGS.
                self.fc_max_data_remote = settings.wt_initial_max_data;
                self.fc_max_streams_bidi_remote = settings.wt_initial_max_streams_bidi;
                self.fc_max_streams_uni_remote = settings.wt_initial_max_streams_uni;
                // The client deferred its Extended CONNECT until it saw the
                // server's SETTINGS: now send it if WebTransport is supported,
                // otherwise reject the session.
                if self.connect_pending {
                    self.connect_pending = false;
                    if settings.webtransport_ok() {
                        self.queue_connect(ev);
                    } else {
                        ev.push(Ev::Error(
                            "server does not advertise WebTransport support".to_string(),
                        ));
                        self.mark_closed(ev, 0, b"webtransport not supported".to_vec(), false);
                    }
                }
                if self.is_server && self.streams.contains_key(&CONNECT_ID) {
                    self.parse_server_connect_frames(conn, CONNECT_ID, ev);
                }
            } else if matches!(
                ty,
                h3::FRAME_DATA | h3::FRAME_HEADERS | 0x2 | 0x6 | 0x8 | 0x9
            ) {
                self.protocol_error(conn, ev, 0x105, "unexpected frame on control stream");
                return;
            } else if ty == h3::FRAME_GOAWAY && !self.drain_emitted && !self.closed {
                // The peer is gracefully shutting down the HTTP/3 connection.
                // Exactly one WebTransport session rides it, so surface GOAWAY as
                // the session draining (going away soon, still usable).
                self.drain_emitted = true;
                ev.push(Ev::Draining);
            }
        }
    }

    /// Build and queue the client's Extended CONNECT on the session (bidi 0)
    /// stream. Called once the server's SETTINGS have confirmed WebTransport.
    fn queue_connect(&mut self, ev: &mut Vec<Ev>) {
        let headers = h3::connect_headers(
            &self.authority,
            &self.path,
            self.origin.as_deref(),
            &self.extra_headers,
            &self.protocols,
        );
        match h3::encode_headers_frame(&headers) {
            Ok(frame) => {
                self.streams.insert(
                    CONNECT_ID,
                    Stream::with_prefix(Role::Connect, frame, self.parser_memory.clone()),
                );
            }
            Err(e) => ev.push(Ev::Error(format!("failed to encode CONNECT: {e}"))),
        }
    }

    fn parse_connect_frames(&mut self, conn: &mut quiche::Connection, id: u64, ev: &mut Vec<Ev>) {
        while !self.closed {
            let (frame, headers_done) = {
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                (st.frames.next_frame(), st.headers_done)
            };
            let Some((ty, payload)) = frame else { break };

            if dbg_on() {
                eprintln!("[rwt-session] connect frame ty={ty} len={}", payload.len());
            }
            if ty == h3::FRAME_HEADERS && !headers_done {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.headers_done = true;
                }
                match h3::decode_header_block(&payload) {
                    Ok(headers) => {
                        let status = h3::status_of(&headers).unwrap_or(0);
                        if dbg_on() {
                            eprintln!("[rwt-session] CONNECT response status={status}");
                        }
                        if (200..300).contains(&status) {
                            if !self.ready {
                                let protocol = h3::wt_protocol_of(&headers);
                                if protocol
                                    .as_ref()
                                    .is_some_and(|p| !self.protocols.contains(p))
                                {
                                    self.protocol_error(
                                        conn,
                                        ev,
                                        0x0817b3dd,
                                        "server selected an unoffered or malformed subprotocol",
                                    );
                                    return;
                                }
                                self.ready = true;
                                let resp_headers = h3::response_headers(&headers);
                                ev.push(Ev::Ready {
                                    protocol,
                                    headers: resp_headers,
                                });
                            }
                        } else {
                            ev.push(Ev::Error(format!(
                                "WebTransport CONNECT rejected with status {status}"
                            )));
                            self.mark_closed(ev, 0, Vec::new(), true);
                            let _ = conn.close(true, 0, b"connect rejected");
                        }
                    }
                    Err(e) => {
                        ev.push(Ev::Error(format!("failed to decode CONNECT response: {e}")));
                    }
                }
            } else if ty == h3::FRAME_DATA {
                // Capsule-protocol content. Best-effort scan for a session close.
                self.scan_capsules(&payload, ev);
            }
        }
    }

    fn scan_capsules(&mut self, payload: &[u8], ev: &mut Vec<Ev>) {
        // A capsule may span multiple HTTP/3 DATA frames (RFC 9297), so append
        // this frame's payload and consume only whole capsules from the buffer.
        if self.closed {
            return;
        }
        if self.capsule_buf.extend_from_slice(payload).is_err() {
            self.parser_limit_exceeded(ev);
            return;
        }
        while !self.closed {
            let (ty, start, len) = {
                let buf = &self.capsule_buf;
                let Some((ty, n1)) = h3::read_varint(buf) else {
                    break;
                };
                let Some((len, n2)) = h3::read_varint(&buf[n1..]) else {
                    break;
                };
                (ty, n1 + n2, len)
            };
            // Guard against a hostile/oversized declared length.
            if len > MAX_CONTROL_FRAME as u64 {
                ev.push(Ev::Error("oversized WebTransport capsule".to_string()));
                self.mark_closed(ev, 0, Vec::new(), true);
                break;
            }
            let len = len as usize;
            if self.capsule_buf.len() < start + len {
                break; // capsule not fully arrived; wait for the next DATA frame
            }
            let value = match self.capsule_buf.copy_range(start..start + len) {
                Ok(value) => value,
                Err(()) => {
                    self.parser_limit_exceeded(ev);
                    return;
                }
            };
            self.capsule_buf.consume(start + len);

            if ty == h3::WT_CLOSE_SESSION_CAPSULE && !self.closed {
                let (code, reason) = if value.len() >= 4 {
                    (
                        u32::from_be_bytes([value[0], value[1], value[2], value[3]]),
                        h3::truncate_close_reason(value[4..].to_vec()),
                    )
                } else {
                    (0, Vec::new())
                };
                self.mark_closed(ev, code, reason, true);
            } else if ty == h3::WT_DRAIN_SESSION_CAPSULE && !self.drain_emitted && !self.closed {
                self.drain_emitted = true;
                ev.push(Ev::Draining);
            } else if matches!(
                ty,
                h3::WT_MAX_DATA_CAPSULE
                    | h3::WT_MAX_STREAMS_BIDI_CAPSULE
                    | h3::WT_MAX_STREAMS_UNI_CAPSULE
                    | h3::WT_DATA_BLOCKED_CAPSULE
                    | h3::WT_STREAMS_BLOCKED_BIDI_CAPSULE
                    | h3::WT_STREAMS_BLOCKED_UNI_CAPSULE
            ) {
                let Some((v, n)) = h3::read_varint(&value) else {
                    self.flow_error(ev, "truncated flow-control capsule");
                    return;
                };
                let stream_limit =
                    ty != h3::WT_MAX_DATA_CAPSULE && ty != h3::WT_DATA_BLOCKED_CAPSULE;
                if n != value.len() || (stream_limit && v > (1 << 60)) {
                    self.flow_error(ev, "invalid flow-control capsule");
                    return;
                }
                let limit = match ty {
                    h3::WT_MAX_DATA_CAPSULE => Some(&mut self.fc_max_data_remote),
                    h3::WT_MAX_STREAMS_BIDI_CAPSULE => Some(&mut self.fc_max_streams_bidi_remote),
                    h3::WT_MAX_STREAMS_UNI_CAPSULE => Some(&mut self.fc_max_streams_uni_remote),
                    _ => None,
                };
                if let Some(limit) = limit {
                    if v <= *limit {
                        self.flow_error(ev, "flow-control credit must increase");
                        return;
                    }
                    *limit = v;
                }
            }
        }
    }

    /// Server role: parse the client's Extended CONNECT request on the session
    /// stream, validate it, respond `200`, and surface the request to the app.
    fn parse_server_connect_frames(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        ev: &mut Vec<Ev>,
    ) {
        // Streams can arrive before SETTINGS because QUIC orders each stream
        // independently. Keep CONNECT buffered until negotiation is validated.
        if self.peer_settings.is_none() {
            return;
        }
        let settings = self.peer_settings.unwrap();
        // ENABLE_CONNECT_PROTOCOL is advertised by the server; browsers need
        // not advertise it when acting as clients.
        if !settings.h3_datagram || !(settings.enable_webtransport || settings.wt_max_sessions > 0)
        {
            self.reject_connect(conn, id, 400, ev);
            return;
        }
        while !self.closed {
            let (frame, headers_done) = {
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                (st.frames.next_frame(), st.headers_done)
            };
            let Some((ty, payload)) = frame else { break };

            if ty == h3::FRAME_HEADERS && !headers_done {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.headers_done = true;
                }
                match h3::decode_header_block(&payload) {
                    Ok(headers) => self.accept_connect(conn, id, &headers, ev),
                    Err(e) => {
                        ev.push(Ev::Error(format!("failed to decode CONNECT request: {e}")));
                        self.reject_connect(conn, id, 400, ev);
                    }
                }
            } else if ty == h3::FRAME_DATA {
                self.scan_capsules(&payload, ev);
            }
        }
    }

    fn accept_connect(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        headers: &[quiche::h3::Header],
        ev: &mut Vec<Ev>,
    ) {
        use quiche::h3::NameValue;
        let mut method: Option<&[u8]> = None;
        let mut protocol: Option<&[u8]> = None;
        let mut authority = String::new();
        let mut path = String::from("/");
        let mut origin: Option<String> = None;
        let mut protocol_fields: Vec<String> = Vec::new();
        let mut extra: Vec<(String, String)> = Vec::new();
        for h in headers {
            let name = h.name();
            let value = h.value();
            match name {
                b":method" => method = Some(value),
                b":protocol" => protocol = Some(value),
                b":authority" => authority = String::from_utf8_lossy(value).into_owned(),
                b":path" => path = String::from_utf8_lossy(value).into_owned(),
                b"origin" => origin = Some(String::from_utf8_lossy(value).into_owned()),
                _ if name == h3::WT_AVAILABLE_PROTOCOLS => {
                    protocol_fields.push(String::from_utf8_lossy(value).into_owned());
                }
                _ if name.starts_with(b":") => {} // other pseudo-headers
                _ => extra.push((
                    String::from_utf8_lossy(name).into_owned(),
                    String::from_utf8_lossy(value).into_owned(),
                )),
            }
        }

        let requested_protocols = h3::parse_sf_string_list(&protocol_fields.join(", "));
        let ok = method == Some(b"CONNECT".as_ref()) && protocol == Some(b"webtransport".as_ref());
        if !ok {
            self.reject_connect(conn, id, 400, ev);
            return;
        }

        // Origin verification (draft-ietf-webtrans-http3 3.3): if an allowlist is
        // configured, the request's origin must be present in it.
        if let Some(allowed) = &self.allowed_origins {
            let permitted = origin
                .as_deref()
                .map(|o| allowed.iter().any(|a| a == o))
                .unwrap_or(false);
            if !permitted {
                self.reject_connect(conn, id, 403, ev);
                return;
            }
        }

        // Select the first supported subprotocol that the client also offered.
        let selected: Option<String> = self
            .supported_protocols
            .iter()
            .find(|p| requested_protocols.iter().any(|r| r == *p))
            .cloned();

        // Build the 200 response: :status, the selected wt-protocol (if any), and
        // any configured static response headers. Scoped so the borrows of
        // `self.response_headers` and `selected` end before the stream mutation.
        let selected_ser = selected.as_ref().map(|p| h3::serialize_sf_string(p));
        let encoded = {
            let mut resp = vec![quiche::h3::Header::new(b":status", b"200")];
            if let Some(val) = &selected_ser {
                resp.push(quiche::h3::Header::new(h3::WT_PROTOCOL, val.as_bytes()));
            }
            for (k, v) in &self.response_headers {
                resp.push(quiche::h3::Header::new(k.as_bytes(), v.as_bytes()));
            }
            h3::encode_headers_frame(&resp)
        };
        match encoded {
            Ok(frame) => {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.out.push_back(OutChunk {
                        data: frame,
                        off: 0,
                        request_id: None,
                    });
                }
                self.ready = true;
                let (remote_addr, remote_port) = match self.remote_addr {
                    Some(a) => (a.ip().to_string(), a.port()),
                    None => (String::new(), 0),
                };
                ev.push(Ev::ServerReady {
                    authority,
                    path,
                    origin,
                    headers: extra,
                    requested_protocols,
                    protocol: selected,
                    remote_addr,
                    remote_port,
                });
            }
            Err(e) => ev.push(Ev::Error(format!("failed to encode CONNECT response: {e}"))),
        }
    }

    fn reject_connect(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        status: u16,
        ev: &mut Vec<Ev>,
    ) {
        let status_str = status.to_string();
        let resp = vec![quiche::h3::Header::new(b":status", status_str.as_bytes())];
        if let Ok(frame) = h3::encode_headers_frame(&resp) {
            if let Some(st) = self.streams.get_mut(&id) {
                st.out.push_back(OutChunk {
                    data: frame,
                    off: 0,
                    request_id: None,
                });
                st.fin_queued = true;
            }
        }
        if !self.ready && !self.closed {
            ev.push(Ev::Error(format!("rejected CONNECT with status {status}")));
        }
        self.mark_closed(ev, 0, Vec::new(), false);
        let _ = conn;
    }

    fn resolve_pending_uni(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        chunk: Vec<u8>,
        fin: bool,
        ev: &mut Vec<Ev>,
    ) {
        let buf = {
            let Some(st) = self.streams.get_mut(&id) else {
                return;
            };
            if st.class_buf.extend_from_slice(&chunk).is_err() {
                self.parser_limit_exceeded(ev);
                return;
            }
            st.class_buf.to_vec()
        };
        let Some((ty, n)) = h3::read_varint(&buf) else {
            // Peer FIN'd before we could classify: no more bytes will ever
            // arrive, so drop the entry rather than leak a Pending stream.
            if fin {
                self.streams.remove(&id);
            }
            return; // need more bytes for the type varint
        };
        match ty {
            t if t == h3::H3_CONTROL_STREAM_TYPE => {
                if self
                    .streams
                    .iter()
                    .any(|(&other, st)| other != id && st.role == Role::PeerControl)
                {
                    self.protocol_error(conn, ev, 0x103, "duplicate HTTP/3 control stream");
                    return;
                }
                let rest = buf[n..].to_vec();
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                st.role = Role::PeerControl;
                st.class_buf.clear();
                // Use the common control path so a budget failure in the very
                // first chunk is fatal even if the peer sends nothing else.
                self.process_recv(conn, id, rest, fin, ev);
            }
            t if t == h3::QPACK_ENCODER_STREAM_TYPE || t == h3::QPACK_DECODER_STREAM_TYPE => {
                let encoder = t == h3::QPACK_ENCODER_STREAM_TYPE;
                if self
                    .streams
                    .iter()
                    .any(|(&other, st)| other != id && st.role == (Role::PeerQpack { encoder }))
                {
                    self.protocol_error(conn, ev, 0x103, "duplicate QPACK stream");
                    return;
                }
                let st = self.streams.get_mut(&id).unwrap();
                st.role = Role::PeerQpack { encoder };
                st.class_buf.clear();
                if fin {
                    self.protocol_error(conn, ev, 0x104, "peer closed a critical QPACK stream");
                }
            }
            t if t == h3::WT_UNI_STREAM_TYPE => {
                let Some((session, n2)) = h3::read_varint(&buf[n..]) else {
                    // Peer FIN'd mid-header: reclaim the entry instead of leaking.
                    if fin {
                        self.streams.remove(&id);
                    }
                    return; // need session id
                };
                if session != CONNECT_ID {
                    let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
                    let Some(st) = self.streams.get_mut(&id) else {
                        return;
                    };
                    st.role = Role::Ignored;
                    st.class_buf.clear();
                    return;
                }
                let data = buf[n + n2..].to_vec();
                {
                    let Some(st) = self.streams.get_mut(&id) else {
                        return;
                    };
                    st.role = Role::WtData { bidi: false };
                    st.class_buf.clear();
                }
                self.streams.get_mut(&id).unwrap().recv_prefix = (n + n2) as u64;
                if !self.account_incoming(false, ev)
                    || !self.account_receive(id, data.len() as u64, ev)
                {
                    return;
                }
                ev.push(Ev::IncomingStream { id, bidi: false });
                if !data.is_empty() {
                    ev.push(Ev::StreamData { id, data });
                }
                if fin {
                    if let Some(st) = self.streams.get_mut(&id) {
                        st.recv_dead = true;
                    }
                    ev.push(Ev::StreamFinished { id });
                }
            }
            _ => {
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                st.role = Role::Ignored;
                st.class_buf.clear();
            }
        }
    }

    fn resolve_pending_bidi(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        chunk: Vec<u8>,
        fin: bool,
        ev: &mut Vec<Ev>,
    ) {
        let buf = {
            let Some(st) = self.streams.get_mut(&id) else {
                return;
            };
            if st.class_buf.extend_from_slice(&chunk).is_err() {
                self.parser_limit_exceeded(ev);
                return;
            }
            st.class_buf.to_vec()
        };
        let Some((signal, n)) = h3::read_varint(&buf) else {
            // Peer FIN'd before we could classify: reclaim the entry.
            if fin {
                self.streams.remove(&id);
            }
            return;
        };
        if signal != h3::WT_BIDI_FRAME_TYPE {
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
            let Some(st) = self.streams.get_mut(&id) else {
                return;
            };
            st.role = Role::Ignored;
            st.class_buf.clear();
            return;
        }
        let Some((session, n2)) = h3::read_varint(&buf[n..]) else {
            // Peer FIN'd mid-header: reclaim the entry instead of leaking.
            if fin {
                self.streams.remove(&id);
            }
            return;
        };
        if session != CONNECT_ID {
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
            let Some(st) = self.streams.get_mut(&id) else {
                return;
            };
            st.role = Role::Ignored;
            st.class_buf.clear();
            return;
        }
        let data = buf[n + n2..].to_vec();
        {
            let Some(st) = self.streams.get_mut(&id) else {
                return;
            };
            st.role = Role::WtData { bidi: true };
            st.class_buf.clear();
        }
        self.streams.get_mut(&id).unwrap().recv_prefix = (n + n2) as u64;
        if !self.account_incoming(true, ev) || !self.account_receive(id, data.len() as u64, ev) {
            return;
        }
        ev.push(Ev::IncomingStream { id, bidi: true });
        if !data.is_empty() {
            ev.push(Ev::StreamData { id, data });
        }
        if fin {
            if let Some(st) = self.streams.get_mut(&id) {
                st.recv_dead = true;
            }
            ev.push(Ev::StreamFinished { id });
        }
    }

    pub fn on_datagrams(&mut self, conn: &mut quiche::Connection, ev: &mut Vec<Ev>) {
        let mut buf = [0u8; 65_536];
        loop {
            match conn.dgram_recv(&mut buf) {
                Ok(n) => {
                    let data = &buf[..n];
                    if let Some((qsi, off)) = h3::read_varint(data) {
                        // quarter-stream-id for our session is CONNECT_ID / 4 = 0.
                        if qsi == CONNECT_ID / 4 {
                            ev.push(Ev::Datagram(data[off..].to_vec()));
                        }
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(_) => break,
            }
        }
    }

    pub fn send_datagram(
        &mut self,
        conn: &mut quiche::Connection,
        data: &[u8],
        request_id: u64,
        ev: &mut Vec<Ev>,
    ) {
        let mut framed = Vec::with_capacity(data.len() + 1);
        h3::put_varint(CONNECT_ID / 4, &mut framed);
        framed.extend_from_slice(data);
        let sent = conn.dgram_send(&framed).is_ok();
        ev.push(Ev::DatagramAck { request_id, sent });
    }

    pub fn max_datagram_size(&self, conn: &quiche::Connection) -> usize {
        // Subtract the 1-byte quarter-stream-id prefix (session id 0 → varint 0).
        conn.dgram_max_writable_len().unwrap_or(0).saturating_sub(1)
    }

    pub fn open_stream(&mut self, bidi: bool, request_id: u64, ev: &mut Vec<Ev>) {
        self.pending_opens.push_back((bidi, request_id));
        self.release_pending_opens(ev);
    }

    fn uses_session_flow_control(&self) -> bool {
        self.peer_settings
            .is_some_and(|settings| settings.wt_max_sessions > 0)
    }

    fn release_pending_opens(&mut self, ev: &mut Vec<Ev>) {
        if !self.ready || self.closed {
            return;
        }
        let mut pending = std::mem::take(&mut self.pending_opens);
        while let Some((bidi, request_id)) = pending.pop_front() {
            let first = if self.is_server {
                if bidi {
                    SERVER_FIRST_WT_BIDI_ID
                } else {
                    SERVER_FIRST_WT_UNI_ID
                }
            } else if bidi {
                CLIENT_FIRST_WT_BIDI_ID
            } else {
                CLIENT_FIRST_WT_UNI_ID
            };
            let next = if bidi { self.next_bidi } else { self.next_uni };
            let limit = if bidi {
                self.fc_max_streams_bidi_remote
            } else {
                self.fc_max_streams_uni_remote
            };
            if self.uses_session_flow_control() && (next - first) / 4 >= limit {
                self.pending_opens.push_back((bidi, request_id));
                continue;
            }
            self.allocate_stream(bidi, request_id, ev);
        }
    }

    fn allocate_stream(&mut self, bidi: bool, request_id: u64, ev: &mut Vec<Ev>) {
        let id = if bidi {
            let id = self.next_bidi;
            self.next_bidi += 4;
            id
        } else {
            let id = self.next_uni;
            self.next_uni += 4;
            id
        };
        // Seed the WebTransport stream signal prefix.
        let mut prefix = Vec::new();
        if bidi {
            h3::put_varint(h3::WT_BIDI_FRAME_TYPE, &mut prefix);
        } else {
            h3::put_varint(h3::WT_UNI_STREAM_TYPE, &mut prefix);
        }
        h3::put_varint(CONNECT_ID, &mut prefix);
        self.streams.insert(
            id,
            Stream::with_prefix(Role::WtData { bidi }, prefix, self.parser_memory.clone()),
        );
        ev.push(Ev::StreamOpened { request_id, id });
    }

    pub fn stream_write(&mut self, id: u64, data: Vec<u8>, request_id: u64, ev: &mut Vec<Ev>) {
        match self.streams.get_mut(&id) {
            Some(st) => st.out.push_back(OutChunk {
                data,
                off: 0,
                request_id: Some(request_id),
            }),
            None => ev.push(Ev::WriteAck { request_id }),
        }
    }

    pub fn stream_fin(&mut self, id: u64) {
        if let Some(st) = self.streams.get_mut(&id) {
            st.fin_queued = true;
        }
    }

    pub fn stream_reset(&mut self, conn: &mut quiche::Connection, id: u64, code: u64) {
        if self.streams.contains_key(&id) {
            // The QUIC RESET_STREAM code must be the WebTransport application code
            // mapped into the HTTP/3 error space.
            let h = h3::webtransport_code_to_http_code(code);
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Write, h);
            self.reconcile_send_final_size(conn, id);
            if let Some(st) = self.streams.get_mut(&id) {
                st.out.clear();
                st.fin_sent = true;
            }
        }
    }

    pub fn stream_stop_sending(&mut self, conn: &mut quiche::Connection, id: u64, code: u64) {
        if self.streams.contains_key(&id) {
            let h = h3::webtransport_code_to_http_code(code);
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, h);
            if let Some(st) = self.streams.get_mut(&id) {
                st.recv_dead = true;
            }
        }
    }

    pub fn set_paused(&mut self, id: u64, paused: bool) {
        if let Some(st) = self.streams.get_mut(&id) {
            st.paused = paused;
        }
    }

    pub fn close(&mut self, code: u32, reason: Vec<u8>) {
        if self.close_req.is_none() {
            self.close_req = Some(CloseReq {
                code,
                reason: h3::truncate_close_reason(reason),
                capsule_queued: false,
            });
        }
    }

    /// Queue a DRAIN_WEBTRANSPORT_SESSION capsule on the session stream, telling
    /// the peer we intend to close soon. The session stays usable; this is not a
    /// close. Emitted at most once and only while the session is open.
    pub fn send_drain(&mut self) {
        if self.closed || self.drain_sent {
            return;
        }
        let mut capsule = Vec::new();
        h3::put_varint(h3::WT_DRAIN_SESSION_CAPSULE, &mut capsule);
        h3::put_varint(0, &mut capsule); // DRAIN carries no content
        let body = h3::frame(h3::FRAME_DATA, &capsule);
        // Only mark it sent once the CONNECT stream actually exists and we have
        // queued the capsule; a drain() issued before establishment retries later.
        if let Some(st) = self.streams.get_mut(&CONNECT_ID) {
            st.out.push_back(OutChunk {
                data: body,
                off: 0,
                request_id: None,
            });
            self.drain_sent = true;
        }
    }

    /// Queue a single-varint-valued capsule (WT_MAX_DATA / WT_MAX_STREAMS_*) on
    /// the session stream.
    fn queue_session_capsule(&mut self, ty: u64, value: u64) {
        let mut val = Vec::new();
        h3::put_varint(value, &mut val);
        let mut capsule = Vec::new();
        h3::put_varint(ty, &mut capsule);
        h3::put_varint(val.len() as u64, &mut capsule);
        capsule.extend_from_slice(&val);
        let body = h3::frame(h3::FRAME_DATA, &capsule);
        if let Some(st) = self.streams.get_mut(&CONNECT_ID) {
            st.out.push_back(OutChunk {
                data: body,
                off: 0,
                request_id: None,
            });
        }
    }

    /// Flush pending outbound stream data and drive the graceful-close sequence.
    /// Returns an optional `(code, reason)` if the QUIC connection should now be
    /// closed.
    pub fn flush(&mut self, conn: &mut quiche::Connection, ev: &mut Vec<Ev>) {
        if let Some((code, message)) = self.terminal_error.take() {
            let _ = conn.close(true, code, &message);
        }
        // Once established, grant the peer ample per-session flow-control credit.
        // Exactly one WebTransport session rides this QUIC connection, so QUIC's
        // connection-level flow control is the real limit; granting a large WT
        // credit here keeps a strict peer that honors our advertised session
        // limits from stalling, while QUIC does the actual enforcement.
        if self.ready && !self.fc_credit_granted && !self.closed {
            self.fc_credit_granted = true;
            self.fc_max_data_local = WT_GRANT_MAX_DATA;
            self.fc_max_streams_local = [WT_GRANT_MAX_STREAMS; 2];
            self.queue_session_capsule(h3::WT_MAX_DATA_CAPSULE, WT_GRANT_MAX_DATA);
            self.queue_session_capsule(h3::WT_MAX_STREAMS_BIDI_CAPSULE, WT_GRANT_MAX_STREAMS);
            self.queue_session_capsule(h3::WT_MAX_STREAMS_UNI_CAPSULE, WT_GRANT_MAX_STREAMS);
        }
        // Replenish cumulative credit for long-lived sessions before the peer
        // reaches the advertised limit. QUIC independently bounds buffering.
        if self.ready && self.fc_credit_granted && self.uses_session_flow_control() && !self.closed
        {
            if self.fc_max_data_local.saturating_sub(self.fc_data_received) <= WT_GRANT_MAX_DATA / 2
            {
                let limit = self
                    .fc_data_received
                    .saturating_add(WT_GRANT_MAX_DATA)
                    .min((1 << 62) - 1);
                if limit > self.fc_max_data_local {
                    self.fc_max_data_local = limit;
                    self.queue_session_capsule(h3::WT_MAX_DATA_CAPSULE, limit);
                }
            }
            for direction in 0..2 {
                if self.fc_max_streams_local[direction]
                    .saturating_sub(self.fc_streams_received[direction])
                    <= WT_GRANT_MAX_STREAMS / 2
                {
                    let limit = self.fc_streams_received[direction]
                        .saturating_add(WT_GRANT_MAX_STREAMS)
                        .min(1 << 60);
                    if limit > self.fc_max_streams_local[direction] {
                        self.fc_max_streams_local[direction] = limit;
                        self.queue_session_capsule(
                            if direction == 0 {
                                h3::WT_MAX_STREAMS_UNI_CAPSULE
                            } else {
                                h3::WT_MAX_STREAMS_BIDI_CAPSULE
                            },
                            limit,
                        );
                    }
                }
            }
        }
        // Queue the CLOSE_WEBTRANSPORT_SESSION capsule + FIN once, if requested.
        if let Some(req) = self.close_req.as_mut() {
            if !req.capsule_queued {
                req.capsule_queued = true;
                let mut value = Vec::with_capacity(4 + req.reason.len());
                value.extend_from_slice(&req.code.to_be_bytes());
                value.extend_from_slice(&req.reason);
                let mut capsule = Vec::new();
                h3::put_varint(h3::WT_CLOSE_SESSION_CAPSULE, &mut capsule);
                h3::put_varint(value.len() as u64, &mut capsule);
                capsule.extend_from_slice(&value);
                let body = h3::frame(h3::FRAME_DATA, &capsule);
                if let Some(st) = self.streams.get_mut(&CONNECT_ID) {
                    st.out.push_back(OutChunk {
                        data: body,
                        off: 0,
                        request_id: None,
                    });
                    st.fin_queued = true;
                } else {
                    // No CONNECT stream (never established) → close QUIC directly.
                    self.want_quic_close = Some((req.code, req.reason.clone()));
                }
            }
        }

        self.release_pending_opens(ev);
        let ids: Vec<u64> = self.streams.keys().copied().collect();
        let mut wt_sent = 0u64;
        for id in ids {
            self.flush_stream(conn, id, ev, &mut wt_sent);
        }
        self.fc_data_sent += wt_sent;

        self.signal_flow_control_blocked();

        // Prune fully-terminated streams so a hostile peer churning streams
        // cannot grow the map without bound. Keep the CONNECT (session) stream
        // and our send-only control-plane streams for the session's lifetime.
        let is_server = self.is_server;
        self.streams.retain(|&id, st| {
            if id == CONNECT_ID || matches!(st.role, Role::LocalControlPlane) {
                return true;
            }
            // Direction from OUR perspective: a uni stream we initiated is
            // send-only; a uni stream the peer initiated is recv-only; bidi is
            // both. (id & 0x1 == 0 is client-initiated.)
            let is_uni = (id & 0x2) != 0;
            let our_init = if is_server {
                (id & 0x1) == 1
            } else {
                (id & 0x1) == 0
            };
            let can_recv = !is_uni || !our_init;
            let can_send = !is_uni || our_init;
            let recv_done = !can_recv || st.recv_dead;
            let send_done = !can_send || (st.fin_sent && st.out.is_empty());
            !(recv_done && send_done)
        });

        // Opens blocked on stream credit simply keep their WT-signal prefix queued
        // in `out`; `flush_stream` retries them on every loop until credit lands.

        // Graceful QUIC close, issued exactly once. After the CONNECT stream has
        // flushed the CLOSE_WEBTRANSPORT_SESSION capsule + FIN, close the HTTP/3
        // connection with H3_NO_ERROR (the WebTransport code/reason ride the
        // capsule, never the QUIC error field). The close is deferred one flush
        // pass so the driver's flush_send() serializes the capsule + FIN before the
        // CONNECTION_CLOSE frame supersedes all buffered stream frames. Ev::Closed
        // is emitted later by the driver's terminal branch (once conn.is_closed),
        // so JS never sees the close while the driver thread is still running.
        if !self.quic_close_issued {
            if self.want_quic_close.is_none() {
                if let Some(req) = self.close_req.as_ref() {
                    let connect_drained = self
                        .streams
                        .get(&CONNECT_ID)
                        .map(|s| s.out.is_empty() && s.fin_sent)
                        .unwrap_or(true);
                    if req.capsule_queued && connect_drained {
                        self.want_quic_close = Some((req.code, req.reason.clone()));
                    }
                }
            }
            if self.want_quic_close.is_some() {
                if self.close_deferred_once {
                    self.want_quic_close = None;
                    self.quic_close_issued = true;
                    let _ = conn.close(true, h3::H3_NO_ERROR, b"");
                } else {
                    self.close_deferred_once = true;
                }
            }
        }
    }

    /// If this session initiated a graceful close, the WebTransport close code and
    /// reason to surface to the application (the wire uses H3_NO_ERROR, but the JS
    /// `closeInfo` must carry the app-level code/reason).
    pub fn local_close_info(&self) -> Option<(u32, Vec<u8>)> {
        self.close_req
            .as_ref()
            .map(|req| (req.code, req.reason.clone()))
    }

    /// Emit WT_DATA_BLOCKED / WT_STREAMS_BLOCKED_* capsules when the peer's
    /// advertised WebTransport limits are holding back a send, once per distinct
    /// limit value, asking the peer to raise the limit.
    fn signal_flow_control_blocked(&mut self) {
        // Limits are only meaningful once the peer's SETTINGS have seeded them.
        if !self.uses_session_flow_control() || self.closed {
            return;
        }

        // Data-blocked: unsent WT stream payload while at the peer's WT_MAX_DATA.
        if self.fc_data_sent >= self.fc_max_data_remote
            && self.data_blocked_at != Some(self.fc_max_data_remote)
        {
            let has_pending = self.streams.values().any(|s| {
                matches!(s.role, Role::WtData { .. })
                    && s.out.iter().any(|c| c.request_id.is_some())
            });
            if has_pending {
                let limit = self.fc_max_data_remote;
                self.queue_session_capsule(h3::WT_DATA_BLOCKED_CAPSULE, limit);
                self.data_blocked_at = Some(limit);
            }
        }

        // Streams-blocked: a requested open is waiting at WT_MAX_STREAMS_*.
        let (first_bidi, first_uni) = if self.is_server {
            (SERVER_FIRST_WT_BIDI_ID, SERVER_FIRST_WT_UNI_ID)
        } else {
            (CLIENT_FIRST_WT_BIDI_ID, CLIENT_FIRST_WT_UNI_ID)
        };
        let opened_bidi = (self.next_bidi - first_bidi) / 4;
        let opened_uni = (self.next_uni - first_uni) / 4;

        if opened_bidi >= self.fc_max_streams_bidi_remote
            && self.pending_opens.iter().any(|(bidi, _)| *bidi)
            && self.streams_bidi_blocked_at != Some(self.fc_max_streams_bidi_remote)
        {
            let limit = self.fc_max_streams_bidi_remote;
            self.queue_session_capsule(h3::WT_STREAMS_BLOCKED_BIDI_CAPSULE, limit);
            self.streams_bidi_blocked_at = Some(limit);
        }
        if opened_uni >= self.fc_max_streams_uni_remote
            && self.pending_opens.iter().any(|(bidi, _)| !*bidi)
            && self.streams_uni_blocked_at != Some(self.fc_max_streams_uni_remote)
        {
            let limit = self.fc_max_streams_uni_remote;
            self.queue_session_capsule(h3::WT_STREAMS_BLOCKED_UNI_CAPSULE, limit);
            self.streams_uni_blocked_at = Some(limit);
        }
    }

    fn flush_stream(
        &mut self,
        conn: &mut quiche::Connection,
        id: u64,
        ev: &mut Vec<Ev>,
        wt_sent: &mut u64,
    ) {
        self.reconcile_send_final_size(conn, id);
        let enforce = self.uses_session_flow_control();
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        // Only WebTransport stream payload (a WtData stream's request_id-carrying
        // chunks) counts toward WT_MAX_DATA; the stream-signal prefix and CONNECT
        // capsules do not.
        let is_wt_data = matches!(st.role, Role::WtData { .. });
        // Note: we call `stream_send` directly rather than pre-checking
        // `stream_capacity`, because a stream does not exist in quiche until its
        // first `stream_send`, and `stream_capacity` errors (InvalidStreamState)
        // on a not-yet-created stream.
        while let Some(front) = st.out.front_mut() {
            let remaining_len = front.data.len() - front.off;
            let is_payload = is_wt_data && front.request_id.is_some();
            let allowance = self
                .fc_max_data_remote
                .saturating_sub(self.fc_data_sent.saturating_add(*wt_sent));
            let amount = if enforce && is_payload {
                remaining_len.min(allowance as usize)
            } else {
                remaining_len
            };
            if amount == 0 && remaining_len != 0 {
                return;
            }
            match conn.stream_send(id, &front.data[front.off..front.off + amount], false) {
                Ok(sent) => {
                    if is_payload {
                        *wt_sent += sent as u64;
                        st.send_payload += sent as u64;
                    }
                    front.off += sent;
                    if front.off >= front.data.len() {
                        let done = st.out.pop_front().unwrap();
                        if let Some(rid) = done.request_id {
                            ev.push(Ev::WriteAck { request_id: rid });
                        }
                    }
                    if sent < remaining_len {
                        return; // no more capacity right now
                    }
                }
                Err(quiche::Error::Done) => return,
                Err(quiche::Error::StreamLimit) => return, // no stream credit yet; retry next loop
                Err(quiche::Error::StreamStopped(code)) => {
                    let app = h3::http_code_to_webtransport_code(code).unwrap_or(code);
                    ev.push(Ev::StreamStopSending { id, code: app });
                    // Settle every queued write's promise so a stalled write()
                    // (and its WritableStream) cannot hang forever.
                    for chunk in st.out.drain(..) {
                        if let Some(rid) = chunk.request_id {
                            ev.push(Ev::WriteAck { request_id: rid });
                        }
                    }
                    st.fin_sent = true;
                    return;
                }
                Err(_) => return,
            }
        }

        if st.out.is_empty()
            && st.fin_queued
            && !st.fin_sent
            && conn.stream_send(id, &[], true).is_ok()
        {
            st.fin_sent = true;
        }
    }

    pub fn mark_closed(&mut self, ev: &mut Vec<Ev>, code: u32, reason: Vec<u8>, remote: bool) {
        if self.closed {
            return;
        }
        self.closed = true;
        // Closed connections can linger during QUIC draining. Release receive
        // allocations immediately rather than tying the budget to that timer.
        self.capsule_buf.clear();
        for stream in self.streams.values_mut() {
            stream.frames.buf.clear();
            stream.class_buf.clear();
            stream.recv_dead = true;
        }
        ev.push(Ev::Closed {
            code,
            reason,
            remote,
        });
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    fn parser_limit_exceeded(&mut self, ev: &mut Vec<Ev>) {
        if !self.closed {
            ev.push(Ev::Error("parser memory limit exceeded".to_string()));
            self.mark_closed(ev, 0, Vec::new(), true);
        }
    }

    /// Whether a stream's HTTP/3 frame parser has latched a fatal error.
    fn frame_error(&self, id: u64) -> bool {
        self.streams
            .get(&id)
            .map(|s| s.frames.error)
            .unwrap_or(false)
    }

    /// Return credit for buffered payload omitted from a reset's final size.
    fn reconcile_send_final_size(&mut self, conn: &quiche::Connection, id: u64) {
        let Some(final_size) = conn.stream_send_final_size(id) else {
            return;
        };
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        if !matches!(st.role, Role::WtData { .. }) {
            return;
        }
        let final_payload = final_size.saturating_sub(st.send_prefix);
        // RESET_STREAM discards bytes accepted by quiche but never serialized.
        // Those bytes are absent from the final size and consume no WT credit.
        if final_payload < st.send_payload {
            self.fc_data_sent = self
                .fc_data_sent
                .saturating_sub(st.send_payload - final_payload);
            st.send_payload = final_payload;
        }
    }

    fn flow_error(&mut self, ev: &mut Vec<Ev>, message: &str) {
        if self.closed {
            return;
        }
        self.terminal_error = Some((0x045d4487, message.as_bytes().to_vec()));
        ev.push(Ev::Error(message.to_string()));
        self.mark_closed(ev, 0, Vec::new(), true);
    }

    fn account_incoming(&mut self, bidi: bool, ev: &mut Vec<Ev>) -> bool {
        let direction = usize::from(bidi);
        self.fc_streams_received[direction] += 1;
        let limit = self.fc_max_streams_local[direction];
        if self.uses_session_flow_control() && self.fc_streams_received[direction] > limit {
            self.flow_error(ev, "peer exceeded the session stream limit");
            return false;
        }
        true
    }

    fn account_receive(&mut self, id: u64, amount: u64, ev: &mut Vec<Ev>) -> bool {
        self.fc_data_received = self.fc_data_received.saturating_add(amount);
        if let Some(st) = self.streams.get_mut(&id) {
            st.recv_payload = st.recv_payload.saturating_add(amount);
        }
        let limit = self.fc_max_data_local;
        if self.uses_session_flow_control() && self.fc_data_received > limit {
            self.flow_error(ev, "peer exceeded the session data limit");
            return false;
        }
        true
    }

    fn protocol_error(
        &mut self,
        conn: &mut quiche::Connection,
        ev: &mut Vec<Ev>,
        code: u64,
        message: &str,
    ) {
        if self.closed {
            return;
        }
        ev.push(Ev::Error(message.to_string()));
        self.mark_closed(ev, 0, Vec::new(), true);
        let _ = conn.close(true, code, message.as_bytes());
    }

    /// Stop reading and release parser buffers after a fatal stream error.
    fn kill_recv(&mut self, conn: &mut quiche::Connection, id: u64) {
        if let Some(st) = self.streams.get_mut(&id) {
            st.recv_dead = true;
            st.frames.buf.clear();
            st.class_buf.clear();
        }
        let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::LocalControlPlane => "LocalControlPlane",
        Role::Connect => "Connect",
        Role::PeerControl => "PeerControl",
        Role::Ignored => "Ignored",
        Role::PeerQpack { .. } => "PeerQpack",
        Role::WtData { .. } => "WtData",
        Role::PendingUni => "PendingUni",
        Role::PendingBidi => "PendingBidi",
    }
}

/// Classify a not-yet-seen stream id by its QUIC stream-type bits.
/// Classify a not-yet-seen peer-initiated stream by its QUIC stream-type bits.
/// The peer is the server (for a client-role session) or the client (for a
/// server-role session).
fn classify_new(id: u64, is_server: bool) -> Role {
    if is_server {
        // Peer = client (id & 0x1 == 0).
        match id & 0x3 {
            0x0 => {
                if id == CONNECT_ID {
                    Role::Connect // the incoming Extended CONNECT
                } else {
                    Role::PendingBidi // a client-opened WebTransport bidi stream
                }
            }
            0x2 => Role::PendingUni, // client control/QPACK or WT uni stream
            _ => Role::Ignored,
        }
    } else {
        // Peer = server (id & 0x1 == 1).
        match id & 0x3 {
            0x3 => Role::PendingUni,  // server-initiated unidirectional
            0x1 => Role::PendingBidi, // server-initiated bidirectional
            _ => Role::Ignored,       // an unexpected client-initiated id
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn established_server(
        settings: Vec<u8>,
    ) -> (quiche::Connection, quiche::Connection, WtSession) {
        let (mut client, mut server) =
            crate::test_support::pair(None, Some("wrong.example"), false);
        crate::test_support::exchange(&mut client, &mut server);
        assert!(server.is_established());
        let mut session = server_with_connect(ParserMemory::new(64 * 1024 * 1024));
        session.ready = false;
        session.peer_settings = None;
        session.on_established();
        let mut control = vec![0];
        control.extend_from_slice(&h3::frame(h3::FRAME_SETTINGS, &settings));
        client.stream_send(2, &control, false).unwrap();
        let headers = h3::connect_headers("wrong.example", "/", None, &[], &[]);
        client
            .stream_send(0, &h3::encode_headers_frame(&headers).unwrap(), false)
            .unwrap();
        crate::test_support::exchange(&mut client, &mut server);
        let mut events = Vec::new();
        // Exercise out-of-order arrival: CONNECT must wait for SETTINGS.
        session.on_readable(&mut server, 0, false, &mut events);
        assert!(!session.ready);
        session.on_readable(&mut server, 2, false, &mut events);
        assert!(session.ready);
        (client, server, session)
    }

    fn capsule(ty: u64, value: u64) -> Vec<u8> {
        let mut body = Vec::new();
        h3::put_varint(value, &mut body);
        let mut out = Vec::new();
        h3::put_varint(ty, &mut out);
        h3::put_varint(body.len() as u64, &mut out);
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn resetting_unsent_data_restores_session_send_credit() {
        let (_, mut conn, mut session) = established_server(h3::settings_payload());
        session.fc_max_data_remote = 8;
        session.open_stream(false, 1, &mut Vec::new());
        let id = SERVER_FIRST_WT_UNI_ID;
        session.stream_write(id, vec![1; 8], 2, &mut Vec::new());
        session.flush(&mut conn, &mut Vec::new());
        assert_eq!(session.fc_data_sent, 8);
        // Reset before conn.send serializes any of this stream's bytes.
        session.stream_reset(&mut conn, id, 0);
        assert_eq!(session.fc_data_sent, 0);
    }

    #[test]
    fn reset_final_size_consumes_session_credit_without_delivered_payload() {
        let (mut client, mut server, mut session) = established_server(h3::settings_payload());
        let mut prefix = Vec::new();
        h3::put_varint(h3::WT_UNI_STREAM_TYPE, &mut prefix);
        h3::put_varint(0, &mut prefix);
        client.stream_send(14, &prefix, false).unwrap();
        crate::test_support::exchange(&mut client, &mut server);
        session.on_readable(&mut server, 14, false, &mut Vec::new());
        client.stream_send(14, &[5; 8192], false).unwrap();
        // Serialize the bytes into QUIC without delivering them to the app.
        crate::test_support::exchange(&mut client, &mut server);
        client
            .stream_shutdown(14, quiche::Shutdown::Write, 0)
            .unwrap();
        crate::test_support::exchange(&mut client, &mut server);
        let mut events = Vec::new();
        session.on_readable(&mut server, 14, false, &mut events);
        assert_eq!(session.fc_data_received, 8192);
        assert!(events
            .iter()
            .any(|e| matches!(e, Ev::StreamReset { id: 14, .. })));
        assert!(!events.iter().any(|e| matches!(e, Ev::StreamData { .. })));
    }

    #[test]
    fn peer_exceeding_advertised_data_or_stream_credit_is_terminal() {
        for data_limit in [false, true] {
            let (mut client, mut server, mut session) = established_server(h3::settings_payload());
            if data_limit {
                session.fc_data_received = session.fc_max_data_local;
            } else {
                session.fc_streams_received[0] = session.fc_max_streams_local[0];
            }
            let mut stream = Vec::new();
            h3::put_varint(h3::WT_UNI_STREAM_TYPE, &mut stream);
            h3::put_varint(0, &mut stream);
            stream.push(1);
            client.stream_send(14, &stream, true).unwrap();
            crate::test_support::exchange(&mut client, &mut server);
            let mut events = Vec::new();
            session.on_readable(&mut server, 14, false, &mut events);
            assert!(session.closed);
            assert!(!events
                .iter()
                .any(|e| matches!(e, Ev::IncomingStream { .. } | Ev::StreamData { .. })));
        }
    }

    #[test]
    fn new_uni_and_bidi_streams_wait_for_global_read_backpressure_to_clear() {
        for bidi in [false, true] {
            let (mut client, mut server, mut session) = established_server(h3::settings_payload());
            let mut prefix = Vec::new();
            h3::put_varint(
                if bidi {
                    h3::WT_BIDI_FRAME_TYPE
                } else {
                    h3::WT_UNI_STREAM_TYPE
                },
                &mut prefix,
            );
            h3::put_varint(0, &mut prefix);
            prefix.extend_from_slice(&[9; 1024]);
            let mut events = Vec::new();
            for i in 0..20 {
                let id = if bidi { 4 + i * 4 } else { 14 + i * 4 };
                client.stream_send(id, &prefix, true).unwrap();
                crate::test_support::exchange(&mut client, &mut server);
                session.on_readable(&mut server, id, true, &mut events);
                session.flush(&mut server, &mut Vec::new());
                crate::test_support::exchange(&mut client, &mut server);
            }
            assert!(
                events.is_empty(),
                "application events leaked while backpressured"
            );
            for i in 0..20 {
                let id = if bidi { 4 + i * 4 } else { 14 + i * 4 };
                session.on_readable(&mut server, id, false, &mut events);
            }
            let bytes: usize = events
                .iter()
                .filter_map(|e| match e {
                    Ev::StreamData { data, .. } => Some(data.len()),
                    _ => None,
                })
                .sum();
            assert_eq!(bytes, 20 * 1024);
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, Ev::IncomingStream { .. }))
                    .count(),
                20
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, Ev::StreamFinished { .. }))
                    .count(),
                20
            );
        }
    }

    #[test]
    fn zero_session_credit_blocks_opens_and_writes_until_capsules_raise_limits() {
        let mut settings = Vec::new();
        for (id, value) in [
            (h3::SETTINGS_ENABLE_CONNECT_PROTOCOL, 1),
            (h3::SETTINGS_H3_DATAGRAM, 1),
            (h3::SETTINGS_WT_MAX_SESSIONS, 1),
        ] {
            h3::put_varint(id, &mut settings);
            h3::put_varint(value, &mut settings);
        }
        let (mut client, mut server, mut session) = established_server(settings);
        let mut events = Vec::new();
        session.open_stream(false, 11, &mut events);
        session.open_stream(true, 12, &mut events);
        session.flush(&mut server, &mut events);
        assert!(!events.iter().any(|e| matches!(e, Ev::StreamOpened { .. })));
        assert_eq!(session.pending_opens.len(), 2);
        session.scan_capsules(&capsule(h3::WT_MAX_STREAMS_UNI_CAPSULE, 1), &mut events);
        session.flush(&mut server, &mut events);
        let id = events
            .iter()
            .find_map(|e| match e {
                Ev::StreamOpened { request_id: 11, id } => Some(*id),
                _ => None,
            })
            .unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e, Ev::StreamOpened { request_id: 12, .. })));
        session.stream_write(id, vec![7; 8], 13, &mut events);
        session.stream_fin(id);
        session.flush(&mut server, &mut events);
        crate::test_support::exchange(&mut client, &mut server);
        let mut buf = [0; 100];
        let (n, fin) = client.stream_recv(id, &mut buf).unwrap();
        assert_eq!(n, 3); // stream type and session ID, no payload
        assert!(!fin);
        assert_eq!(session.fc_data_sent, 0);
        session.scan_capsules(&capsule(h3::WT_MAX_DATA_CAPSULE, 3), &mut events);
        session.flush(&mut server, &mut events);
        crate::test_support::exchange(&mut client, &mut server);
        let (n, fin) = client.stream_recv(id, &mut buf).unwrap();
        assert_eq!(&buf[..n], &[7; 3]);
        assert!(!fin);
        assert!(!events
            .iter()
            .any(|e| matches!(e, Ev::WriteAck { request_id: 13 })));
        session.scan_capsules(&capsule(h3::WT_MAX_DATA_CAPSULE, 8), &mut events);
        session.scan_capsules(&capsule(h3::WT_MAX_STREAMS_BIDI_CAPSULE, 1), &mut events);
        session.flush(&mut server, &mut events);
        crate::test_support::exchange(&mut client, &mut server);
        let (n, fin) = client.stream_recv(id, &mut buf).unwrap();
        assert_eq!(&buf[..n], &[7; 5]);
        assert!(fin);
        assert_eq!(session.fc_data_sent, 8);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, Ev::WriteAck { request_id: 13 }))
                .count(),
            1
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, Ev::StreamOpened { request_id: 12, .. })));
    }

    #[test]
    fn session_credit_rejects_non_increasing_oversized_and_truncated_values() {
        for bytes in [
            capsule(h3::WT_MAX_DATA_CAPSULE, 0),
            capsule(h3::WT_MAX_STREAMS_UNI_CAPSULE, (1 << 60) + 1),
            capsule(h3::WT_STREAMS_BLOCKED_BIDI_CAPSULE, (1 << 60) + 1),
            vec![0x99, 0x0b, 0x4d, 0x3d, 1, 0x40],
        ] {
            let mut session = client_with_connect();
            session.scan_capsules(&bytes, &mut Vec::new());
            assert!(session.closed);
        }
    }

    #[test]
    fn critical_streams_reject_fin_duplicates_and_settings_order_errors() {
        for (first, second, fin) in [
            (h3::control_stream_prefix(), None, true),
            (
                h3::control_stream_prefix(),
                Some(h3::frame(h3::FRAME_SETTINGS, &h3::settings_payload())),
                false,
            ),
            (vec![0, 0, 0], None, false),
            (vec![2], None, true),
            (vec![3], None, true),
        ] {
            let mut session = server_with_connect(ParserMemory::new(4096));
            session.streams.insert(
                2,
                Stream::new(Role::PendingUni, session.parser_memory.clone()),
            );
            let mut conn = connection();
            let mut events = Vec::new();
            session.process_recv(&mut conn, 2, first, fin, &mut events);
            if let Some(second) = second {
                session.process_recv(&mut conn, 2, second, false, &mut events);
            }
            assert!(session.closed);
            assert!(events.iter().any(|e| matches!(e, Ev::Error(_))));
        }
        let mut session = server_with_connect(ParserMemory::new(4096));
        let mut conn = connection();
        for id in [2, 6] {
            session.streams.insert(
                id,
                Stream::new(Role::PendingUni, session.parser_memory.clone()),
            );
            session.process_recv(
                &mut conn,
                id,
                h3::control_stream_prefix(),
                false,
                &mut Vec::new(),
            );
        }
        assert!(session.closed);
    }

    #[test]
    fn peer_reset_of_critical_stream_is_terminal() {
        let (mut client, mut server, mut session) = established_server(h3::settings_payload());
        client
            .stream_shutdown(2, quiche::Shutdown::Write, 0)
            .unwrap();
        crate::test_support::exchange(&mut client, &mut server);
        let mut events = Vec::new();
        session.on_readable(&mut server, 2, false, &mut events);
        assert!(session.closed);
        assert!(events
            .iter()
            .any(|e| matches!(e, Ev::Error(message) if message.contains("critical"))));
    }

    #[test]
    fn unoffered_response_protocol_rejects_the_session() {
        let mut session = client_with_connect();
        session.ready = false;
        session.protocols = vec!["offered".into()];
        let frame = h3::encode_headers_frame(&[
            quiche::h3::Header::new(b":status", b"200"),
            quiche::h3::Header::new(h3::WT_PROTOCOL, b"\"unoffered\""),
        ])
        .unwrap();
        let mut events = Vec::new();
        session.process_recv(&mut connection(), CONNECT_ID, frame, false, &mut events);
        assert!(session.closed);
        assert!(!events.iter().any(|e| matches!(e, Ev::Ready { .. })));
    }

    #[test]
    fn complete_capsules_release_retained_storage() {
        let mut session = client_with_connect();
        let mut capsule = Vec::new();
        h3::put_varint(0x42, &mut capsule);
        h3::put_varint(512 * 1024, &mut capsule);
        capsule.resize(capsule.len() + 512 * 1024, 0);
        session.scan_capsules(&capsule, &mut Vec::new());
        assert_eq!(session.capsule_buf.capacity(), 0);
    }

    #[test]
    fn aggregate_incomplete_capsules_are_bounded() {
        let memory = ParserMemory::new(64 * 1024 * 1024);
        let mut sessions = Vec::new();
        let mut fragment = Vec::new();
        h3::put_varint(0x42, &mut fragment);
        h3::put_varint(MAX_CONTROL_FRAME as u64, &mut fragment);
        fragment.resize(512 * 1024, 0);
        for _ in 0..160 {
            let mut session = server_with_connect(memory.clone());
            session.scan_capsules(&fragment, &mut Vec::new());
            sessions.push(session);
        }
        let retained: usize = sessions.iter().map(|s| s.capsule_buf.capacity()).sum();
        assert!(
            retained <= 64 * 1024 * 1024,
            "retained {retained} parser bytes"
        );
        assert_eq!(memory.used(), retained);
        assert!(sessions.iter().any(WtSession::is_closed));
        assert!(sessions.iter().any(|s| !s.is_closed()));
        drop(sessions);
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn equal_byte_complete_capsules_do_not_accumulate_across_sessions() {
        let memory = ParserMemory::new(4 * 1024 * 1024);
        let mut sessions = Vec::new();
        let mut capsule = Vec::new();
        h3::put_varint(0x42, &mut capsule);
        h3::put_varint(512 * 1024 - 6, &mut capsule);
        capsule.resize(512 * 1024, 0);
        for _ in 0..160 {
            let mut session = server_with_connect(memory.clone());
            session.scan_capsules(&capsule, &mut Vec::new());
            assert!(!session.is_closed());
            assert_eq!(memory.used(), 0);
            sessions.push(session);
        }
    }

    fn connection() -> quiche::Connection {
        let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION).unwrap();
        config.set_application_protos(&[b"h3"]).unwrap();
        quiche::connect(
            Some("localhost"),
            &quiche::ConnectionId::from_ref(&[0; 16]),
            "127.0.0.1:1234".parse().unwrap(),
            "127.0.0.1:4433".parse().unwrap(),
            &mut config,
        )
        .unwrap()
    }

    #[test]
    fn fragmented_capsule_reassembles_across_data_frames_and_releases_budget() {
        let memory = ParserMemory::new(4096);
        let mut session = server_with_connect(memory.clone());
        let mut capsule = Vec::new();
        h3::put_varint(h3::WT_DRAIN_SESSION_CAPSULE, &mut capsule);
        h3::put_varint(0, &mut capsule);
        let mut conn = connection();
        let mut events = Vec::new();
        for byte in capsule {
            let frame = h3::frame(h3::FRAME_DATA, &[byte]);
            // Also split the HTTP/3 frame header across QUIC reads.
            for byte in frame {
                session.process_recv(&mut conn, CONNECT_ID, vec![byte], false, &mut events);
            }
        }
        assert_eq!(
            events.iter().filter(|e| matches!(e, Ev::Draining)).count(),
            1
        );
        assert!(!session.is_closed());
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn budget_failure_releases_all_offender_buffers_and_preserves_other_sessions() {
        let memory = ParserMemory::new(4096);
        let mut healthy = server_with_connect(memory.clone());
        healthy
            .streams
            .get_mut(&CONNECT_ID)
            .unwrap()
            .frames
            .push(&[0; 1000]);
        let mut offender = server_with_connect(memory.clone());
        let mut fragment = Vec::new();
        h3::put_varint(0x42, &mut fragment);
        h3::put_varint(MAX_CONTROL_FRAME as u64, &mut fragment);
        fragment.resize(1500, 0);
        offender.scan_capsules(&fragment, &mut Vec::new());
        offender
            .streams
            .get_mut(&CONNECT_ID)
            .unwrap()
            .class_buf
            .extend_from_slice(&[0; 500])
            .unwrap();
        assert_eq!(memory.used(), 1024 + 2048 + 512);
        let mut events = Vec::new();
        offender.scan_capsules(&[0; 2048], &mut events);
        assert!(offender.is_closed());
        assert!(events
            .iter()
            .any(|e| matches!(e, Ev::Error(message) if message.contains("memory limit"))));
        assert_eq!(memory.used(), 1024);
        assert!(!healthy.is_closed());
        let mut replacement = server_with_connect(memory.clone());
        replacement.scan_capsules(&[0x00, 0x00], &mut Vec::new());
        assert!(!replacement.is_closed());
        assert_eq!(memory.used(), 1024);
        healthy.mark_closed(&mut Vec::new(), 0, Vec::new(), false);
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn initial_control_chunk_budget_failure_is_terminal() {
        let memory = ParserMemory::new(64);
        let mut session = server_with_connect(memory.clone());
        session.streams.insert(
            2,
            Stream::new(Role::PendingUni, session.parser_memory.clone()),
        );
        let mut conn = connection();
        let mut events = Vec::new();
        session.process_recv(&mut conn, 2, vec![0; 65], false, &mut events);
        assert!(session.is_closed());
        assert_eq!(memory.used(), 0);
    }

    #[test]
    fn oversized_lengths_are_rejected_without_truncation() {
        let memory = ParserMemory::new(4096);
        let mut session = server_with_connect(memory.clone());
        let mut header = Vec::new();
        h3::put_varint(0x42, &mut header);
        h3::put_varint(1 << 40, &mut header);
        session.scan_capsules(&header, &mut Vec::new());
        assert!(session.is_closed());
        assert_eq!(memory.used(), 0);
        let mut frames = FrameParser::new(memory.clone());
        frames.push(&header);
        assert!(frames.next_frame().is_none());
        assert!(frames.error);
        drop(frames);
        assert_eq!(memory.used(), 0);
    }

    fn server_with_connect(memory: Arc<ParserMemory>) -> WtSession {
        let mut s = WtSession::new_server(
            "127.0.0.1:4433".parse().unwrap(),
            Vec::new(),
            None,
            Vec::new(),
            memory,
        );
        s.streams.insert(
            CONNECT_ID,
            Stream::new(Role::Connect, s.parser_memory.clone()),
        );
        s.ready = true;
        s.peer_settings = Some(h3::PeerSettings {
            enable_connect_protocol: true,
            h3_datagram: true,
            wt_max_sessions: 1,
            ..Default::default()
        });
        s
    }

    /// A minimal established-looking client session with a CONNECT stream (so
    /// queued session capsules have somewhere to go) and seeded peer settings.
    fn client_with_connect() -> WtSession {
        let mut s = WtSession::new_client("h".into(), "/".into(), None, Vec::new(), Vec::new());
        s.streams.insert(
            CONNECT_ID,
            Stream::new(Role::Connect, s.parser_memory.clone()),
        );
        s.peer_settings = Some(h3::PeerSettings {
            wt_max_sessions: 1,
            ..Default::default()
        });
        s.ready = true;
        s
    }

    fn connect_out_len(s: &WtSession) -> usize {
        s.streams
            .get(&CONNECT_ID)
            .map(|st| st.out.len())
            .unwrap_or(0)
    }

    #[test]
    fn emits_wt_data_blocked_once_per_limit() {
        let mut s = client_with_connect();
        s.fc_max_data_remote = 100;
        s.fc_data_sent = 100;
        let mut st = Stream::new(Role::WtData { bidi: true }, s.parser_memory.clone());
        st.out.push_back(OutChunk {
            data: vec![1, 2, 3],
            off: 0,
            request_id: Some(1),
        });
        s.streams.insert(4, st);

        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 1); // one WT_DATA_BLOCKED capsule queued
        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 1); // deduped: not re-queued at the same limit

        // A raised limit that we immediately re-exhaust re-arms the signal.
        s.fc_max_data_remote = 200;
        s.fc_data_sent = 200;
        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 2);
    }

    #[test]
    fn no_data_blocked_without_pending_payload() {
        let mut s = client_with_connect();
        s.fc_max_data_remote = 100;
        s.fc_data_sent = 100;
        // A WtData stream whose only queued chunk is the prefix (request_id None).
        let mut st = Stream::new(Role::WtData { bidi: false }, s.parser_memory.clone());
        st.out.push_back(OutChunk {
            data: vec![0x54],
            off: 0,
            request_id: None,
        });
        s.streams.insert(14, st);
        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 0);
    }

    #[test]
    fn emits_streams_blocked_when_over_limit() {
        let mut s = client_with_connect();
        s.fc_max_streams_bidi_remote = 1;
        // Simulate having opened 2 bidi WT streams (next_bidi advanced twice).
        s.next_bidi = CLIENT_FIRST_WT_BIDI_ID + 4;
        s.pending_opens.push_back((true, 99));
        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 1); // WT_STREAMS_BLOCKED_BIDI
        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 1); // deduped
    }

    #[test]
    fn no_signal_before_peer_settings() {
        let mut s = client_with_connect();
        s.peer_settings = None;
        s.fc_max_data_remote = 0;
        s.fc_data_sent = 0;
        s.next_bidi = CLIENT_FIRST_WT_BIDI_ID + 8;
        s.signal_flow_control_blocked();
        assert_eq!(connect_out_len(&s), 0);
    }
}
