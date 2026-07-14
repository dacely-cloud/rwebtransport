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

pub const H3_CONTROL_STREAM_TYPE: u64 = 0x00;
pub const QPACK_ENCODER_STREAM_TYPE: u64 = 0x02;
pub const QPACK_DECODER_STREAM_TYPE: u64 = 0x03;

pub const FRAME_DATA: u64 = 0x00;
pub const FRAME_HEADERS: u64 = 0x01;
pub const FRAME_SETTINGS: u64 = 0x04;
pub const FRAME_GOAWAY: u64 = 0x07;

pub const SETTINGS_QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
pub const SETTINGS_MAX_FIELD_SECTION_SIZE: u64 = 0x06;
pub const SETTINGS_QPACK_BLOCKED_STREAMS: u64 = 0x07;
pub const SETTINGS_ENABLE_CONNECT_PROTOCOL: u64 = 0x08;
pub const SETTINGS_H3_DATAGRAM: u64 = 0x33;
/// draft-ietf-webtrans-http3 `SETTINGS_WEBTRANSPORT_MAX_SESSIONS`.
pub const SETTINGS_WT_MAX_SESSIONS: u64 = 0x14e9_cd29;
/// Older draft `SETTINGS_ENABLE_WEBTRANSPORT` (kept for maximum compatibility).
pub const SETTINGS_ENABLE_WEBTRANSPORT: u64 = 0x2b60_3742;
/// `SETTINGS_WT_INITIAL_MAX_DATA`: initial per-session send-data allowance.
pub const SETTINGS_WT_INITIAL_MAX_DATA: u64 = 0x2b61;
/// `SETTINGS_WT_INITIAL_MAX_STREAMS_UNI`: initial per-session uni stream limit.
pub const SETTINGS_WT_INITIAL_MAX_STREAMS_UNI: u64 = 0x2b64;
/// `SETTINGS_WT_INITIAL_MAX_STREAMS_BIDI`: initial per-session bidi stream limit.
pub const SETTINGS_WT_INITIAL_MAX_STREAMS_BIDI: u64 = 0x2b65;

/// Per-session flow-control capsule types (draft-ietf-webtrans-http3 §5).
pub const WT_MAX_DATA_CAPSULE: u64 = 0x190b_4d3d;
pub const WT_MAX_STREAMS_BIDI_CAPSULE: u64 = 0x190b_4d3f;
pub const WT_MAX_STREAMS_UNI_CAPSULE: u64 = 0x190b_4d40;
pub const WT_DATA_BLOCKED_CAPSULE: u64 = 0x190b_4d41;
pub const WT_STREAMS_BLOCKED_BIDI_CAPSULE: u64 = 0x190b_4d43;
pub const WT_STREAMS_BLOCKED_UNI_CAPSULE: u64 = 0x190b_4d44;

/// Our advertised per-session flow-control limits. They match the QUIC transport
/// limits in `config.rs` because exactly one WebTransport session rides each QUIC
/// connection, so the session budget and the connection budget coincide.
pub const WT_INITIAL_MAX_DATA: u64 = 10 * 1024 * 1024;
pub const WT_INITIAL_MAX_STREAMS: u64 = 256;

/// Unidirectional WebTransport stream type prefix.
pub const WT_UNI_STREAM_TYPE: u64 = 0x54;
/// Bidirectional WebTransport stream signal (first frame type on the stream).
pub const WT_BIDI_FRAME_TYPE: u64 = 0x41;
/// `CLOSE_WEBTRANSPORT_SESSION` capsule type.
pub const WT_CLOSE_SESSION_CAPSULE: u64 = 0x2843;
/// `DRAIN_WEBTRANSPORT_SESSION` capsule type.
pub const WT_DRAIN_SESSION_CAPSULE: u64 = 0x78ae;

/// Maximum length in bytes of a `CLOSE_WEBTRANSPORT_SESSION` reason phrase.
pub const WT_MAX_CLOSE_REASON: usize = 1024;

/// First code of the HTTP/3 error range reserved for WebTransport application
/// errors (draft-ietf-webtrans-http3 §4.3).
pub const WT_APP_ERROR_FIRST: u64 = 0x52e4_a40f_a8db;
/// Last code of that range: `webtransport_code_to_http_code(0xffff_ffff)`.
pub const WT_APP_ERROR_LAST: u64 = 0x52e5_ac98_3162;

/// Map a 32-bit WebTransport application error code into the HTTP/3 error-code
/// space, skipping the HTTP/3 GREASE codepoints (`0x1f * N + 0x21`). This is the
/// code an endpoint MUST put in a QUIC RESET_STREAM / STOP_SENDING for a
/// WebTransport data stream.
pub fn webtransport_code_to_http_code(n: u64) -> u64 {
    WT_APP_ERROR_FIRST + n + n / 0x1e
}

/// Inverse of [`webtransport_code_to_http_code`]. Returns `None` when `h` is
/// outside the reserved range or lands on a reserved GREASE codepoint, i.e. it
/// is not a valid WebTransport-mapped code.
pub fn http_code_to_webtransport_code(h: u64) -> Option<u64> {
    if !(WT_APP_ERROR_FIRST..=WT_APP_ERROR_LAST).contains(&h) {
        return None;
    }
    if h.wrapping_sub(0x21).is_multiple_of(0x1f) {
        return None;
    }
    let shifted = h - WT_APP_ERROR_FIRST;
    Some(shifted - shifted / 0x1f)
}

/// Truncate a close reason to the longest valid UTF-8 prefix not exceeding
/// [`WT_MAX_CLOSE_REASON`] bytes, without splitting a multi-byte character.
pub fn truncate_close_reason(mut reason: Vec<u8>) -> Vec<u8> {
    if reason.len() <= WT_MAX_CLOSE_REASON {
        return reason;
    }
    let mut end = WT_MAX_CLOSE_REASON;
    while end > 0 && (reason[end] & 0xc0) == 0x80 {
        end -= 1;
    }
    reason.truncate(end);
    reason
}

/// Largest QPACK header block we will decode from a peer.
pub const MAX_HEADER_BLOCK: u64 = 64 * 1024;

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
    let pairs: [(u64, u64); 9] = [
        (SETTINGS_QPACK_MAX_TABLE_CAPACITY, 0),
        (SETTINGS_QPACK_BLOCKED_STREAMS, 0),
        (SETTINGS_ENABLE_CONNECT_PROTOCOL, 1),
        (SETTINGS_H3_DATAGRAM, 1),
        (SETTINGS_ENABLE_WEBTRANSPORT, 1),
        (SETTINGS_WT_MAX_SESSIONS, 256),
        (SETTINGS_WT_INITIAL_MAX_DATA, WT_INITIAL_MAX_DATA),
        (SETTINGS_WT_INITIAL_MAX_STREAMS_BIDI, WT_INITIAL_MAX_STREAMS),
        (SETTINGS_WT_INITIAL_MAX_STREAMS_UNI, WT_INITIAL_MAX_STREAMS),
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
    /// Peer's initial per-session send-data allowance (SETTINGS_WT_INITIAL_MAX_DATA).
    pub wt_initial_max_data: u64,
    /// Peer's initial per-session bidi stream limit.
    pub wt_initial_max_streams_bidi: u64,
    /// Peer's initial per-session uni stream limit.
    pub wt_initial_max_streams_uni: u64,
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
            SETTINGS_WT_INITIAL_MAX_DATA => s.wt_initial_max_data = val,
            SETTINGS_WT_INITIAL_MAX_STREAMS_BIDI => s.wt_initial_max_streams_bidi = val,
            SETTINGS_WT_INITIAL_MAX_STREAMS_UNI => s.wt_initial_max_streams_uni = val,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_code_mapping_roundtrips() {
        assert_eq!(webtransport_code_to_http_code(0), WT_APP_ERROR_FIRST);
        assert_eq!(
            webtransport_code_to_http_code(0xffff_ffff),
            WT_APP_ERROR_LAST
        );
        for n in [
            0u64,
            1,
            29,
            30,
            31,
            100,
            1000,
            0x1e,
            0x1f,
            65_535,
            0xffff_ffff,
        ] {
            let h = webtransport_code_to_http_code(n);
            assert!((WT_APP_ERROR_FIRST..=WT_APP_ERROR_LAST).contains(&h));
            assert_eq!(http_code_to_webtransport_code(h), Some(n), "n={n} h={h:#x}");
        }
    }

    #[test]
    fn mapping_never_lands_on_a_grease_codepoint() {
        for n in 0u64..3000 {
            let h = webtransport_code_to_http_code(n);
            assert_ne!(
                h.wrapping_sub(0x21) % 0x1f,
                0,
                "n={n} mapped onto a GREASE code"
            );
        }
    }

    #[test]
    fn out_of_range_and_grease_have_no_inverse() {
        assert_eq!(http_code_to_webtransport_code(WT_APP_ERROR_FIRST - 1), None);
        assert_eq!(http_code_to_webtransport_code(WT_APP_ERROR_LAST + 1), None);
        let mut grease = WT_APP_ERROR_FIRST;
        while grease.wrapping_sub(0x21) % 0x1f != 0 {
            grease += 1;
        }
        assert!(grease <= WT_APP_ERROR_LAST);
        assert_eq!(http_code_to_webtransport_code(grease), None);
    }

    #[test]
    fn reason_truncation_keeps_utf8_boundary() {
        assert_eq!(truncate_close_reason(b"hi".to_vec()), b"hi");
        // 3-byte chars so 1024 does not fall on a boundary; must back up.
        let big = "\u{20ac}".repeat(500).into_bytes(); // 1500 bytes
        let out = truncate_close_reason(big);
        assert!(out.len() <= WT_MAX_CLOSE_REASON);
        assert!(
            std::str::from_utf8(&out).is_ok(),
            "truncation split a character"
        );
        assert_eq!(out.len(), 1023); // 341 * 3
    }

    #[test]
    fn settings_advertise_and_parse_wt_flow_control() {
        let s = parse_settings(&settings_payload());
        assert!(s.webtransport_ok());
        assert_eq!(s.wt_max_sessions, 256);
        assert_eq!(s.wt_initial_max_data, WT_INITIAL_MAX_DATA);
        assert_eq!(s.wt_initial_max_streams_bidi, WT_INITIAL_MAX_STREAMS);
        assert_eq!(s.wt_initial_max_streams_uni, WT_INITIAL_MAX_STREAMS);
    }
}
