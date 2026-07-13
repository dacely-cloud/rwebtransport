// SPDX-License-Identifier: Apache-2.0
//! QUIC client configuration built on the vendored Cloudflare quiche.

use crate::tls::{build_client_tls, CertVerification};

/// HTTP/3 ALPN token.
pub const H3_ALPN: &[u8] = b"h3";

/// Tunable client parameters. Defaults are chosen to comfortably run a
/// WebTransport session (control + QPACK streams plus application streams and
/// datagrams) against a public-internet server.
#[derive(Clone, Debug)]
pub struct ClientConfigParams {
    pub verify: CertVerification,
    pub max_idle_timeout_ms: u64,
    pub max_udp_payload_size: usize,
    pub initial_max_data: u64,
    pub initial_max_stream_data: u64,
    pub initial_max_streams_bidi: u64,
    pub initial_max_streams_uni: u64,
    pub dgram_recv_queue: usize,
    pub dgram_send_queue: usize,
}

impl Default for ClientConfigParams {
    fn default() -> Self {
        Self {
            verify: CertVerification::PkiDefault,
            max_idle_timeout_ms: 30_000,
            max_udp_payload_size: 1350,
            initial_max_data: 10 * 1024 * 1024,
            initial_max_stream_data: 1024 * 1024,
            initial_max_streams_bidi: 256,
            initial_max_streams_uni: 256,
            dgram_recv_queue: 65_536,
            dgram_send_queue: 65_536,
        }
    }
}

/// Build a ready-to-use quiche client [`quiche::Config`].
pub fn build_config(p: &ClientConfigParams) -> Result<quiche::Config, String> {
    let builder = build_client_tls(&p.verify)?;
    let mut config = quiche::Config::with_boring_ssl_ctx_builder(quiche::PROTOCOL_VERSION, builder)
        .map_err(|e| format!("quiche config: {e:?}"))?;

    config
        .set_application_protos(&[H3_ALPN])
        .map_err(|e| format!("set_application_protos: {e:?}"))?;

    match p.verify {
        CertVerification::PkiDefault => {
            // Turn on BoringSSL's built-in chain + hostname verification and give
            // it a trust anchor set. Hostname verification is wired by quiche's
            // `set_host_name` (X509_VERIFY_PARAM_set1_host).
            config.verify_peer(true);
            #[cfg(unix)]
            {
                // Best-effort system roots. If neither path exists the handshake
                // will fail closed, which is the correct default.
                let _ = config.load_verify_locations_from_directory("/etc/ssl/certs");
            }
        }
        CertVerification::Hashes(_) | CertVerification::Insecure => {
            // Verification is fully handled on the SSL context (custom callback /
            // NONE). Do NOT call verify_peer here — it would install quiche's own
            // None-callback over ours.
        }
    }

    config.set_max_idle_timeout(p.max_idle_timeout_ms);
    config.set_max_recv_udp_payload_size(p.max_udp_payload_size);
    config.set_max_send_udp_payload_size(p.max_udp_payload_size);
    config.set_initial_max_data(p.initial_max_data);
    config.set_initial_max_stream_data_bidi_local(p.initial_max_stream_data);
    config.set_initial_max_stream_data_bidi_remote(p.initial_max_stream_data);
    config.set_initial_max_stream_data_uni(p.initial_max_stream_data);
    config.set_initial_max_streams_bidi(p.initial_max_streams_bidi);
    config.set_initial_max_streams_uni(p.initial_max_streams_uni);
    config.set_disable_active_migration(true);
    config.enable_dgram(true, p.dgram_recv_queue, p.dgram_send_queue);

    Ok(config)
}
