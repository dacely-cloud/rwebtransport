# Getting started

This is a hands-on walkthrough. By the end you will have a running WebTransport
server and a client that connects to it over real QUIC / HTTP-3, opens a
bidirectional stream, and gets its bytes echoed back. Everything runs locally
with a self-signed certificate that the client trusts by its SHA-256
fingerprint, exactly the way the browser API does with `serverCertificateHashes`.

Total time: a few minutes. No public CA, no DNS, no reverse proxy.

## Prerequisites

- **Node.js 24.x or 26.x.** These are the only supported versions; `rwebtransport`
  throws on any other. Check with `node --version`.
- **`openssl`** on your `PATH` (version 1.1.1 or newer, for the `-addext` flag).
  Preinstalled on macOS and most Linux distributions.
- A working directory to hold the four files we create: `cert.pem`, `key.pem`,
  `server.mjs`, and `client.mjs`.

```bash
mkdir wt-quickstart && cd wt-quickstart
```

## 1. Install

```bash
npm install rwebtransport
```

Prebuilt native binaries ship for Node 24 and 26 on Linux (x64 and arm64),
macOS (Apple Silicon), and Windows (x64), so this is a normal install. If no
prebuilt binary matches your platform (for example an Intel Mac), the package
compiles the Rust core at install time (that path needs a Rust toolchain,
`cmake`, and a C/C++ compiler). See the
[README](../README.md#install) for details.

## 2. Generate a development certificate

WebTransport always runs over TLS, so we need a certificate and a private key,
even locally. We generate a P-256 (prime256v1) key and a self-signed certificate
whose Subject Alternative Name (SAN) covers `localhost` and `127.0.0.1`.

```bash
# 1. A P-256 (prime256v1) private key.
openssl ecparam -name prime256v1 -genkey -noout -out key.pem

# 2. A self-signed certificate for that key, with a SAN for localhost.
openssl req -new -x509 -key key.pem -out cert.pem -days 14 \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

You now have `key.pem` (the private key) and `cert.pem` (the certificate). A few
notes:

- **P-256** (`prime256v1`, also called `secp256r1`) is a modern, widely
  supported curve for the TLS 1.3 handshake.
- **The SAN matters.** A certificate without a SAN entry for the name you connect
  to is rejected under normal PKI validation. When you pin the certificate by its
  hash (step 4) the client skips CA and hostname checks for that exact cert, but
  we still add the SAN so the same cert works everywhere.
- **`-days 14`** keeps the validity short. Browsers cap hash-pinned certificates
  at 14 days, so a short-lived cert mirrors that convention; regenerate it when
  it expires.

If your `openssl` is older than 1.1.1 and rejects `-addext`, put the SAN in a
small config file instead:

```bash
cat > san.cnf <<'EOF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = localhost
[ext]
subjectAltName = DNS:localhost,IP:127.0.0.1
EOF

openssl req -new -x509 -key key.pem -out cert.pem -days 14 -config san.cnf
```

## 3. Write the server

The server binds a UDP port with the certificate, then consumes
`incomingSessions`. For each session we read its incoming bidirectional streams
and pipe each stream's readable half straight back into its writable half: a
one-line echo.

Save this as `server.mjs`:

```js
// server.mjs
import { WebTransportServer } from 'rwebtransport';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

const server = new WebTransportServer({
    port: 4433,
    host: '127.0.0.1',
    cert: join(here, 'cert.pem'), // PEM certificate file path
    key: join(here, 'key.pem'), // PEM private-key file path
});

await server.ready;
console.log(`listening on https://127.0.0.1:${server.port}/echo`);

// Accept sessions as clients connect.
const sessions = server.incomingSessions.getReader();
for (;;) {
    const { value: session, done } = await sessions.read();
    if (done) break;
    console.log('session opened:', session.path);
    handleSession(session).catch((err) => console.error('session error:', err));
}

async function handleSession(session) {
    // Echo every bidirectional stream the client opens.
    const streams = session.incomingBidirectionalStreams.getReader();
    for (;;) {
        const { value: stream, done } = await streams.read();
        if (done) break;
        // readable -> writable on the same stream: the bytes go right back.
        stream.readable.pipeTo(stream.writable).catch(() => {});
    }
}
```

Two things worth noting:

- `cert` and `key` are **file paths**, not the PEM text itself.
- `handleSession` is not awaited in the accept loop, so the server keeps
  accepting new sessions while each one runs its own echo loop.

## 4. Write the client

The client connects to the server and trusts the self-signed certificate by
pinning its SHA-256 fingerprint. We compute that fingerprint the same way the
browser does: take the certificate's DER bytes (`X509Certificate.raw`) and hash
them with SHA-256. The result is a 32-byte `Uint8Array` that goes straight into
`serverCertificateHashes`.

Save this as `client.mjs`:

```js
// client.mjs
import { WebTransport } from 'rwebtransport';
import { X509Certificate, createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

// SHA-256 of the certificate's DER encoding: the fingerprint we pin.
const der = new X509Certificate(readFileSync(join(here, 'cert.pem'))).raw;
const certHash = new Uint8Array(createHash('sha256').update(der).digest());

const wt = new WebTransport('https://127.0.0.1:4433/echo', {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash }],
});

await wt.ready;
console.log('connected');

// Open a bidirectional stream and send a message.
const stream = await wt.createBidirectionalStream();

const writer = stream.writable.getWriter();
await writer.write(new TextEncoder().encode('hello over QUIC'));
await writer.close(); // sends FIN so the server sees end-of-input

// Read the echo back until the server closes its side.
const reader = stream.readable.getReader();
const decoder = new TextDecoder();
let received = '';
for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    received += decoder.decode(value, { stream: true });
}
received += decoder.decode();
console.log('server echoed:', received);

wt.close({ closeCode: 0, reason: 'done' });
await wt.closed;
```

Because we set `serverCertificateHashes`, the client accepts this one certificate
by its fingerprint and skips CA and hostname validation for it. Drop that option
and the client would do full PKI validation against the system trust store and
reject a self-signed cert, which is what you want in production.

## 5. Run it

Open two terminals in the same directory.

Terminal 1, start the server:

```bash
node server.mjs
```

You should see:

```
listening on https://127.0.0.1:4433/echo
```

Terminal 2, run the client:

```bash
node client.mjs
```

Expected output:

```
connected
server echoed: hello over QUIC
```

Meanwhile the server prints `session opened: /echo`. The client exits on its own
after `wt.closed` resolves; stop the server with Ctrl+C.

## What just happened

1. The client did a QUIC + TLS 1.3 handshake with the server, accepting its
   certificate because the SHA-256 of the certificate's DER bytes matched the
   hash you pinned.
2. On top of that connection it performed an HTTP-3 Extended CONNECT to `/echo`,
   which established the WebTransport session. The server auto-accepts any valid
   WebTransport CONNECT and surfaces it on `incomingSessions`, already
   established (its `ready` is already resolved).
3. The client opened one bidirectional stream, a `WebTransportBidirectionalStream`
   whose `.writable` is a `WritableStream<Uint8Array>` and whose `.readable` is a
   `ReadableStream<Uint8Array>`. Writing then closing the writer sent the bytes
   plus a FIN.
4. The server received the stream on `incomingBidirectionalStreams` and piped its
   readable half back into its writable half, echoing the bytes and forwarding
   the FIN.
5. The client read until `done`, printed the echo, and closed the session
   cleanly, which resolved `wt.closed`.

## Troubleshooting

- **`throws on Node version`**: you are not on Node 24.x or 26.x. Check
  `node --version`.
- **`ready` rejects with a certificate error**: the pinned hash does not match
  the server's certificate. Make sure both files read the same `cert.pem`, and
  regenerate the cert if it expired (we set a 14-day validity).
- **Connection refused / no session**: confirm the server printed its listening
  line first, and that the client URL host, port, and scheme (`https://`) match
  the server. The URL must be `https://host:port/path`.
- **`-addext` is not recognized**: your `openssl` predates 1.1.1; use the config
  file variant in step 2.

## Next steps

- **Streams and datagrams in depth**: see the client API in the
  [README](../README.md#the-client-api), including unidirectional streams,
  datagrams, and backpressure.
- **The server API**: see the [README](../README.md#the-server-api) for session
  metadata (`authority`, `path`, `origin`, `headers`) and server-initiated
  streams.
- **Certificates and TLS**: the same pinning approach scales to CA-issued
  certificates by simply omitting `serverCertificateHashes`, at which point the
  client validates against the system trust store with hostname checking.
