// SPDX-License-Identifier: Apache-2.0
//! Shared WebTransport-over-HTTP/3 protocol primitives.
//!
//! Framing, SETTINGS, the Extended CONNECT header codec, and the WebTransport
//! stream/capsule constants: everything both the client (`rwebtransport-native`)
//! and the test echo server (`rwebtransport-echo-server`) need on top of quiche's
//! raw QUIC streams. Header (de)compression reuses `quiche::h3::qpack`.

pub mod h3;
