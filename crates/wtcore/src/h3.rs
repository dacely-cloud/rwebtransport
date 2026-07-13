// SPDX-License-Identifier: Apache-2.0
//! HTTP/3 + WebTransport framing primitives.
//!
//! quiche 0.29 knows nothing about WebTransport, so this module supplies the
//! minimum HTTP/3 machinery a WebTransport *client* needs on top of quiche's raw
//! QUIC streams: variable-length integers, the handful of HTTP/3 frames used
//! during setup, the SETTINGS we advertise, the Extended CONNECT request codec
//! (built on quiche's public QPACK), and the WebTransport stream/capsule
//! constants. Header (de)compression reuses `quiche::h3::qpack`.

use quiche::h3::{Header, NameValue};

// ---- HTTP/3 unidirectional stream types (RFC 9114) --------------------------
pub const H3_CONTROL_STREAM_TYPE: u64 = 0x00;
pub const QPACK_ENCODER_STREAM_TYPE: u64 = 0x02;
pub const QPACK_DECODER_STREAM_TYPE: u64 = 0x03;

// ---- HTTP/3 frame types -----------------------------------------------------
pub const FRAME_DATA: u64 = 0x00;
pub const FRAME_HEADERS: u64 = 0x01;
pub const FRAME_SETTINGS: u64 = 0x04;
pub const FRAME_GOAWAY: u64 = 0x07;

// ---- HTTP/3 SETTINGS identifiers --------------------------------------------
pub const SETTINGS_QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
pub const SETTINGS_MAX_FIELD_SECTION_SIZE: u64 = 0x06;
pub const SETTINGS_QPACK_BLOCKED_STREAMS: u64 = 0x07;
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u64 = 0x08;
pub const SETTINGS_H3_DATAGRAM: u64 = 0x33;
/// draft-ietf-webtrans-http3 `SETTINGS_WEBTRANSPORT_MAX_SESSIONS`.
pub const SETTINGS_WT_MAX_SESSIONS: u64 = 0x14e9_cd29;
/// Older draft `SETTINGS_ENABLE_WEBTRANSPORT` (kept for maximum compatibility).
pub const SETTINGS_ENABLE_WEBTRANSPORT: u64 = 0x2b60_3742;

// ---- WebTransport-over-HTTP/3 stream signals & capsules ---------------------
/// Unidirectional WebTransport stream type prefix.
pub const WT_UNI_STREAM_TYPE: u64 = 0x54;
/// Bidirectional WebTransport stream signal (first frame type on the stream).
pub const WT_BIDI_FRAME_TYPE: u64 = 0x41;
/// `CLOSE_WEBTRANSPORT_SESSION` capsule type.
pub const WT_CLOSE_SESSION_CAPSULE: u64 = 0x2843;
/// `DRAIN_WEBTRANSPORT_SESSION` capsule type.
pub const WT_DRAIN_SESSION_CAPSULE: u64 = 0x78ae;

/// Largest QPACK header block we will decode from a peer.
pub const MAX_HEADER_BLOCK: u64 = 64 * 1024;

// ---- Variable-length integers (RFC 9000 §16) --------------------------------

/// Try to read a varint from the front of `buf`. Returns `(value, len)` or
/// `None` if the buffer does not yet hold a complete varint.
pub fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    if buf.len() < len {
        return None;
    }
    let mut v = u64::from(first & 0x3f);
    for &b in &buf[1..len] {
        v = (v << 8) | u64::from(b);
    }
    Some((v, len))
}

/// Append `v` to `out` using the shortest varint encoding.
pub fn put_varint(v: u64, out: &mut Vec<u8>) {
    if v < (1 << 6) {
        out.push(v as u8);
    } else if v < (1 << 14) {
        out.push(0x40 | (v >> 8) as u8);
        out.push(v as u8);
    } else if v < (1 << 30) {
        out.push(0x80 | (v >> 24) as u8);
        out.push((v >> 16) as u8);
        out.push((v >> 8) as u8);
        out.push(v as u8);
    } else {
        out.push(0xc0 | (v >> 56) as u8);
        for shift in [48u32, 40, 32, 24, 16, 8, 0] {
            out.push((v >> shift) as u8);
        }
    }
}

/// Number of bytes `put_varint` would emit for `v`.
pub fn varint_len(v: u64) -> usize {
    if v < (1 << 6) {
        1
    } else if v < (1 << 14) {
        2
    } else if v < (1 << 30) {
        4
    } else {
        8
    }
}

/// Encode a complete HTTP/3 frame (`type`, `length`, `payload`).
pub fn frame(ty: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 16);
    put_varint(ty, &mut out);
    put_varint(payload.len() as u64, &mut out);
    out.extend_from_slice(payload);
    out
}

/// Bytes for the client control stream: the stream-type prefix followed by our
/// SETTINGS frame.
pub fn control_stream_prefix() -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(H3_CONTROL_STREAM_TYPE, &mut out);
    out.extend_from_slice(&frame(FRAME_SETTINGS, &settings_payload()));
    out
}

/// The SETTINGS we advertise: QPACK static-only, plus everything a WebTransport
/// client needs (Extended CONNECT, H3 datagrams, and both the modern and legacy
/// WebTransport capability settings).
pub fn settings_payload() -> Vec<u8> {
    let mut p = Vec::new();
    let pairs: [(u64, u64); 6] = [
        (SETTINGS_QPACK_MAX_TABLE_CAPACITY, 0),
        (SETTINGS_QPACK_BLOCKED_STREAMS, 0),
        (SETTINGS_ENABLE_CONNECT_PROTOCOL, 1),
        (SETTINGS_H3_DATAGRAM, 1),
        (SETTINGS_ENABLE_WEBTRANSPORT, 1),
        (SETTINGS_WT_MAX_SESSIONS, 256),
    ];
    for (id, val) in pairs {
        put_varint(id, &mut p);
        put_varint(val, &mut p);
    }
    p
}

/// Peer SETTINGS relevant to WebTransport.
#[derive(Clone, Copy, Debug, Default)]
pub struct PeerSettings {
    pub enable_connect_protocol: bool,
    pub h3_datagram: bool,
    pub enable_webtransport: bool,
    pub wt_max_sessions: u64,
}

impl PeerSettings {
    /// Whether the peer advertises enough to run a WebTransport session.
    pub fn webtransport_ok(&self) -> bool {
        self.enable_connect_protocol
            && self.h3_datagram
            && (self.enable_webtransport || self.wt_max_sessions > 0)
    }
}

/// Parse a SETTINGS frame payload. Unknown identifiers are ignored (as required).
pub fn parse_settings(mut payload: &[u8]) -> PeerSettings {
    let mut s = PeerSettings::default();
    while !payload.is_empty() {
        let Some((id, n1)) = read_varint(payload) else {
            break;
        };
        let Some((val, n2)) = read_varint(&payload[n1..]) else {
            break;
        };
        match id {
            SETTINGS_ENABLE_CONNECT_PROTOCOL => s.enable_connect_protocol = val != 0,
            SETTINGS_H3_DATAGRAM => s.h3_datagram = val != 0,
            SETTINGS_ENABLE_WEBTRANSPORT => s.enable_webtransport = val != 0,
            SETTINGS_WT_MAX_SESSIONS => s.wt_max_sessions = val,
            _ => {}
        }
        payload = &payload[n1 + n2..];
    }
    s
}

/// Build the Extended CONNECT request header list for a WebTransport session.
pub fn connect_headers(
    authority: &str,
    path: &str,
    origin: Option<&str>,
    extra: &[(String, String)],
) -> Vec<Header> {
    let mut h = vec![
        Header::new(b":method", b"CONNECT"),
        Header::new(b":protocol", b"webtransport"),
        Header::new(b":scheme", b"https"),
        Header::new(b":authority", authority.as_bytes()),
        Header::new(b":path", path.as_bytes()),
    ];
    if let Some(o) = origin {
        h.push(Header::new(b"origin", o.as_bytes()));
    }
    for (k, v) in extra {
        h.push(Header::new(k.as_bytes(), v.as_bytes()));
    }
    h
}

/// QPACK-encode a header list and wrap it in an HTTP/3 HEADERS frame.
pub fn encode_headers_frame(headers: &[Header]) -> Result<Vec<u8>, String> {
    // Upper bound: static-only encoding never exceeds name+value+8 per field,
    // plus the 2-byte required-insert-count/base prefix.
    let cap: usize = headers
        .iter()
        .map(|h| h.name().len() + h.value().len() + 8)
        .sum::<usize>()
        + 16;
    let mut block = vec![0u8; cap];
    let mut enc = quiche::h3::qpack::Encoder::new();
    let n = enc
        .encode(headers, &mut block)
        .map_err(|e| format!("qpack encode: {e:?}"))?;
    block.truncate(n);
    Ok(frame(FRAME_HEADERS, &block))
}

/// QPACK-decode a HEADERS frame payload into a header list.
pub fn decode_header_block(block: &[u8]) -> Result<Vec<Header>, String> {
    let mut dec = quiche::h3::qpack::Decoder::new();
    dec.decode(block, MAX_HEADER_BLOCK)
        .map_err(|e| format!("qpack decode: {e:?}"))
}

/// Extract the numeric `:status` from a decoded response header list.
pub fn status_of(headers: &[Header]) -> Option<u16> {
    for h in headers {
        if h.name() == b":status" {
            return std::str::from_utf8(h.value()).ok()?.trim().parse().ok();
        }
    }
    None
}
