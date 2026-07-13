#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# generate-cert.sh
#
# Generates a self-signed EC (prime256v1 / P-256) certificate and key for the
# rwebtransport examples in this folder. The output pair is written next to this
# script as cert.pem and key.pem:
#
#   cert.pem  ->  PEM certificate chain  (WebTransportServerOptions.cert)
#   key.pem   ->  PEM private key        (WebTransportServerOptions.key)
#
# Both are FILE PATHS you hand to `new WebTransportServer({ port, cert, key })`.
#
# The certificate is valid for ~90 days with CN=localhost and
# subjectAltName = DNS:localhost, IP:127.0.0.1, IP:::1 so that a browser-style
# client connecting to https://localhost:PORT/ validates the hostname.
#
# At the end this prints the DER SHA-256 fingerprint of the certificate. Pin it
# from a client to accept this exact cert without a CA (bypasses CA + hostname,
# just like the browser):
#
#   const hash = { algorithm: "sha-256", value: <32-byte Uint8Array> };
#   new WebTransport("https://localhost:PORT/path", {
#     serverCertificateHashes: [hash],
#   });
#
# Note: browsers only accept pinned serverCertificateHashes for certs valid at
# most 14 days. rwebtransport does not impose that limit, so this 90-day cert is
# fine for the examples; regenerate more often if you mirror the browser rule.

set -euo pipefail

# Resolve the directory this script lives in, so it works from any cwd.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CERT_PATH="${SCRIPT_DIR}/cert.pem"
KEY_PATH="${SCRIPT_DIR}/key.pem"
DAYS=90

if ! command -v openssl >/dev/null 2>&1; then
  echo "error: openssl not found on PATH" >&2
  exit 1
fi

echo "Generating P-256 (prime256v1) private key: ${KEY_PATH}"
openssl ecparam -name prime256v1 -genkey -noout -out "${KEY_PATH}"

echo "Generating self-signed certificate (~${DAYS} days): ${CERT_PATH}"
openssl req -new -x509 \
  -key "${KEY_PATH}" \
  -out "${CERT_PATH}" \
  -days "${DAYS}" \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1"

# Lock down the private key; it is only meant for local development.
chmod 600 "${KEY_PATH}"

echo
echo "Done. Wrote:"
echo "  cert  -> ${CERT_PATH}"
echo "  key   -> ${KEY_PATH}"
echo
echo "DER SHA-256 fingerprint (pin this via serverCertificateHashes):"
openssl x509 -in "${CERT_PATH}" -outform DER | openssl dgst -sha256
