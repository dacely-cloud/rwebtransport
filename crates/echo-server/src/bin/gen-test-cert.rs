// SPDX-License-Identifier: Apache-2.0
//! Generate a short-lived EC P-256 self-signed certificate for the test suite.
//!
//! The WebTransport `serverCertificateHashes` contract requires a pinned leaf to
//! be currently valid and to span at most 14 days, so the client now enforces
//! that. A long-lived committed fixture cannot satisfy it, so the test cert is
//! minted fresh (13-day validity) on every run instead.
//!
//! Usage: `gen-test-cert <cert.pem> <key.pem> [days] [keyType]`. `days` defaults
//! to 13; `keyType` is `ec` (P-256, the default) or `rsa` (used by tests that
//! assert a non-P-256 pinned cert is rejected).

use std::fs;

use boring::asn1::Asn1Time;
use boring::bn::BigNum;
use boring::ec::{EcGroup, EcKey};
use boring::hash::MessageDigest;
use boring::nid::Nid;
use boring::pkey::PKey;
use boring::rsa::Rsa;
use boring::x509::extension::SubjectAlternativeName;
use boring::x509::{X509NameBuilder, X509};

fn main() {
    let mut args = std::env::args().skip(1);
    let (cert_path, key_path) = match (args.next(), args.next()) {
        (Some(c), Some(k)) => (c, k),
        _ => {
            eprintln!("usage: gen-test-cert <cert.pem> <key.pem> [days] [ec|rsa]");
            std::process::exit(2);
        }
    };
    // Validity span in days; 13 keeps within the 14-day pinning ceiling. A larger
    // value is used by tests that assert the client rejects an over-long cert.
    let days: u32 = args.next().and_then(|d| d.parse().ok()).unwrap_or(13);
    let key_type = args.next().unwrap_or_else(|| "ec".to_string());

    let pkey = if key_type == "rsa" {
        let rsa = Rsa::generate(2048).expect("generate RSA key");
        PKey::from_rsa(rsa).expect("wrap RSA key")
    } else {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).expect("P-256 group");
        let ec = EcKey::generate(&group).expect("generate EC key");
        PKey::from_ec_key(ec).expect("wrap EC key")
    };

    let mut name = X509NameBuilder::new().expect("name builder");
    name.append_entry_by_text("CN", "localhost").expect("CN");
    let name = name.build();

    let mut b = X509::builder().expect("x509 builder");
    b.set_version(2).expect("v3"); // X.509 v3 (0-indexed)

    let serial = BigNum::from_u32(1)
        .and_then(|n| n.to_asn1_integer())
        .expect("serial");
    b.set_serial_number(&serial).expect("serial");

    b.set_subject_name(&name).expect("subject");
    b.set_issuer_name(&name).expect("issuer");
    b.set_pubkey(&pkey).expect("pubkey");

    let not_before = Asn1Time::days_from_now(0).expect("not_before");
    let not_after = Asn1Time::days_from_now(days).expect("not_after");
    b.set_not_before(&not_before).expect("set not_before");
    b.set_not_after(&not_after).expect("set not_after");

    let san = SubjectAlternativeName::new()
        .dns("localhost")
        .ip("127.0.0.1")
        .ip("::1")
        .build(&b.x509v3_context(None, None))
        .expect("SAN");
    b.append_extension(san).expect("append SAN");

    b.sign(&pkey, MessageDigest::sha256()).expect("sign");
    let cert = b.build();

    fs::write(&cert_path, cert.to_pem().expect("cert pem")).expect("write cert");
    fs::write(&key_path, pkey.private_key_to_pem_pkcs8().expect("key pem")).expect("write key");
}
