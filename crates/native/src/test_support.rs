// SPDX-License-Identifier: Apache-2.0
//! Real QUIC pairs for native protocol regression tests.
use crate::config::{build_config, build_server_config, ClientConfigParams};
use boring::{
    asn1::Asn1Time,
    bn::BigNum,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::PKey,
    x509::{
        extension::{BasicConstraints, SubjectAlternativeName},
        X509NameBuilder, X509,
    },
};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn pair(
    expected_ip: Option<std::net::IpAddr>,
    name: Option<&str>,
    ip_san: bool,
) -> (quiche::Connection, quiche::Connection) {
    let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
    let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
    let mut subject = X509NameBuilder::new().unwrap();
    subject.append_entry_by_text("CN", "wrong.example").unwrap();
    let subject = subject.build();
    let mut cert = X509::builder().unwrap();
    cert.set_version(2).unwrap();
    cert.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
        .unwrap();
    cert.set_subject_name(&subject).unwrap();
    cert.set_issuer_name(&subject).unwrap();
    cert.set_pubkey(&key).unwrap();
    cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
        .unwrap();
    cert.append_extension(BasicConstraints::new().critical().ca().build().unwrap())
        .unwrap();
    let mut san = SubjectAlternativeName::new();
    san.dns("wrong.example");
    if ip_san {
        san.ip("127.0.0.1").ip("::1");
    }
    let san = san.build(&cert.x509v3_context(None, None)).unwrap();
    cert.append_extension(san).unwrap();
    cert.sign(&key, MessageDigest::sha256()).unwrap();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "rwt-native-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&dir).unwrap();
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    std::fs::write(&cert_path, cert.build().to_pem().unwrap()).unwrap();
    std::fs::write(&key_path, key.private_key_to_pem_pkcs8().unwrap()).unwrap();
    let mut cc = build_config(&ClientConfigParams {
        expected_ip,
        ..Default::default()
    })
    .unwrap();
    cc.load_verify_locations_from_file(cert_path.to_str().unwrap())
        .unwrap();
    let mut sc =
        build_server_config(cert_path.to_str().unwrap(), key_path.to_str().unwrap()).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    let ca = "127.0.0.1:1234".parse().unwrap();
    let sa = "127.0.0.1:4433".parse().unwrap();
    let client = quiche::connect(
        name,
        &quiche::ConnectionId::from_ref(&[1; 16]),
        ca,
        sa,
        &mut cc,
    )
    .unwrap();
    let server = quiche::accept(
        &quiche::ConnectionId::from_ref(&[2; 16]),
        None,
        sa,
        ca,
        &mut sc,
    )
    .unwrap();
    (client, server)
}

pub fn exchange(client: &mut quiche::Connection, server: &mut quiche::Connection) {
    let mut buf = [0; 1350];
    for _ in 0..100 {
        let mut moved = false;
        while let Ok((n, info)) = client.send(&mut buf) {
            moved = true;
            let _ = server.recv(
                &mut buf[..n],
                quiche::RecvInfo {
                    from: info.from,
                    to: info.to,
                },
            );
        }
        while let Ok((n, info)) = server.send(&mut buf) {
            moved = true;
            let _ = client.recv(
                &mut buf[..n],
                quiche::RecvInfo {
                    from: info.from,
                    to: info.to,
                },
            );
        }
        if !moved {
            break;
        }
    }
}
