// SPDX-License-Identifier: Apache-2.0
//! WebTransport client session state machine over a single quiche connection.
//!
//! One `WtSession` drives exactly one WebTransport session (one HTTP/3 Extended
//! CONNECT) over one QUIC connection. It owns no I/O — the [`crate::driver`]
//! event loop feeds it readable-stream / datagram / timer notifications and asks
//! it to flush. Everything the session wants to tell JS is returned as [`Ev`]s.
//!
//! Stream-id layout (client-initiated):
//! * bidi `0` — the CONNECT stream (this is also the WebTransport *session id*).
//! * uni `2`, `6`, `10` — HTTP/3 control, QPACK encoder, QPACK decoder.
//! * bidi `4, 8, …` — WebTransport bidirectional streams.
//! * uni `14, 18, …` — WebTransport unidirectional streams.

use std::collections::{HashMap, VecDeque};

use wtcore::h3;

fn dbg_on() -> bool {
    std::env::var("RWT_DEBUG").is_ok()
}

/// The session id / CONNECT stream id.
const CONNECT_ID: u64 = 0;
const LOCAL_CONTROL_ID: u64 = 2;
const LOCAL_QPACK_ENC_ID: u64 = 6;
const LOCAL_QPACK_DEC_ID: u64 = 10;
const FIRST_WT_BIDI_ID: u64 = 4;
const FIRST_WT_UNI_ID: u64 = 14;

/// Cap on a single buffered HTTP/3 control/CONNECT frame (protects against a
/// hostile peer advertising a huge length).
const MAX_CONTROL_FRAME: usize = 1 << 20;

/// Events the session emits for the driver to forward to JS.
#[derive(Debug)]
pub enum Ev {
    /// The WebTransport session is established (CONNECT got a 2xx).
    Ready,
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
    /// The peer's HTTP/3 control stream — parse SETTINGS/frames.
    PeerControl,
    /// A peer uni stream we drain and discard (QPACK/push/unknown).
    Ignored,
    /// A resolved WebTransport data stream.
    WtData { bidi: bool },
    /// A peer-initiated uni stream whose type varint isn't fully read yet.
    PendingServerUni,
    /// A peer-initiated bidi stream whose WT signal isn't fully read yet.
    PendingServerBidi,
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
    /// Peer already reset/finished — stop touching recv.
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
    authority: String,
    path: String,
    origin: Option<String>,
    extra_headers: Vec<(String, String)>,

    next_bidi: u64,
    next_uni: u64,

    setup_sent: bool,
    ready: bool,
    closed: bool,

    peer_settings: Option<h3::PeerSettings>,

    streams: HashMap<u64, Stream>,

    close_req: Option<CloseReq>,
    /// Set once the QUIC connection should be closed (after connect FIN flush).
    want_quic_close: Option<(u32, Vec<u8>)>,
}

impl WtSession {
    pub fn new(
        authority: String,
        path: String,
        origin: Option<String>,
        extra_headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            authority,
            path,
            origin,
            extra_headers,
            next_bidi: FIRST_WT_BIDI_ID,
            next_uni: FIRST_WT_UNI_ID,
            setup_sent: false,
            ready: false,
            closed: false,
            peer_settings: None,
            streams: HashMap::new(),
            close_req: None,
            want_quic_close: None,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    // ---- handshake -----------------------------------------------------------

    /// Called once the QUIC connection is established: open the HTTP/3
    /// control-plane streams, advertise SETTINGS, and send Extended CONNECT.
    pub fn on_established(&mut self, ev: &mut Vec<Ev>) {
        if self.setup_sent {
            return;
        }
        self.setup_sent = true;

        // HTTP/3 control stream: type prefix + SETTINGS.
        self.streams.insert(
            LOCAL_CONTROL_ID,
            Stream::with_prefix(Role::LocalControlPlane, h3::control_stream_prefix()),
        );
        // QPACK encoder / decoder streams (dynamic table disabled → type only).
        let mut enc = Vec::new();
        h3::put_varint(h3::QPACK_ENCODER_STREAM_TYPE, &mut enc);
        self.streams.insert(
            LOCAL_QPACK_ENC_ID,
            Stream::with_prefix(Role::LocalControlPlane, enc),
        );
        let mut dec = Vec::new();
        h3::put_varint(h3::QPACK_DECODER_STREAM_TYPE, &mut dec);
        self.streams.insert(
            LOCAL_QPACK_DEC_ID,
            Stream::with_prefix(Role::LocalControlPlane, dec),
        );

        // CONNECT request on the session (bidi 0) stream.
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
        if dbg_on() {
            eprintln!(
                "[rwt-session] on_established: queued setup, {} streams",
                self.streams.len()
            );
        }
    }

    // ---- reads ---------------------------------------------------------------

    /// Process a readable QUIC stream.
    pub fn on_readable(&mut self, conn: &mut quiche::Connection, id: u64, ev: &mut Vec<Ev>) {
        if !self.streams.contains_key(&id) {
            let role = classify_new(id);
            self.streams.insert(id, Stream::new(role));
        }

        // Respect read backpressure for resolved data streams.
        let (paused, is_data) = {
            let st = &self.streams[&id];
            (st.paused, matches!(st.role, Role::WtData { .. }))
        };
        if paused && is_data {
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
            if let Some(st) = self.streams.get_mut(&id) {
                st.recv_dead = true;
                if matches!(st.role, Role::WtData { .. }) {
                    ev.push(Ev::StreamReset { id, code });
                }
            }
            return;
        }

        if dbg_on() {
            let role = self.streams.get(&id).map(|s| role_name(s.role)).unwrap_or("?");
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
            Some(Role::LocalControlPlane) | Some(Role::Ignored) | None => {
                // discard
            }
            Some(Role::PeerControl) => {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.frames.push(&chunk);
                }
                self.parse_control_frames(id);
            }
            Some(Role::Connect) => {
                if let Some(st) = self.streams.get_mut(&id) {
                    st.frames.push(&chunk);
                }
                self.parse_connect_frames(conn, id, ev);
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
            Some(Role::PendingServerUni) => {
                self.resolve_pending_uni(conn, id, chunk, fin, ev);
            }
            Some(Role::PendingServerBidi) => {
                self.resolve_pending_bidi(conn, id, chunk, fin, ev);
            }
        }
    }

    fn parse_control_frames(&mut self, id: u64) {
        loop {
            let frame = self.streams.get_mut(&id).and_then(|s| s.frames.next_frame());
            let Some((ty, payload)) = frame else { break };
            if ty == h3::FRAME_SETTINGS {
                self.peer_settings = Some(h3::parse_settings(&payload));
            }
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

    fn scan_capsules(&mut self, mut buf: &[u8], ev: &mut Vec<Ev>) {
        while !buf.is_empty() {
            let Some((ty, n1)) = h3::read_varint(buf) else {
                break;
            };
            let Some((len, n2)) = h3::read_varint(&buf[n1..]) else {
                break;
            };
            let len = len as usize;
            let start = n1 + n2;
            if buf.len() < start + len {
                break;
            }
            let value = &buf[start..start + len];
            if ty == h3::WT_CLOSE_SESSION_CAPSULE && !self.closed {
                let (code, reason) = if value.len() >= 4 {
                    (
                        u32::from_be_bytes([value[0], value[1], value[2], value[3]]),
                        value[4..].to_vec(),
                    )
                } else {
                    (0, Vec::new())
                };
                self.mark_closed(ev, code, reason, true);
            }
            buf = &buf[start + len..];
        }
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
            let st = self.streams.get_mut(&id).unwrap();
            st.class_buf.extend_from_slice(&chunk);
            st.class_buf.clone()
        };
        let Some((ty, n)) = h3::read_varint(&buf) else {
            return; // need more bytes for the type varint
        };
        match ty {
            t if t == h3::H3_CONTROL_STREAM_TYPE => {
                let rest = buf[n..].to_vec();
                let st = self.streams.get_mut(&id).unwrap();
                st.role = Role::PeerControl;
                st.class_buf.clear();
                st.frames.push(&rest);
                self.parse_control_frames(id);
            }
            t if t == h3::QPACK_ENCODER_STREAM_TYPE || t == h3::QPACK_DECODER_STREAM_TYPE => {
                let st = self.streams.get_mut(&id).unwrap();
                st.role = Role::Ignored;
                st.class_buf.clear();
            }
            t if t == h3::WT_UNI_STREAM_TYPE => {
                let Some((session, n2)) = h3::read_varint(&buf[n..]) else {
                    return; // need session id
                };
                if session != CONNECT_ID {
                    let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
                    let st = self.streams.get_mut(&id).unwrap();
                    st.role = Role::Ignored;
                    st.class_buf.clear();
                    return;
                }
                let data = buf[n + n2..].to_vec();
                {
                    let st = self.streams.get_mut(&id).unwrap();
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
                let st = self.streams.get_mut(&id).unwrap();
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
            let st = self.streams.get_mut(&id).unwrap();
            st.class_buf.extend_from_slice(&chunk);
            st.class_buf.clone()
        };
        let Some((signal, n)) = h3::read_varint(&buf) else {
            return;
        };
        if signal != h3::WT_BIDI_FRAME_TYPE {
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
            let st = self.streams.get_mut(&id).unwrap();
            st.role = Role::Ignored;
            st.class_buf.clear();
            return;
        }
        let Some((session, n2)) = h3::read_varint(&buf[n..]) else {
            return;
        };
        if session != CONNECT_ID {
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, 0);
            let st = self.streams.get_mut(&id).unwrap();
            st.role = Role::Ignored;
            st.class_buf.clear();
            return;
        }
        let data = buf[n + n2..].to_vec();
        {
            let st = self.streams.get_mut(&id).unwrap();
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

    // ---- datagrams -----------------------------------------------------------

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

    // ---- application commands -------------------------------------------------

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
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Write, code);
            if let Some(st) = self.streams.get_mut(&id) {
                st.out.clear();
                st.fin_sent = true;
            }
        }
    }

    pub fn stream_stop_sending(&mut self, conn: &mut quiche::Connection, id: u64, code: u64) {
        if self.streams.contains_key(&id) {
            let _ = conn.stream_shutdown(id, quiche::Shutdown::Read, code);
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
                reason,
                capsule_queued: false,
            });
        }
    }

    // ---- flush ---------------------------------------------------------------

    /// Flush pending outbound stream data and drive the graceful-close sequence.
    /// Returns an optional `(code, reason)` if the QUIC connection should now be
    /// closed.
    pub fn flush(&mut self, conn: &mut quiche::Connection, ev: &mut Vec<Ev>) {
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
                    ev.push(Ev::StreamStopSending { id, code });
                    st.out.clear();
                    st.fin_sent = true;
                    return;
                }
                Err(_) => return,
            }
        }

        if st.out.is_empty() && st.fin_queued && !st.fin_sent {
            if conn.stream_send(id, &[], true).is_ok() {
                st.fin_sent = true;
            }
        }
    }

    // ---- lifecycle -----------------------------------------------------------

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
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::LocalControlPlane => "LocalControlPlane",
        Role::Connect => "Connect",
        Role::PeerControl => "PeerControl",
        Role::Ignored => "Ignored",
        Role::WtData { .. } => "WtData",
        Role::PendingServerUni => "PendingServerUni",
        Role::PendingServerBidi => "PendingServerBidi",
    }
}

/// Classify a not-yet-seen stream id by its QUIC stream-type bits.
fn classify_new(id: u64) -> Role {
    match id & 0x3 {
        0x3 => Role::PendingServerUni,  // server-initiated unidirectional
        0x1 => Role::PendingServerBidi, // server-initiated bidirectional
        _ => Role::Ignored,             // an unexpected client-initiated id
    }
}
