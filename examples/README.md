# rwebtransport examples

Small, runnable ESM (`.mjs`) programs that exercise the real
[`rwebtransport`](../README.md) API: an all-in-one echo demo, a standalone echo
server, and a client you point at any WebTransport endpoint.

## 1. Build the package first

The examples import the freshly built library from `../dist/`, so build it once
from the repository root before running anything:

```bash
npm install
npm run build
```

`npm install` fetches (or, on unsupported platforms, compiles) the native addon;
`npm run build` produces `dist/index.mjs` plus the prebuilt `.node` binary.
rwebtransport runs on Node 24 and Node 26 only (it throws on other versions).

## 2. Generate a dev certificate

The server needs a PEM certificate chain and private key, and the client pins
that certificate by its SHA-256 fingerprint (exactly like the browser API), so
no public CA is involved. Generate a short-lived self-signed cert with the
helper script:

```bash
cd examples
./generate-cert.sh
```

or run the equivalent openssl commands by hand (P-256 key, `localhost` +
loopback SANs), which write `cert.pem` and `key.pem` into the current directory:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout key.pem -out cert.pem -days 14 \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1"
```

Compute the certificate hash hex that `client.mjs` expects (SHA-256 of the DER
form) at any time with:

```bash
openssl x509 -in cert.pem -outform der | openssl dgst -sha256 -r | cut -d' ' -f1
```

The cert is valid for 14 days; regenerate it (and reconnect with the new hash)
once it expires.

## 3. Run an example

Run each from the repository root after building. One line each:

- `node examples/echo-demo.mjs` starts an in-process `WebTransportServer` and a `WebTransport` client on loopback, then round-trips a bidirectional stream and a datagram end to end (self-contained: it reads `examples/cert.pem` / `examples/key.pem` and derives the pinned hash itself, so no arguments are needed).
- `node examples/server.mjs` runs a standalone echo server on `127.0.0.1` (reads `examples/cert.pem` / `examples/key.pem`) and prints its `https://` URL and the certificate hash hex to paste into the client.
- `node examples/client.mjs <url> <certHashHex>` connects to any WebTransport server (for example the one printed by `server.mjs`), opens a bidi stream, sends a message, prints the echoed reply, then sends and reads back a datagram; `<certHashHex>` is the SHA-256 hex from step 2 and is passed as `serverCertificateHashes: [{ algorithm: 'sha-256', value }]`.

See [../README.md](../README.md) for the full API reference and
[../scripts/bench.mjs](../scripts/bench.mjs) for a loopback micro-benchmark that
uses the same client and server surface.
