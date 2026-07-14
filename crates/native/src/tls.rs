// SPDX-License-Identifier: Apache-2.0
//! The default "Google Chrome" TLS profile for the QUIC (TLS 1.3) handshake.
//!
//! QUIC mandates TLS 1.3, so, unlike a TCP/TLS stack, there is no TLS 1.2
//! cipher list to negotiate. What still shapes the ClientHello, and what servers
//! (Cloudflare in particular) fingerprint, is the group preference, the
//! signature-algorithm list, GREASE, and extension ordering. We mirror Chrome's
//! choices so the handshake is browser-indistinguishable. The TLS 1.3
//! cipher-suite preference order is fixed inside BoringSSL and already matches
//! Chrome (AES-128-GCM, AES-256-GCM, ChaCha20-Poly1305).

use boring::hash::MessageDigest;
use boring::ssl::{
    SslAlert, SslContextBuilder, SslMethod, SslVerifyError, SslVerifyMode, SslVersion,
};

/// How the server certificate is verified.
#[derive(Clone, Debug)]
pub enum CertVerification {
    /// Full PKI validation against the system trust store, with hostname
    /// checking (the default). The `verify_peer(true)` + root loading half is
    /// applied on the quiche `Config` in [`crate::config`]; nothing is set on
    /// the SSL context here.
    PkiDefault,
    /// Accept the server certificate iff its SHA-256 fingerprint matches one of
    /// these. This mirrors the WebTransport `serverCertificateHashes` option:
    /// no CA chain and no hostname check, just a pinned leaf fingerprint.
    Hashes(Vec<[u8; 32]>),
    /// Accept any certificate. Development only.
    Insecure,
}

/// Chrome's TLS 1.3 signature-algorithm preference.
const CHROME_SIGALGS: &str = "ecdsa_secp256r1_sha256:\
     rsa_pss_rsae_sha256:\
     rsa_pkcs1_sha256:\
     ecdsa_secp384r1_sha384:\
     rsa_pss_rsae_sha384:\
     rsa_pkcs1_sha384:\
     rsa_pss_rsae_sha512:\
     rsa_pkcs1_sha512:\
     rsa_pkcs1_sha1";

/// Build a BoringSSL client context configured with the Chrome TLS profile and
/// the requested certificate-verification policy.
pub fn build_client_tls(verify: &CertVerification) -> Result<SslContextBuilder, String> {
    let mut b =
        SslContextBuilder::new(SslMethod::tls()).map_err(|e| format!("SslContextBuilder: {e}"))?;

    // QUIC mandates TLS 1.3.
    b.set_min_proto_version(Some(SslVersion::TLS1_3))
        .map_err(|e| format!("set_min_proto_version: {e}"))?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))
        .map_err(|e| format!("set_max_proto_version: {e}"))?;

    // Chrome's group (named curve) preference.
    b.set_curves_list("X25519:P-256:P-384")
        .map_err(|e| format!("set_curves_list: {e}"))?;

    // Chrome's signature-algorithm list.
    b.set_sigalgs_list(CHROME_SIGALGS)
        .map_err(|e| format!("set_sigalgs_list: {e}"))?;

    // Browser-like ClientHello shape: GREASE values and permuted extensions.
    b.set_grease_enabled(true);
    b.set_permute_extensions(true);

    match verify {
        CertVerification::PkiDefault => {
            // Applied on the quiche Config (verify_peer + system roots). Leaving
            // the SSL context untouched keeps quiche's built-in verification
            // (including the hostname check wired by `set_host_name`).
        }
        CertVerification::Insecure => {
            b.set_verify(SslVerifyMode::NONE);
        }
        CertVerification::Hashes(hashes) => {
            let hashes = hashes.clone();
            b.set_custom_verify_callback(SslVerifyMode::PEER, move |ssl| {
                let cert = ssl
                    .peer_certificate()
                    .ok_or(SslVerifyError::Invalid(SslAlert::CERTIFICATE_UNKNOWN))?;
                let digest = cert
                    .digest(MessageDigest::sha256())
                    .map_err(|_| SslVerifyError::Invalid(SslAlert::CERTIFICATE_UNKNOWN))?;
                if hashes.iter().any(|h| digest.as_ref() == h.as_slice()) {
                    Ok(())
                } else {
                    Err(SslVerifyError::Invalid(SslAlert::BAD_CERTIFICATE))
                }
            });
        }
    }

    Ok(b)
}
