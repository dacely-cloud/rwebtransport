# Troubleshooting

A practical FAQ for the most common problems you will hit with
[`rwebtransport`](../README.md). Each entry lists the symptom, the cause, and
the fix.

- [Unsupported Node version](#unsupported-node-version)
- [No prebuilt binary / compiling at install](#no-prebuilt-binary--compiling-at-install)
- [Certificate errors on connect](#certificate-errors-on-connect)
- [A large bidi transfer hangs](#a-large-bidi-transfer-hangs)
- [Server port already in use / cluster port sharing](#server-port-already-in-use--cluster-port-sharing)
- [Enabling native debug logging (RWT_DEBUG)](#enabling-native-debug-logging-rwt_debug)

---

## Unsupported Node version

**Symptom.** Importing or using the package throws:

```
rwebtransport supports Node 24 and Node 26 only; this is Node 22.14.0.
```

During `npm install` you may instead see a non-fatal warning:

```
rwebtransport: warning: Node 22.14.0 is unsupported (need 24 or 26).
```

**Cause.** The native addon is built and tested only against Node 24 and
Node 26. The loader checks `process.versions.node` and refuses to run on any
other major version, because the N-API and V8 ABI it was compiled against are
not guaranteed to match.

**Fix.** Run on Node 24 or Node 26. Check your version and switch:

```bash
node --version          # must print v24.x or v26.x
nvm install 26 && nvm use 26   # if you use nvm
```

If a build tool (a bundler, a test runner, an IDE task) launches a different
Node than your shell, point it at a supported binary explicitly.

---

## No prebuilt binary / compiling at install

**Symptom.** Install prints something like:

```
rwebtransport: no prebuilt binary for this platform; compiling the native addon...
```

or, on first use if the postinstall build was skipped:

```
rwebtransport: no prebuilt binary for this platform; compiling the native addon (this happens once)...
```

**Cause.** Prebuilt binaries ship for Node 24 and Node 26 on linux, macOS, and
Windows (x64 and arm64). The loader looks for
`prebuilds/<platform>-<arch>/rwebtransport.node`. If your platform/arch is not
covered, the package compiles the Rust core from source on the fly. This is
expected and happens once; the result is cached for subsequent loads.

**What you need to compile.** A working native toolchain:

- **Rust** (via [rustup](https://rustup.rs)).
- **cmake**.
- A **C/C++ compiler** (needed to build BoringSSL through quiche).
- On **Windows only**, additionally **NASM**.

If the toolchain is missing, install prints:

```
rwebtransport: no prebuilt binary for this platform and no Rust toolchain found;
rwebtransport: install Rust (https://rustup.rs) + cmake, or use a supported prebuilt platform.
```

Install the tools above, then build manually:

```bash
npm run build        # builds the Rust core and the TypeScript
# or just the native addon:
npm run build:rust
```

If you ever see:

```
rwebtransport: no prebuilt binary at <path> and no build script to compile one.
```

then the package files are incomplete. Reinstall it, or build the addon
manually with `npm run build:rust`.

---

## Certificate errors on connect

**Symptom.** `await transport.ready` rejects with a `WebTransportError` whose
`source` is `"session"`, for a self-signed or otherwise untrusted server
certificate. Always inspect the rejection:

```ts
const transport = new WebTransport('https://localhost:4433/');
try {
    await transport.ready;
} catch (err) {
    // err is a WebTransportError; err.name === "WebTransportError"
    console.error(err.name, (err as Error).message);
}
```

**Cause.** When neither `serverCertificateHashes` nor `insecure` is set, the
client performs full PKI + hostname validation, exactly like a browser. A
self-signed cert, a hostname mismatch, or an untrusted CA fails that check.

**Fix (recommended): pin the certificate by its SHA-256 hash.** This accepts a
specific server certificate by its SHA-256 DER fingerprint and bypasses CA and
hostname validation, the same mechanism browsers expose. Compute the hash from
the server's PEM certificate and pass it in:

```ts
import { readFileSync } from 'node:fs';
import { createHash, X509Certificate } from 'node:crypto';
import { WebTransport } from 'rwebtransport';

// DER bytes of the server's leaf certificate, then its SHA-256.
const der = new X509Certificate(readFileSync('server-cert.pem')).raw;
const value = createHash('sha-256').update(der).digest(); // 32-byte Buffer

const transport = new WebTransport('https://localhost:4433/', {
    serverCertificateHashes: [{ algorithm: 'sha-256', value }],
});
await transport.ready;
```

Notes:

- `algorithm` must be `"sha-256"` (the only supported value). Anything else
  throws `unsupported certificate hash algorithm: ...`.
- `value` must be exactly 32 bytes, or the client throws
  `sha-256 certificate hash must be 32 bytes`. Any `BufferSource` works
  (`Buffer`, `Uint8Array`, `ArrayBuffer`).

**Fix (dev only): disable verification entirely.** The `insecure` option turns
off **all** certificate verification. Use it only for local development, never
in production:

```ts
const transport = new WebTransport('https://localhost:4433/', {
    insecure: true, // disables ALL verification; dev only
});
await transport.ready;
```

For how to generate a matching cert/key pair for the server side, see the
server options in the [README](../README.md).

---

## A large bidi transfer hangs

**Symptom.** You open a bidirectional stream, write a large amount of data, and
the program stalls forever. `writer.write(...)` never resolves and the process
appears wedged.

**Cause.** This is universal to stream APIs, not specific to this package.
`write()` resolves only once the bytes are accepted into the QUIC send buffer.
Once that buffer and the peer's flow-control window fill up, further writes
block until the peer reads. If you write the whole stream **before** you start
reading its readable side, and the peer is echoing or otherwise waiting on you
to read, both ends stall waiting for the other. You must **read while you
write**.

**Wrong** (writes everything, then reads: can deadlock on large transfers):

```js
const stream = await transport.createBidirectionalStream();
const writer = stream.writable.getWriter();
for (const chunk of chunks) {
    await writer.write(chunk); // eventually blocks and never returns
}
await writer.close();
// reading only starts here, too late
```

**Right** (drain the readable concurrently with writing the writable):

```js
const stream = await transport.createBidirectionalStream();

async function writeAll(writable) {
    const writer = writable.getWriter();
    for (const chunk of chunks) {
        await writer.write(chunk);
    }
    await writer.close();
}

async function readAll(readable) {
    const reader = readable.getReader();
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        // consume value (a Uint8Array) here
    }
}

// Both directions make progress at once, so neither side can wedge.
await Promise.all([writeAll(stream.writable), readAll(stream.readable)]);
```

The same rule applies when piping: pipe the readable somewhere as you write,
rather than fully writing first. Read backpressure is automatic, so if you stop
reading, the peer is flow-controlled and its writes stall in turn.

---

## Server port already in use / cluster port sharing

**Symptom.** `await server.ready` rejects on startup (a bind failure such as
`EADDRINUSE`), or a second `cluster` worker fails to start a server on a port a
sibling already bound.

**Cause and fixes.**

**Port already in use.** Another process (or a previous run that has not fully
exited) holds the UDP port. The server surfaces bind failures by rejecting
`ready`, so always await it:

```js
import { WebTransportServer } from 'rwebtransport';

const server = new WebTransportServer({
    port: 4433,
    host: '0.0.0.0',
    cert: '/path/to/cert.pem',
    key: '/path/to/key.pem',
});

try {
    await server.ready;
    console.log('listening on', server.port);
} catch (err) {
    console.error('server failed to start:', err);
}
```

To avoid clashes entirely, bind port `0` to let the OS assign a free port, then
read it back from `.port` after `ready`:

```js
const server = new WebTransportServer({ port: 0, cert, key });
await server.ready;
console.log('OS-assigned port:', server.port); // valid once ready resolves
```

**cluster port sharing.** To let several `cluster` workers share one server
port, pass `reusePort: true`. The library sets `SO_REUSEPORT` and the kernel
load-balances inbound QUIC connections across the workers:

```js
const server = new WebTransportServer({ port: 4433, cert, key, reusePort: true });
await server.ready; // every worker can bind the same port
```

`SO_REUSEPORT` is Unix-only (Linux and the BSDs, including macOS). On Windows the
flag is ignored, so the second worker to bind the same port still fails with
`EADDRINUSE`. Portable options that work everywhere:

- Run **one server per port** (give each worker its own port), or
- Put a **UDP load balancer** in front and fan connections out to per-worker
  ports.

Note that `cluster` **clients** are unaffected: each worker is an independent
process and can create its own `WebTransport` client freely. If you want
multiple concurrent sessions inside one process instead, use
`worker_threads`, which is fully supported: a client or a server can be created
in any Worker, and multiple Workers run concurrently.

---

## Enabling native debug logging (RWT_DEBUG)

**When to use it.** If a session will not establish, a stream behaves oddly, or
a transfer stalls and the JS-side error is not specific enough, turn on the
native trace logging from the Rust core.

**How.** Set the `RWT_DEBUG` environment variable to any non-empty value (the
code checks only that it is set) before starting your process. Logs are written
to **stderr**:

```bash
RWT_DEBUG=1 node app.mjs
```

On Windows (PowerShell):

```powershell
$env:RWT_DEBUG = '1'; node app.mjs
```

This surfaces per-session driver events (handshake progress, stream lifecycle,
flush activity) from the background thread that runs each session, which is
usually enough to tell a certificate/handshake failure apart from a
flow-control stall. It is verbose, so enable it only while diagnosing and leave
it off in production.

---

If none of the above resolves your issue, capture an `RWT_DEBUG=1` stderr log
and the exact `WebTransportError` (`name`, `message`, `source`,
`streamErrorCode`) and open an issue.
