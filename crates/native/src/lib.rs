// SPDX-License-Identifier: Apache-2.0
//! rwebtransport native addon: a WebTransport **client** for Node.js.
//!
//! This crate binds a native QUIC/HTTP-3/WebTransport client (Cloudflare quiche
//! with BoringSSL) to Node through neon (N-API). The public surface is
//! intentionally low-level and event-driven; the WHATWG `WebTransport` API is
//! layered on top in TypeScript. See the JS↔native contract below.
//!
//! ## JS-visible functions
//! * `connect(url, hashes[], insecure, origin|null, hdrNames[], hdrVals[], onEvent) -> handle`
//! * `openStream(handle, bidi, requestId)`
//! * `writeStream(handle, streamId, bytes, requestId)`
//! * `finStream(handle, streamId)`
//! * `resetStream(handle, streamId, code)`
//! * `stopSending(handle, streamId, code)`
//! * `setPaused(handle, streamId, paused)`
//! * `sendDatagram(handle, bytes, requestId)`
//! * `maxDatagramSize(handle) -> number`
//! * `isReady(handle) -> boolean` / `isClosed(handle) -> boolean`
//! * `closeSession(handle, code, reasonBytes)`
//! * `shutdown(handle)`
//!
//! Events delivered to `onEvent(ev)` are plain objects `{ type, ... }`. See
//! `ev_to_js`.

mod config;
mod driver;
mod server;
mod session;
mod tls;

use std::net::IpAddr;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use neon::prelude::*;
use neon::types::buffer::TypedArray;

use config::ClientConfigParams;
use driver::{Command, DriverShared, EventSink};
use session::Ev;
use tls::CertVerification;

/// JS-facing session handle (a `JsBox`).
struct SessionHandle {
    tx: Mutex<Option<Sender<Command>>>,
    waker: Arc<mio::Waker>,
    shared: Arc<DriverShared>,
}

impl SessionHandle {
    fn send(&self, cmd: Command) {
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(cmd);
                let _ = self.waker.wake();
            }
        }
    }
}

impl Finalize for SessionHandle {
    fn finalize<'a, C: Context<'a>>(self, _cx: &mut C) {
        // Safety net if JS forgot to call shutdown() before GC.
        self.send(Command::Shutdown);
    }
}

/// Bridges driver events onto the JS event loop.
struct NeonSink {
    channel: Channel,
    callback: Arc<Root<JsFunction>>,
    shared: Arc<DriverShared>,
}

impl EventSink for NeonSink {
    fn emit(&self, events: Vec<Ev>) {
        let cb = self.callback.clone();
        let shared = self.shared.clone();
        // Count this batch as in flight to the JS loop; the driver reads this to
        // back off from pulling more data out of quiche when JS falls behind.
        self.shared.inflight.fetch_add(1, Ordering::Relaxed);
        self.channel.send(move |mut cx| {
            // Dequeued by the JS loop: no longer contributing to queue depth.
            shared.inflight.fetch_sub(1, Ordering::Relaxed);
            let f = cb.to_inner(&mut cx);
            for ev in &events {
                let obj = ev_to_js(&mut cx, ev)?;
                let this = cx.undefined();
                let args = [obj.upcast::<JsValue>()];
                f.call(&mut cx, this, args)?;
            }
            Ok(())
        });
    }
}

// ---- URL parsing ------------------------------------------------------------

struct ParsedUrl {
    host: String,
    port: u16,
    /// SNI / verification host (None for IP literals).
    sni: Option<String>,
    authority: String,
    path: String,
}

/// Parse a WebTransport URL WITHOUT resolving DNS (resolution is deferred to the
/// driver thread so it never blocks the JS event loop).
fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| "WebTransport URL must use the https:// scheme".to_string())?;

    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err("WebTransport URL has an empty authority".to_string());
    }

    // Split host and port, handling IPv6 literals.
    let (host, port): (String, u16) = if let Some(hstr) = authority.strip_prefix('[') {
        let end = hstr
            .find(']')
            .ok_or_else(|| "malformed IPv6 authority".to_string())?;
        let host = hstr[..end].to_string();
        let after = &hstr[end + 1..];
        let port = after
            .strip_prefix(':')
            .map(|p| p.parse::<u16>().map_err(|_| "invalid port".to_string()))
            .transpose()?
            .unwrap_or(443);
        (host, port)
    } else if let Some(i) = authority.rfind(':') {
        let host = authority[..i].to_string();
        let port = authority[i + 1..]
            .parse::<u16>()
            .map_err(|_| "invalid port".to_string())?;
        (host, port)
    } else {
        (authority.to_string(), 443)
    };

    let sni = if host.parse::<IpAddr>().is_ok() {
        None
    } else {
        Some(host.clone())
    };

    Ok(ParsedUrl {
        host,
        port,
        sni,
        authority: authority.to_string(),
        path,
    })
}

/// Convert a JS number to a stream id. NaN / negative / non-finite values map to
/// `u64::MAX`, which matches no real stream, so a malformed call is ignored
/// instead of aliasing the CONNECT (session) stream id 0.
fn to_stream_id(v: f64) -> u64 {
    if v.is_finite() && v >= 0.0 {
        v as u64
    } else {
        u64::MAX
    }
}

/// Convert a JS number to a QUIC/WebTransport application code, clamped below the
/// 62-bit varint ceiling so quiche's varint encoder can never panic.
fn to_code(v: f64) -> u64 {
    if v.is_finite() && v >= 0.0 {
        (v as u64).min((1u64 << 62) - 1)
    } else {
        0
    }
}

/// Convert a JS number to a request id (NaN / non-finite → 0).
fn to_request_id(v: f64) -> u64 {
    if v.is_finite() && v >= 0.0 {
        v as u64
    } else {
        0
    }
}

// ---- connect ----------------------------------------------------------------

fn connect(mut cx: FunctionContext) -> JsResult<JsBox<SessionHandle>> {
    let url = cx.argument::<JsString>(0)?.value(&mut cx);
    let hashes_arr = cx.argument::<JsArray>(1)?;
    let insecure = cx.argument::<JsBoolean>(2)?.value(&mut cx);
    let origin = {
        let v = cx.argument::<JsValue>(3)?;
        if v.is_a::<JsString, _>(&mut cx) {
            Some(v.downcast_or_throw::<JsString, _>(&mut cx)?.value(&mut cx))
        } else {
            None
        }
    };
    let names_arr = cx.argument::<JsArray>(4)?;
    let vals_arr = cx.argument::<JsArray>(5)?;
    let callback = cx.argument::<JsFunction>(6)?;

    // Certificate hashes.
    let mut hashes: Vec<[u8; 32]> = Vec::new();
    let hlen = hashes_arr.len(&mut cx);
    for i in 0..hlen {
        let elem = hashes_arr.get::<JsTypedArray<u8>, _, _>(&mut cx, i)?;
        let slice = elem.as_slice(&cx);
        if slice.len() == 32 {
            let mut h = [0u8; 32];
            h.copy_from_slice(slice);
            hashes.push(h);
        } else {
            return cx.throw_error("serverCertificateHashes entries must be 32 bytes (sha-256)");
        }
    }

    // Extra headers.
    let mut extra_headers: Vec<(String, String)> = Vec::new();
    let nlen = names_arr.len(&mut cx).min(vals_arr.len(&mut cx));
    for i in 0..nlen {
        let k = names_arr.get::<JsString, _, _>(&mut cx, i)?.value(&mut cx);
        let v = vals_arr.get::<JsString, _, _>(&mut cx, i)?.value(&mut cx);
        extra_headers.push((k, v));
    }

    let parsed = match parse_url(&url) {
        Ok(p) => p,
        Err(e) => return cx.throw_error(e),
    };

    let verify = if !hashes.is_empty() {
        CertVerification::Hashes(hashes)
    } else if insecure {
        CertVerification::Insecure
    } else {
        CertVerification::PkiDefault
    };

    let params = ClientConfigParams {
        verify,
        ..ClientConfigParams::default()
    };

    // Event-loop plumbing (created here so the poll registry can back the waker
    // stored in the handle). DNS resolution, socket bind, TLS/QUIC config, and
    // the handshake all run on the driver thread — see `driver::SessionSetup`.
    let poll = match mio::Poll::new() {
        Ok(p) => p,
        Err(e) => return cx.throw_error(format!("mio poll: {e}")),
    };
    let waker = match mio::Waker::new(poll.registry(), driver::WAKE_TOKEN) {
        Ok(w) => Arc::new(w),
        Err(e) => return cx.throw_error(format!("mio waker: {e}")),
    };
    let (tx, rx) = std::sync::mpsc::channel::<Command>();
    let shared = Arc::new(DriverShared::default());

    let sink: Box<dyn EventSink> = Box::new(NeonSink {
        channel: cx.channel(),
        callback: Arc::new(callback.root(&mut cx)),
        shared: shared.clone(),
    });

    let setup = driver::SessionSetup {
        host: parsed.host,
        port: parsed.port,
        sni: parsed.sni,
        authority: parsed.authority,
        path: parsed.path,
        origin,
        extra_headers,
        config: params,
    };

    let shared_for_thread = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("rwt-driver".into())
        .spawn(move || {
            driver::run(setup, poll, rx, sink, shared_for_thread);
        });
    if let Err(e) = spawned {
        return cx.throw_error(format!("failed to spawn driver thread: {e}"));
    }

    Ok(cx.boxed(SessionHandle {
        tx: Mutex::new(Some(tx)),
        waker,
        shared,
    }))
}

// ---- command functions ------------------------------------------------------

fn open_stream(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let bidi = cx.argument::<JsBoolean>(1)?.value(&mut cx);
    let request_id = to_request_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    handle.send(Command::OpenStream { request_id, bidi });
    Ok(cx.undefined())
}

fn write_stream(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let id = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let data = cx.argument::<JsTypedArray<u8>>(2)?.as_slice(&cx).to_vec();
    let request_id = to_request_id(cx.argument::<JsNumber>(3)?.value(&mut cx));
    handle.send(Command::Write {
        id,
        data,
        request_id,
    });
    Ok(cx.undefined())
}

fn fin_stream(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let id = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    handle.send(Command::Fin { id });
    Ok(cx.undefined())
}

fn reset_stream(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let id = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let code = to_code(cx.argument::<JsNumber>(2)?.value(&mut cx));
    handle.send(Command::ResetStream { id, code });
    Ok(cx.undefined())
}

fn stop_sending(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let id = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let code = to_code(cx.argument::<JsNumber>(2)?.value(&mut cx));
    handle.send(Command::StopSending { id, code });
    Ok(cx.undefined())
}

fn set_paused(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let id = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let paused = cx.argument::<JsBoolean>(2)?.value(&mut cx);
    handle.send(Command::SetPaused { id, paused });
    Ok(cx.undefined())
}

fn send_datagram(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let data = cx.argument::<JsTypedArray<u8>>(1)?.as_slice(&cx).to_vec();
    let request_id = to_request_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    handle.send(Command::SendDatagram { data, request_id });
    Ok(cx.undefined())
}

fn max_datagram_size(mut cx: FunctionContext) -> JsResult<JsNumber> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let n = handle.shared.max_datagram_size.load(Ordering::Relaxed);
    Ok(cx.number(n as f64))
}

fn is_ready(mut cx: FunctionContext) -> JsResult<JsBoolean> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    Ok(cx.boolean(handle.shared.ready.load(Ordering::Relaxed)))
}

fn is_closed(mut cx: FunctionContext) -> JsResult<JsBoolean> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    Ok(cx.boolean(handle.shared.closed.load(Ordering::Relaxed)))
}

fn close_session(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    let code = to_code(cx.argument::<JsNumber>(1)?.value(&mut cx)) as u32;
    let reason = cx.argument::<JsTypedArray<u8>>(2)?.as_slice(&cx).to_vec();
    handle.send(Command::Close { code, reason });
    Ok(cx.undefined())
}

fn shutdown(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<SessionHandle>>(0)?;
    handle.send(Command::Shutdown);
    Ok(cx.undefined())
}

// ---- server -----------------------------------------------------------------

use server::{ServerCommand, ServerEventSink, ServerMsg, ServerSetup};

/// JS-facing server handle (a `JsBox`).
struct ServerHandle {
    tx: Mutex<Option<Sender<ServerCommand>>>,
    waker: Arc<mio::Waker>,
    shared: Arc<DriverShared>,
}

impl ServerHandle {
    fn send(&self, cmd: ServerCommand) {
        if let Ok(guard) = self.tx.lock() {
            if let Some(tx) = guard.as_ref() {
                let _ = tx.send(cmd);
                let _ = self.waker.wake();
            }
        }
    }
}

impl Finalize for ServerHandle {
    fn finalize<'a, C: Context<'a>>(self, _cx: &mut C) {
        self.send(ServerCommand::Shutdown);
    }
}

struct NeonServerSink {
    channel: Channel,
    callback: Arc<Root<JsFunction>>,
    shared: Arc<DriverShared>,
}

impl ServerEventSink for NeonServerSink {
    fn emit(&self, messages: Vec<ServerMsg>) {
        let cb = self.callback.clone();
        let shared = self.shared.clone();
        // Balance every emit (setup, teardown, and per-loop batches) so the
        // backpressure counter can never underflow and silently disable itself.
        self.shared.inflight.fetch_add(1, Ordering::Relaxed);
        self.channel.send(move |mut cx| {
            shared.inflight.fetch_sub(1, Ordering::Relaxed);
            let f = cb.to_inner(&mut cx);
            for msg in &messages {
                let obj = server_msg_to_js(&mut cx, msg)?;
                let this = cx.undefined();
                let args = [obj.upcast::<JsValue>()];
                f.call(&mut cx, this, args)?;
            }
            Ok(())
        });
    }
}

fn server_listen(mut cx: FunctionContext) -> JsResult<JsBox<ServerHandle>> {
    let cert_path = cx.argument::<JsString>(0)?.value(&mut cx);
    let key_path = cx.argument::<JsString>(1)?.value(&mut cx);
    let host = cx.argument::<JsString>(2)?.value(&mut cx);
    let port = cx.argument::<JsNumber>(3)?.value(&mut cx) as u16;
    let reuse_port = cx.argument::<JsBoolean>(4)?.value(&mut cx);
    let callback = cx.argument::<JsFunction>(5)?;

    let poll = match mio::Poll::new() {
        Ok(p) => p,
        Err(e) => return cx.throw_error(format!("mio poll: {e}")),
    };
    let waker = match mio::Waker::new(poll.registry(), driver::WAKE_TOKEN) {
        Ok(w) => Arc::new(w),
        Err(e) => return cx.throw_error(format!("mio waker: {e}")),
    };
    let (tx, rx) = std::sync::mpsc::channel::<ServerCommand>();
    let shared = Arc::new(DriverShared::default());

    let sink: Box<dyn ServerEventSink> = Box::new(NeonServerSink {
        channel: cx.channel(),
        callback: Arc::new(callback.root(&mut cx)),
        shared: shared.clone(),
    });

    let setup = ServerSetup {
        host,
        port,
        cert_path,
        key_path,
        reuse_port,
    };

    let shared_for_thread = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("rwt-server".into())
        .spawn(move || {
            server::run(setup, poll, rx, sink, shared_for_thread);
        });
    if let Err(e) = spawned {
        return cx.throw_error(format!("failed to spawn server thread: {e}"));
    }

    Ok(cx.boxed(ServerHandle {
        tx: Mutex::new(Some(tx)),
        waker,
        shared,
    }))
}

fn server_open_stream(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let bidi = cx.argument::<JsBoolean>(2)?.value(&mut cx);
    let request_id = to_request_id(cx.argument::<JsNumber>(3)?.value(&mut cx));
    handle.send(ServerCommand::OpenStream {
        session,
        request_id,
        bidi,
    });
    Ok(cx.undefined())
}

fn server_write(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let id = to_stream_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    let data = cx.argument::<JsTypedArray<u8>>(3)?.as_slice(&cx).to_vec();
    let request_id = to_request_id(cx.argument::<JsNumber>(4)?.value(&mut cx));
    handle.send(ServerCommand::Write {
        session,
        id,
        data,
        request_id,
    });
    Ok(cx.undefined())
}

fn server_fin(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let id = to_stream_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    handle.send(ServerCommand::Fin { session, id });
    Ok(cx.undefined())
}

fn server_reset(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let id = to_stream_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    let code = to_code(cx.argument::<JsNumber>(3)?.value(&mut cx));
    handle.send(ServerCommand::ResetStream { session, id, code });
    Ok(cx.undefined())
}

fn server_stop_sending(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let id = to_stream_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    let code = to_code(cx.argument::<JsNumber>(3)?.value(&mut cx));
    handle.send(ServerCommand::StopSending { session, id, code });
    Ok(cx.undefined())
}

fn server_set_paused(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let id = to_stream_id(cx.argument::<JsNumber>(2)?.value(&mut cx));
    let paused = cx.argument::<JsBoolean>(3)?.value(&mut cx);
    handle.send(ServerCommand::SetPaused {
        session,
        id,
        paused,
    });
    Ok(cx.undefined())
}

fn server_send_datagram(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let data = cx.argument::<JsTypedArray<u8>>(2)?.as_slice(&cx).to_vec();
    let request_id = to_request_id(cx.argument::<JsNumber>(3)?.value(&mut cx));
    handle.send(ServerCommand::SendDatagram {
        session,
        data,
        request_id,
    });
    Ok(cx.undefined())
}

fn server_max_datagram_size(mut cx: FunctionContext) -> JsResult<JsNumber> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let n = handle.shared.max_datagram_size.load(Ordering::Relaxed);
    Ok(cx.number(n as f64))
}

fn server_close_session(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    let session = to_stream_id(cx.argument::<JsNumber>(1)?.value(&mut cx));
    let code = to_code(cx.argument::<JsNumber>(2)?.value(&mut cx)) as u32;
    let reason = cx.argument::<JsTypedArray<u8>>(3)?.as_slice(&cx).to_vec();
    handle.send(ServerCommand::CloseSession {
        session,
        code,
        reason,
    });
    Ok(cx.undefined())
}

fn server_shutdown(mut cx: FunctionContext) -> JsResult<JsUndefined> {
    let handle = cx.argument::<JsBox<ServerHandle>>(0)?;
    handle.send(ServerCommand::Shutdown);
    Ok(cx.undefined())
}

fn server_msg_to_js<'a>(cx: &mut TaskContext<'a>, msg: &ServerMsg) -> JsResult<'a, JsObject> {
    match msg {
        ServerMsg::Listening(port) => {
            let obj = cx.empty_object();
            set_str(cx, &obj, "type", "listening")?;
            set_num(cx, &obj, "port", *port as f64)?;
            Ok(obj)
        }
        ServerMsg::ServerError(m) => {
            let obj = cx.empty_object();
            set_str(cx, &obj, "type", "serverError")?;
            set_str(cx, &obj, "message", m)?;
            Ok(obj)
        }
        ServerMsg::ServerClosed => {
            let obj = cx.empty_object();
            set_str(cx, &obj, "type", "serverClosed")?;
            Ok(obj)
        }
        ServerMsg::Session(session, ev) => {
            let obj = ev_to_js(cx, ev)?;
            set_num(cx, &obj, "session", *session as f64)?;
            Ok(obj)
        }
    }
}

// ---- event → JS -------------------------------------------------------------

fn ev_to_js<'a>(cx: &mut TaskContext<'a>, ev: &Ev) -> JsResult<'a, JsObject> {
    let obj = cx.empty_object();
    match ev {
        Ev::Ready => {
            set_str(cx, &obj, "type", "ready")?;
        }
        Ev::ServerReady {
            authority,
            path,
            origin,
            headers,
        } => {
            set_str(cx, &obj, "type", "serverReady")?;
            set_str(cx, &obj, "authority", authority)?;
            set_str(cx, &obj, "path", path)?;
            match origin {
                Some(o) => set_str(cx, &obj, "origin", o)?,
                None => {
                    let n = cx.null();
                    obj.set(cx, "origin", n)?;
                }
            }
            let hobj = cx.empty_object();
            for (k, v) in headers {
                set_str(cx, &hobj, k, v)?;
            }
            obj.set(cx, "headers", hobj)?;
        }
        Ev::Closed {
            code,
            reason,
            remote,
        } => {
            set_str(cx, &obj, "type", "closed")?;
            set_num(cx, &obj, "code", *code as f64)?;
            set_buf(cx, &obj, "reason", reason)?;
            set_bool(cx, &obj, "remote", *remote)?;
        }
        Ev::Error(msg) => {
            set_str(cx, &obj, "type", "error")?;
            set_str(cx, &obj, "message", msg)?;
        }
        Ev::Datagram(data) => {
            set_str(cx, &obj, "type", "datagram")?;
            set_buf(cx, &obj, "data", data)?;
        }
        Ev::IncomingStream { id, bidi } => {
            set_str(cx, &obj, "type", "stream")?;
            set_num(cx, &obj, "streamId", *id as f64)?;
            set_bool(cx, &obj, "bidi", *bidi)?;
        }
        Ev::StreamData { id, data } => {
            set_str(cx, &obj, "type", "streamData")?;
            set_num(cx, &obj, "streamId", *id as f64)?;
            set_buf(cx, &obj, "data", data)?;
        }
        Ev::StreamFinished { id } => {
            set_str(cx, &obj, "type", "streamFin")?;
            set_num(cx, &obj, "streamId", *id as f64)?;
        }
        Ev::StreamReset { id, code } => {
            set_str(cx, &obj, "type", "streamReset")?;
            set_num(cx, &obj, "streamId", *id as f64)?;
            set_num(cx, &obj, "code", *code as f64)?;
        }
        Ev::StreamStopSending { id, code } => {
            set_str(cx, &obj, "type", "streamStopSending")?;
            set_num(cx, &obj, "streamId", *id as f64)?;
            set_num(cx, &obj, "code", *code as f64)?;
        }
        Ev::StreamOpened { request_id, id } => {
            set_str(cx, &obj, "type", "streamOpened")?;
            set_num(cx, &obj, "requestId", *request_id as f64)?;
            set_num(cx, &obj, "streamId", *id as f64)?;
        }
        Ev::WriteAck { request_id } => {
            set_str(cx, &obj, "type", "writeAck")?;
            set_num(cx, &obj, "requestId", *request_id as f64)?;
        }
        Ev::DatagramAck { request_id, sent } => {
            set_str(cx, &obj, "type", "datagramAck")?;
            set_num(cx, &obj, "requestId", *request_id as f64)?;
            set_bool(cx, &obj, "sent", *sent)?;
        }
    }
    Ok(obj)
}

fn set_str<'a>(
    cx: &mut TaskContext<'a>,
    obj: &Handle<'a, JsObject>,
    key: &str,
    val: &str,
) -> NeonResult<()> {
    let v = cx.string(val);
    obj.set(cx, key, v)?;
    Ok(())
}

fn set_num<'a>(
    cx: &mut TaskContext<'a>,
    obj: &Handle<'a, JsObject>,
    key: &str,
    val: f64,
) -> NeonResult<()> {
    let v = cx.number(val);
    obj.set(cx, key, v)?;
    Ok(())
}

fn set_bool<'a>(
    cx: &mut TaskContext<'a>,
    obj: &Handle<'a, JsObject>,
    key: &str,
    val: bool,
) -> NeonResult<()> {
    let v = cx.boolean(val);
    obj.set(cx, key, v)?;
    Ok(())
}

fn set_buf<'a>(
    cx: &mut TaskContext<'a>,
    obj: &Handle<'a, JsObject>,
    key: &str,
    val: &[u8],
) -> NeonResult<()> {
    let v = JsBuffer::from_slice(cx, val)?;
    obj.set(cx, key, v)?;
    Ok(())
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    cx.export_function("connect", connect)?;
    cx.export_function("openStream", open_stream)?;
    cx.export_function("writeStream", write_stream)?;
    cx.export_function("finStream", fin_stream)?;
    cx.export_function("resetStream", reset_stream)?;
    cx.export_function("stopSending", stop_sending)?;
    cx.export_function("setPaused", set_paused)?;
    cx.export_function("sendDatagram", send_datagram)?;
    cx.export_function("maxDatagramSize", max_datagram_size)?;
    cx.export_function("isReady", is_ready)?;
    cx.export_function("isClosed", is_closed)?;
    cx.export_function("closeSession", close_session)?;
    cx.export_function("shutdown", shutdown)?;

    // Server surface.
    cx.export_function("serverListen", server_listen)?;
    cx.export_function("serverOpenStream", server_open_stream)?;
    cx.export_function("serverWrite", server_write)?;
    cx.export_function("serverFin", server_fin)?;
    cx.export_function("serverReset", server_reset)?;
    cx.export_function("serverStopSending", server_stop_sending)?;
    cx.export_function("serverSetPaused", server_set_paused)?;
    cx.export_function("serverSendDatagram", server_send_datagram)?;
    cx.export_function("serverMaxDatagramSize", server_max_datagram_size)?;
    cx.export_function("serverCloseSession", server_close_session)?;
    cx.export_function("serverShutdown", server_shutdown)?;
    Ok(())
}
