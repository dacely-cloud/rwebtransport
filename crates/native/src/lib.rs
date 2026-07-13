// SPDX-License-Identifier: Apache-2.0
//! rwebtransport native addon (de-risk skeleton).
//!
//! At this stage the module only proves the full native toolchain links and
//! loads: neon (N-API) + vendored Cloudflare quiche + BoringSSL (`boring`).
//! The real WebTransport client is layered on top in later modules.

use neon::prelude::*;

/// Smoke-test export: touches quiche + boring so both are linked into the
/// cdylib, and returns a small diagnostic string to JS.
fn info(mut cx: FunctionContext) -> JsResult<JsString> {
    // Touch quiche so it is linked.
    let pv = quiche::PROTOCOL_VERSION;
    let supported = quiche::version_is_supported(pv);

    // Touch BoringSSL via the `boring` crate so it is linked, and confirm we can
    // build a TLS context (the same primitive the real client configures).
    let boring_ok = boring::ssl::SslContextBuilder::new(boring::ssl::SslMethod::tls()).is_ok();

    Ok(cx.string(format!(
        "rwebtransport-native ok; quiche_protocol_version=0x{pv:08x} supported={supported} boringssl={boring_ok}"
    )))
}

#[neon::main]
fn main(mut cx: ModuleContext) -> NeonResult<()> {
    cx.export_function("info", info)?;
    Ok(())
}
