// SPDX-License-Identifier: Apache-2.0
//! WebTransport **echo** server — a test fixture for the rwebtransport client.
//!
//! It speaks real QUIC/HTTP-3/WebTransport (Cloudflare quiche + BoringSSL,
//! server role) and echoes everything a client sends:
//! * bidirectional streams — bytes are written straight back on the same stream;
//! * unidirectional streams — the server opens a matching uni stream and echoes;
//! * datagrams — sent back verbatim.
//!
//! Usage: `wt-echo-server --cert <pem> --key <pem> [--host 127.0.0.1] [--port 0]`.
//! On startup it prints two machine-readable lines to stdout:
//! `PORT <n>` (the actually-bound UDP port) and `READY`.

use std::collections::HashMap;
use std::net::SocketAddr;

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use wtcore::h3;

const LOCAL_CONN_ID_LEN: usize = 16;
const SOCKET: Token = Token(0);

/// Server behaviour, selectable via `--mode` for adversarial client tests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Echo everything (default).
    Echo,
    /// Reject the Extended CONNECT with `:status 404`.
    Reject,
    /// Answer CONNECT with a HEADERS frame carrying a malformed QPACK block.
    MalformedHeaders,
    /// After a normal CONNECT, dump random non-frame bytes on the CONNECT stream.
    Garbage,
    /// Echo bidi streams but RESET them instead of finishing cleanly.
    ResetStreams,
    /// Accept CONNECT, then immediately close the WebTransport session (FIN).
    CloseSession,
}

impl Mode {
    fn parse(s: &str) -> Mode {
        match s {
            "reject" => Mode::Reject,
            "malformed-headers" => Mode::MalformedHeaders,
            "garbage" => Mode::Garbage,
            "reset" => Mode::ResetStreams,
            "close" => Mode::CloseSession,
            _ => Mode::Echo,
        }
    }
}

// ---- server-side WebTransport session state --------------------------------

const CONNECT_ID: u64 = 0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    LocalControlPlane,
    PeerControlPlane, // client control/qpack uni streams — drain
    Connect,          // client CONNECT bidi (id 0)
    WtBidi,           // client bidi WT stream — echo on the same stream
    WtUniIn,          // client uni WT stream — echo via a new server uni stream
    PendingUni,       // client uni, type varint not yet read
    Ignored,
}

struct Stream {
    role: Role,
    class_buf: Vec<u8>,
    frames: FrameBuf,
    out: Vec<u8>,
    out_off: usize,
    fin_queued: bool,
    fin_sent: bool,
    prefix_done: bool,
    reset_queued: bool,
}

impl Stream {
    fn new(role: Role) -> Self {
        Self {
            role,
            class_buf: Vec::new(),
            frames: FrameBuf::default(),
            out: Vec::new(),
            out_off: 0,
            fin_queued: false,
            fin_sent: false,
            prefix_done: false,
            reset_queued: false,
        }
    }
    fn with_prefix(role: Role, prefix: Vec<u8>) -> Self {
        let mut s = Self::new(role);
        s.out = prefix;
        s
    }
    fn queue(&mut self, data: &[u8]) {
        self.out.extend_from_slice(data);
    }
}

#[derive(Default)]
struct FrameBuf {
    buf: Vec<u8>,
}
impl FrameBuf {
    fn push(&mut self, d: &[u8]) {
        self.buf.extend_from_slice(d);
    }
    fn next(&mut self) -> Option<(u64, Vec<u8>)> {
        let (ty, n1) = h3::read_varint(&self.buf)?;
        let (len, n2) = h3::read_varint(&self.buf[n1..])?;
        let len = len as usize;
        if len > (1 << 20) {
            return None;
        }
        let total = n1 + n2 + len;
        if self.buf.len() < total {
            return None;
        }
        let payload = self.buf[n1 + n2..total].to_vec();
        self.buf.drain(..total);
        Some((ty, payload))
    }
}

struct Server {
    conn: quiche::Connection,
    streams: HashMap<u64, Stream>,
    setup_done: bool,
    connected: bool,
    next_server_uni: u64,
    established_logged: bool,
    mode: Mode,
    adversary_done: bool,
}

impl Server {
    fn new(conn: quiche::Connection, mode: Mode) -> Self {
        Self {
            conn,
            streams: HashMap::new(),
            setup_done: false,
            connected: false,
            next_server_uni: 15, // 3/7/11 are control/qpack; WT uni starts at 15
            established_logged: false,
            mode,
            adversary_done: false,
        }
    }

    fn on_established(&mut self) {
        if self.setup_done {
            return;
        }
        self.setup_done = true;
        self.streams.insert(
            3,
            Stream::with_prefix(Role::LocalControlPlane, h3::control_stream_prefix()),
        );
        let mut enc = Vec::new();
        h3::put_varint(h3::QPACK_ENCODER_STREAM_TYPE, &mut enc);
        self.streams
            .insert(7, Stream::with_prefix(Role::LocalControlPlane, enc));
        let mut dec = Vec::new();
        h3::put_varint(h3::QPACK_DECODER_STREAM_TYPE, &mut dec);
        self.streams
            .insert(11, Stream::with_prefix(Role::LocalControlPlane, dec));
    }

    fn drive(&mut self) {
        if self.conn.is_established() {
            self.on_established();
        }
        // Datagrams: echo back verbatim.
        let mut dbuf = [0u8; 65_536];
        while let Ok(n) = self.conn.dgram_recv(&mut dbuf) {
            let data = dbuf[..n].to_vec();
            let _ = self.conn.dgram_send(&data);
        }
        // Readable streams.
        let readable: Vec<u64> = self.conn.readable().collect();
        if std::env::var("RWT_DEBUG").is_ok() && !readable.is_empty() {
            eprintln!("[echo] readable streams: {readable:?}");
        }
        for id in readable {
            self.on_readable(id);
        }
        self.adversary();
        self.flush();
    }

    fn on_readable(&mut self, id: u64) {
        self.streams.entry(id).or_insert_with(|| Stream::new(classify(id)));
        let mut chunk = Vec::new();
        let mut fin = false;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.conn.stream_recv(id, &mut buf) {
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
                Err(_) => break,
            }
        }
        if std::env::var("RWT_DEBUG").is_ok() {
            eprintln!("[echo] readable id={id} chunk={} fin={fin}", chunk.len());
        }
        self.process(id, chunk, fin);
    }

    fn process(&mut self, id: u64, chunk: Vec<u8>, fin: bool) {
        let role = self.streams.get(&id).map(|s| s.role);
        match role {
            Some(Role::PeerControlPlane)
            | Some(Role::LocalControlPlane)
            | Some(Role::Ignored)
            | None => {}
            Some(Role::Connect) => {
                if let Some(s) = self.streams.get_mut(&id) {
                    s.frames.push(&chunk);
                }
                self.handle_connect(id);
            }
            Some(Role::WtBidi) => {
                // Strip the 0x41 + session prefix once, then echo the rest.
                let data = self.strip_prefix(id, chunk, h3::WT_BIDI_FRAME_TYPE);
                let reset = self.mode == Mode::ResetStreams;
                if let Some(s) = self.streams.get_mut(&id) {
                    if !data.is_empty() {
                        s.queue(&data);
                    }
                    if fin {
                        if reset {
                            s.reset_queued = true;
                        } else {
                            s.fin_queued = true;
                        }
                    }
                }
            }
            Some(Role::WtUniIn) => {
                let data = self.strip_prefix(id, chunk, h3::WT_UNI_STREAM_TYPE);
                if !data.is_empty() || fin {
                    self.echo_uni(&data, fin);
                }
            }
            Some(Role::PendingUni) => {
                self.resolve_uni(id, chunk, fin);
            }
        }
    }

    /// Consume the `signal + session-id` prefix on a WT stream the first time,
    /// returning the application bytes that follow.
    fn strip_prefix(&mut self, id: u64, chunk: Vec<u8>, _signal: u64) -> Vec<u8> {
        let s = self.streams.get_mut(&id).unwrap();
        if s.prefix_done {
            return chunk;
        }
        s.class_buf.extend_from_slice(&chunk);
        let buf = s.class_buf.clone();
        let Some((_sig, n1)) = h3::read_varint(&buf) else {
            return Vec::new();
        };
        let Some((_sess, n2)) = h3::read_varint(&buf[n1..]) else {
            return Vec::new();
        };
        s.prefix_done = true;
        let rest = buf[n1 + n2..].to_vec();
        s.class_buf.clear();
        rest
    }

    fn resolve_uni(&mut self, id: u64, chunk: Vec<u8>, fin: bool) {
        let buf = {
            let s = self.streams.get_mut(&id).unwrap();
            s.class_buf.extend_from_slice(&chunk);
            s.class_buf.clone()
        };
        let Some((ty, n)) = h3::read_varint(&buf) else {
            return;
        };
        match ty {
            t if t == h3::H3_CONTROL_STREAM_TYPE
                || t == h3::QPACK_ENCODER_STREAM_TYPE
                || t == h3::QPACK_DECODER_STREAM_TYPE =>
            {
                let s = self.streams.get_mut(&id).unwrap();
                s.role = Role::PeerControlPlane;
                s.class_buf.clear();
            }
            t if t == h3::WT_UNI_STREAM_TYPE => {
                // Read session id, then treat remaining as echo payload.
                let Some((_sess, n2)) = h3::read_varint(&buf[n..]) else {
                    return;
                };
                let data = buf[n + n2..].to_vec();
                {
                    let s = self.streams.get_mut(&id).unwrap();
                    s.role = Role::WtUniIn;
                    s.prefix_done = true;
                    s.class_buf.clear();
                }
                if !data.is_empty() || fin {
                    self.echo_uni(&data, fin);
                }
            }
            _ => {
                let s = self.streams.get_mut(&id).unwrap();
                s.role = Role::Ignored;
                s.class_buf.clear();
            }
        }
    }

    fn echo_uni(&mut self, data: &[u8], fin: bool) {
        // Open (or reuse) a server uni stream carrying the echoed bytes.
        let id = self.next_server_uni;
        self.streams.entry(id).or_insert_with(|| {
            let mut prefix = Vec::new();
            h3::put_varint(h3::WT_UNI_STREAM_TYPE, &mut prefix);
            h3::put_varint(CONNECT_ID, &mut prefix);
            Stream::with_prefix(Role::LocalControlPlane, prefix)
        });
        let s = self.streams.get_mut(&id).unwrap();
        s.queue(data);
        if fin {
            s.fin_queued = true;
            self.next_server_uni += 4; // finish this uni stream, next echo uses a new one
        }
    }

    fn handle_connect(&mut self, id: u64) {
        loop {
            let frame = self.streams.get_mut(&id).and_then(|s| s.frames.next());
            let Some((ty, payload)) = frame else { break };
            if std::env::var("RWT_DEBUG").is_ok() {
                eprintln!(
                    "[echo] connect stream {id} frame ty={ty} len={}",
                    payload.len()
                );
            }
            if ty == h3::FRAME_HEADERS {
                let valid = match h3::decode_header_block(&payload) {
                    Ok(headers) => headers
                        .iter()
                        .any(|hh| hh.name() == b":protocol" && hh.value() == b"webtransport"),
                    Err(_) => false,
                };

                // Adversarial: a deliberately malformed QPACK response block.
                if self.mode == Mode::MalformedHeaders {
                    let mut framed = Vec::new();
                    h3::put_varint(h3::FRAME_HEADERS, &mut framed);
                    let garbage = [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff];
                    h3::put_varint(garbage.len() as u64, &mut framed);
                    framed.extend_from_slice(&garbage);
                    if let Some(s) = self.streams.get_mut(&id) {
                        s.queue(&framed);
                        s.fin_queued = true;
                    }
                    self.connected = false;
                    continue;
                }

                let ok = valid && self.mode != Mode::Reject;
                let status: &[u8] = if ok { b"200" } else { b"404" };
                let resp = vec![quiche::h3::Header::new(b":status", status)];
                if let Ok(frame) = h3::encode_headers_frame(&resp) {
                    if let Some(s) = self.streams.get_mut(&id) {
                        s.queue(&frame);
                        if !ok {
                            s.fin_queued = true;
                        }
                    }
                }
                self.connected = ok;
            }
        }
    }

    fn flush(&mut self) {
        let ids: Vec<u64> = self.streams.keys().copied().collect();
        for id in ids {
            let s = self.streams.get_mut(&id).unwrap();
            // Call stream_send directly (a stream doesn't exist until its first
            // send, so stream_capacity would error on a not-yet-created stream).
            while s.out_off < s.out.len() {
                let remaining_len = s.out.len() - s.out_off;
                match self.conn.stream_send(id, &s.out[s.out_off..], false) {
                    Ok(sent) => {
                        s.out_off += sent;
                        if sent < remaining_len {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            if s.reset_queued && !s.fin_sent {
                let _ = self.conn.stream_shutdown(id, quiche::Shutdown::Write, 7);
                s.fin_sent = true;
            } else if s.out_off >= s.out.len() && s.fin_queued && !s.fin_sent
                && self.conn.stream_send(id, &[], true).is_ok() {
                    s.fin_sent = true;
                }
        }
    }

    /// Fire the mode-specific hostile action once, after CONNECT succeeds.
    fn adversary(&mut self) {
        if self.adversary_done || !self.connected {
            return;
        }
        match self.mode {
            Mode::Garbage => {
                // Dump non-frame bytes on the CONNECT (session) stream. The client
                // must survive this without crashing or hanging.
                let garbage = [
                    0xffu8, 0xff, 0xff, 0xff, 0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03,
                ];
                if let Some(s) = self.streams.get_mut(&CONNECT_ID) {
                    s.queue(&garbage);
                }
                self.adversary_done = true;
            }
            Mode::CloseSession => {
                // FIN the CONNECT stream: the WebTransport session ends.
                if let Some(s) = self.streams.get_mut(&CONNECT_ID) {
                    s.fin_queued = true;
                }
                self.adversary_done = true;
            }
            _ => {}
        }
    }
}

use quiche::h3::NameValue;

fn classify(id: u64) -> Role {
    match id & 0x3 {
        0x0 => {
            if id == CONNECT_ID {
                Role::Connect
            } else {
                Role::WtBidi
            }
        }
        0x2 => Role::PendingUni, // client-initiated uni
        _ => Role::Ignored,
    }
}

// ---- main / accept loop -----------------------------------------------------

fn main() {
    let mut cert = String::new();
    let mut key = String::new();
    let mut host = "127.0.0.1".to_string();
    let mut port: u16 = 0;
    let mut mode = Mode::Echo;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--cert" => cert = args.next().unwrap_or_default(),
            "--key" => key = args.next().unwrap_or_default(),
            "--host" => host = args.next().unwrap_or_else(|| "127.0.0.1".into()),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            "--mode" => mode = Mode::parse(&args.next().unwrap_or_default()),
            _ => {}
        }
    }
    if cert.is_empty() || key.is_empty() {
        eprintln!("usage: wt-echo-server --cert <pem> --key <pem> [--host H] [--port P]");
        std::process::exit(2);
    }

    let bind: SocketAddr = format!("{host}:{port}").parse().expect("bind addr");
    let mut socket = UdpSocket::bind(bind).expect("bind udp");
    let local_addr = socket.local_addr().expect("local addr");

    let mut config = build_server_config(&cert, &key).expect("server config");

    let mut poll = Poll::new().unwrap();
    poll.registry()
        .register(&mut socket, SOCKET, Interest::READABLE)
        .unwrap();
    let mut events = Events::with_capacity(1024);

    // Machine-readable handshake for the test harness.
    println!("PORT {}", local_addr.port());
    println!("READY");
    use std::io::Write;
    std::io::stdout().flush().ok();

    let debug = std::env::var("RWT_DEBUG").is_ok();
    let mut clients: HashMap<Vec<u8>, Server> = HashMap::new();
    let mut routing: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut recv_buf = vec![0u8; 65_535];
    let mut send_buf = vec![0u8; 1350];

    loop {
        let timeout = clients.values().filter_map(|c| c.conn.timeout()).min();
        poll.poll(&mut events, timeout).ok();

        // Read all datagrams.
        loop {
            let (len, from) = match socket.recv_from(&mut recv_buf) {
                Ok(v) => v,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            };
            let pkt = &mut recv_buf[..len];
            let hdr = match quiche::Header::from_slice(pkt, LOCAL_CONN_ID_LEN) {
                Ok(h) => h,
                Err(_) => continue,
            };

            let key_scid = routing.get(&hdr.dcid.to_vec()).cloned();
            let scid = match key_scid {
                Some(s) => s,
                None => {
                    if hdr.ty != quiche::Type::Initial {
                        continue;
                    }
                    let mut scid = [0u8; LOCAL_CONN_ID_LEN];
                    getrandom_fill(&mut scid);
                    let scid_id = quiche::ConnectionId::from_ref(&scid);
                    let conn = match quiche::accept(&scid_id, None, local_addr, from, &mut config) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    let key = scid.to_vec();
                    routing.insert(hdr.dcid.to_vec(), key.clone());
                    routing.insert(key.clone(), key.clone());
                    clients.insert(key.clone(), Server::new(conn, mode));
                    if debug {
                        eprintln!("[echo] new connection from {from} (type={:?})", hdr.ty);
                    }
                    key
                }
            };

            if let Some(server) = clients.get_mut(&scid) {
                let info = quiche::RecvInfo {
                    from,
                    to: local_addr,
                };
                match server.conn.recv(pkt, info) {
                    Ok(n) => {
                        if debug {
                            eprintln!("[echo] recv {len} bytes -> processed {n}");
                        }
                    }
                    Err(e) => {
                        if debug {
                            eprintln!("[echo] recv err {e:?}");
                        }
                    }
                }
            }
        }

        // Fire timeouts only for connections whose timer has actually elapsed.
        for server in clients.values_mut() {
            if server.conn.timeout() == Some(std::time::Duration::ZERO) {
                server.conn.on_timeout();
            }
        }

        // Drive each connection and flush.
        for server in clients.values_mut() {
            let est_before = server.established_logged;
            server.drive();
            if debug && !est_before && server.conn.is_established() {
                server.established_logged = true;
                eprintln!("[echo] connection established");
            }
            let mut sent = 0;
            loop {
                match server.conn.send(&mut send_buf) {
                    Ok((write, info)) => {
                        let _ = socket.send_to(&send_buf[..write], info.to);
                        sent += write;
                    }
                    Err(quiche::Error::Done) => break,
                    Err(_) => break,
                }
            }
            if debug && sent > 0 {
                eprintln!("[echo] sent {sent} bytes");
            }
        }

        // Reap closed connections.
        let before = clients.len();
        clients.retain(|_, s| !s.conn.is_closed());
        if debug && clients.len() < before {
            eprintln!("[echo] reaped a closed connection");
        }
        routing.retain(|_, scid| clients.contains_key(scid));
    }
}

fn build_server_config(cert: &str, key: &str) -> Result<quiche::Config, String> {
    use boring::ssl::{SslContextBuilder, SslMethod, SslVersion};
    let mut b = SslContextBuilder::new(SslMethod::tls()).map_err(|e| e.to_string())?;
    b.set_min_proto_version(Some(SslVersion::TLS1_3))
        .map_err(|e| e.to_string())?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))
        .map_err(|e| e.to_string())?;

    let mut config = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, b)
        .map_err(|e| format!("{e:?}"))?;
    config
        .load_cert_chain_from_pem_file(cert)
        .map_err(|e| format!("cert: {e:?}"))?;
    config
        .load_priv_key_from_pem_file(key)
        .map_err(|e| format!("key: {e:?}"))?;
    config
        .set_application_protos(&[b"h3"])
        .map_err(|e| format!("{e:?}"))?;
    config.set_max_idle_timeout(30_000);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(10 * 1024 * 1024);
    config.set_initial_max_stream_data_bidi_local(1024 * 1024);
    config.set_initial_max_stream_data_bidi_remote(1024 * 1024);
    config.set_initial_max_stream_data_uni(1024 * 1024);
    config.set_initial_max_streams_bidi(256);
    config.set_initial_max_streams_uni(256);
    config.set_disable_active_migration(true);
    config.enable_dgram(true, 65_536, 65_536);
    Ok(config)
}

fn getrandom_fill(buf: &mut [u8]) {
    // Cheap unique-ish CID source for a localhost test server.
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    for b in buf.iter_mut() {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}
