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
    Ready,
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
    /// A resolved WebTransport data stream.
    WtData { bidi: bool },
    /// A peer-initiated uni stream whose type varint isn't fully read yet.
    PendingUni,
    /// A peer-initiated bidi stream whose WT signal isn't fully read yet.
    PendingBidi,
}

struct Stream {
    role: Role,
    /// Classification accumulator (pending server streams) / control byte carry.
    class_buf: Vec<u8>,
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
}

impl Stream {
    fn new(role: Role) -> Self {
        Self {
            role,
            class_buf: Vec::new(),
            frames: FrameParser::default(),
            headers_done: false,
            paused: false,
            out: VecDeque::new(),
            fin_queued: false,
            fin_sent: false,
            recv_dead: false,
        }
    }

    fn with_prefix(role: Role, prefix: Vec<u8>) -> Self {
        let mut s = Self::new(role);
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
#[derive(Default)]
struct FrameParser {
    buf: Vec<u8>,
    error: bool,
}

impl FrameParser {
    fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pop the next complete `(type, payload)` frame, or `None` if incomplete.
    fn next_frame(&mut self) -> Option<(u64, Vec<u8>)> {
        if self.error {
            return None;
        }
        let (ty, n1) = h3::read_varint(&self.buf)?;
        let (len, n2) = h3::read_varint(&self.buf[n1..])?;
        let len = len as usize;
        if len > MAX_CONTROL_FRAME {
            self.error = true;
            return None;
        }
        let header = n1 + n2;
        let total = header + len;
        if self.buf.len() < total {
            return None;
        }
        let payload = self.buf[header..total].to_vec();
        self.buf.drain(..total);
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

    next_bidi: u64,
    next_uni: u64,

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
    /// capsules). Parsed and stored; QUIC performs the actual enforcement, so
    /// these are informational under the one-session-per-connection model.
    #[allow(dead_code)]
    fc_max_data_remote: u64,
    #[allow(dead_code)]
    fc_max_streams_bidi_remote: u64,
    #[allow(dead_code)]
    fc_max_streams_uni_remote: u64,
    /// Reassembly buffer for capsules on the session (CONNECT) stream: RFC 9297
    /// lets a capsule span multiple HTTP/3 DATA frames, so bytes are accumulated
    /// here and only whole capsules are consumed.
    capsule_buf: Vec<u8>,

    peer_settings: Option<h3::PeerSettings>,

    streams: HashMap<u64, Stream>,

    close_req: Option<CloseReq>,
    /// Set once the QUIC connection should be closed (after connect FIN flush).
    want_quic_close: Option<(u32, Vec<u8>)>,
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
    ) -> Self {
        Self {
            is_server: false,
            authority,
            path,
            origin,
            extra_headers,
            next_bidi: CLIENT_FIRST_WT_BIDI_ID,
            next_uni: CLIENT_FIRST_WT_UNI_ID,
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
            capsule_buf: Vec::new(),
            peer_settings: None,
            streams: HashMap::new(),
            close_req: None,
            want_quic_close: None,
            remote_addr: None,
        }
    }

    /// A server-role session that will accept an incoming Extended CONNECT.
    /// `remote` is the client's address, surfaced to the application via the
    /// `serverReady` event.
    pub fn new_server(remote: std::net::SocketAddr) -> Self {
        Self {
            is_server: true,
            authority: String::new(),
            path: String::new(),
            origin: None,
            extra_headers: Vec::new(),
            next_bidi: SERVER_FIRST_WT_BIDI_ID,
            next_uni: SERVER_FIRST_WT_UNI_ID,
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
            capsule_buf: Vec::new(),
            peer_settings: None,
            streams: HashMap::new(),
            close_req: None,
            want_quic_close: None,
            remote_addr: Some(remote),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Called once the QUIC connection is established: open the HTTP/3
    /// control-plane streams and advertise SETTINGS. A client also sends the
    /// Extended CONNECT immediately; a server waits for the client's CONNECT on
    /// bidi stream 0.
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
            Stream::with_prefix(Role::LocalControlPlane, h3::control_stream_prefix()),
        );
        // QPACK encoder / decoder streams (dynamic table disabled → type only).
        let mut enc = Vec::new();
        h3::put_varint(h3::QPACK_ENCODER_STREAM_TYPE, &mut enc);
        self.streams.insert(
            qpack_enc_id,
            Stream::with_prefix(Role::LocalControlPlane, enc),
        );
        let mut dec = Vec::new();
        h3::put_varint(h3::QPACK_DECODER_STREAM_TYPE, &mut dec);
        self.streams.insert(
            qpack_dec_id,
            Stream::with_prefix(Role::LocalControlPlane, dec),
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
        let is_server = self.is_server;
        self.streams
            .entry(id)
            .or_insert_with(|| Stream::new(classify_new(id, is_server)));

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

        // Drain everything quiche currently has for this stream.
        let mut chunk = Vec::new();
        let mut fin = false;
        let mut reset: Option<u64> = None;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match conn.stream_recv(id, &mut buf) {
                Ok((n, f)) => {
                    chunk.extend_from_slice(&buf[..n]);
                    if f {
                        fin = true;
                        break;
                    }
                    if n == 0 {
                        break;
                    }
                }
                Err(quiche::Error::Done) => break,
                Err(quiche::Error::StreamReset(code)) => {
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

        if dbg_on() {
            let role = self
                .streams
                .get(&id)
                .map(|s| role_name(s.role))
                .unwrap_or("?");
            eprintln!(
                "[rwt-session] readable id={id} role={role} chunk={} fin={fin}",
                chunk.len()
            );
        }
        self.process_recv(conn, id, chunk, fin, ev);
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
        match role {
            Some(Role::LocalControlPlane) | None => {
                // Our send-only control-plane streams / unknown: discard reads.
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
                self.parse_control_frames(id, ev);
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
                    self.mark_closed(ev, 0, Vec::new(), true);
                }
            }
            Some(Role::WtData { .. }) => {
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

    fn parse_control_frames(&mut self, id: u64, ev: &mut Vec<Ev>) {
        loop {
            let frame = self
                .streams
                .get_mut(&id)
                .and_then(|s| s.frames.next_frame());
            let Some((ty, payload)) = frame else { break };
            if ty == h3::FRAME_SETTINGS {
                let settings = h3::parse_settings(&payload);
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
        );
        match h3::encode_headers_frame(&headers) {
            Ok(frame) => {
                self.streams
                    .insert(CONNECT_ID, Stream::with_prefix(Role::Connect, frame));
            }
            Err(e) => ev.push(Ev::Error(format!("failed to encode CONNECT: {e}"))),
        }
    }

    fn parse_connect_frames(&mut self, conn: &mut quiche::Connection, id: u64, ev: &mut Vec<Ev>) {
        loop {
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
                                self.ready = true;
                                ev.push(Ev::Ready);
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
        self.capsule_buf.extend_from_slice(payload);
        loop {
            let (ty, start, len) = {
                let buf = &self.capsule_buf;
                let Some((ty, n1)) = h3::read_varint(buf) else {
                    break;
                };
                let Some((len, n2)) = h3::read_varint(&buf[n1..]) else {
                    break;
                };
                (ty, n1 + n2, len as usize)
            };
            // Guard against a hostile/oversized declared length.
            if len > MAX_CONTROL_FRAME {
                self.capsule_buf.clear();
                break;
            }
            if self.capsule_buf.len() < start + len {
                break; // capsule not fully arrived; wait for the next DATA frame
            }
            let value: Vec<u8> = self.capsule_buf[start..start + len].to_vec();
            self.capsule_buf.drain(..start + len);

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
            } else if ty == h3::WT_MAX_DATA_CAPSULE {
                if let Some((v, _)) = h3::read_varint(&value) {
                    self.fc_max_data_remote = self.fc_max_data_remote.max(v);
                }
            } else if ty == h3::WT_MAX_STREAMS_BIDI_CAPSULE {
                if let Some((v, _)) = h3::read_varint(&value) {
                    self.fc_max_streams_bidi_remote = self.fc_max_streams_bidi_remote.max(v);
                }
            } else if ty == h3::WT_MAX_STREAMS_UNI_CAPSULE {
                if let Some((v, _)) = h3::read_varint(&value) {
                    self.fc_max_streams_uni_remote = self.fc_max_streams_uni_remote.max(v);
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
        loop {
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
                _ if name.starts_with(b":") => {} // other pseudo-headers
                _ => extra.push((
                    String::from_utf8_lossy(name).into_owned(),
                    String::from_utf8_lossy(value).into_owned(),
                )),
            }
        }

        let ok = method == Some(b"CONNECT".as_ref()) && protocol == Some(b"webtransport".as_ref());
        if !ok {
            self.reject_connect(conn, id, 400, ev);
            return;
        }

        let resp = vec![quiche::h3::Header::new(b":status", b"200")];
        match h3::encode_headers_frame(&resp) {
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
            st.class_buf.extend_from_slice(&chunk);
            st.class_buf.clone()
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
                let rest = buf[n..].to_vec();
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                st.role = Role::PeerControl;
                st.class_buf.clear();
                st.frames.push(&rest);
                self.parse_control_frames(id, ev);
            }
            t if t == h3::QPACK_ENCODER_STREAM_TYPE || t == h3::QPACK_DECODER_STREAM_TYPE => {
                let Some(st) = self.streams.get_mut(&id) else {
                    return;
                };
                st.role = Role::Ignored;
                st.class_buf.clear();
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
            st.class_buf.extend_from_slice(&chunk);
            st.class_buf.clone()
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
        self.streams
            .insert(id, Stream::with_prefix(Role::WtData { bidi }, prefix));
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
        // Once established, grant the peer ample per-session flow-control credit.
        // Exactly one WebTransport session rides this QUIC connection, so QUIC's
        // connection-level flow control is the real limit; granting a large WT
        // credit here keeps a strict peer that honors our advertised session
        // limits from stalling, while QUIC does the actual enforcement.
        if self.ready && !self.fc_credit_granted && !self.closed {
            self.fc_credit_granted = true;
            self.queue_session_capsule(h3::WT_MAX_DATA_CAPSULE, WT_GRANT_MAX_DATA);
            self.queue_session_capsule(h3::WT_MAX_STREAMS_BIDI_CAPSULE, WT_GRANT_MAX_STREAMS);
            self.queue_session_capsule(h3::WT_MAX_STREAMS_UNI_CAPSULE, WT_GRANT_MAX_STREAMS);
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

        let ids: Vec<u64> = self.streams.keys().copied().collect();
        for id in ids {
            self.flush_stream(conn, id, ev);
        }

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

        // If a graceful close finished flushing the CONNECT stream, close QUIC.
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

        if let Some((code, reason)) = self.want_quic_close.take() {
            let _ = conn.close(true, code as u64, &reason);
        }
    }

    fn flush_stream(&mut self, conn: &mut quiche::Connection, id: u64, ev: &mut Vec<Ev>) {
        let Some(st) = self.streams.get_mut(&id) else {
            return;
        };
        // Note: we call `stream_send` directly rather than pre-checking
        // `stream_capacity`, because a stream does not exist in quiche until its
        // first `stream_send`, and `stream_capacity` errors (InvalidStreamState)
        // on a not-yet-created stream.
        while let Some(front) = st.out.front_mut() {
            let remaining_len = front.data.len() - front.off;
            match conn.stream_send(id, &front.data[front.off..], false) {
                Ok(sent) => {
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
        ev.push(Ev::Closed {
            code,
            reason,
            remote,
        });
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Whether a stream's HTTP/3 frame parser has latched a fatal error.
    fn frame_error(&self, id: u64) -> bool {
        self.streams
            .get(&id)
            .map(|s| s.frames.error)
            .unwrap_or(false)
    }

    /// Stop reading a stream and release its receive buffers (used when a stream
    /// is finished, reset, or has produced a fatal protocol error), so a hostile
    /// server cannot keep us draining it into an unbounded buffer.
    fn kill_recv(&mut self, conn: &mut quiche::Connection, id: u64) {
        if let Some(st) = self.streams.get_mut(&id) {
            st.recv_dead = true;
            st.frames.buf = Vec::new();
            st.class_buf = Vec::new();
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
