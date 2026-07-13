# Client API

The `WebTransport` class is the client entry point. It mirrors the
[W3C WebTransport](https://w3c.github.io/webtransport/) interface, so the same
code that runs in a browser runs here, backed by the native quiche + BoringSSL
core.

```ts
import { WebTransport } from 'rwebtransport';

const wt = new WebTransport('https://example.com:4433/echo');
await wt.ready;
```

For the streams and datagrams surface (which the client shares with a server
session) see [`streams.md`](./streams.md) and [`datagrams.md`](./datagrams.md).
For error handling see [`errors.md`](./errors.md). For certificate options in
depth see [`certificates.md`](./certificates.md).

---

## Constructor

```ts
new WebTransport(url: string, options?: WebTransportOptions)
```

Construction is synchronous and returns immediately. It starts the QUIC handshake
and Extended CONNECT on a background thread; you wait for the result through
[`ready`](#ready). The constructor itself does no I/O, but it validates its
arguments and **throws a `WebTransportError` synchronously** when:

- `url` is not a string beginning with `https://`.
- a `serverCertificateHashes` entry uses an algorithm other than `sha-256`
  (case-insensitive).
- a `serverCertificateHashes` value is not exactly 32 bytes.

Wrap construction in `try`/`catch` if any of those inputs are dynamic:

```ts
import { WebTransport, WebTransportError } from 'rwebtransport';

try {
    const wt = new WebTransport(url, options);
    await wt.ready;
} catch (err) {
    if (err instanceof WebTransportError) {
        // bad URL, bad hash, or failed handshake
    }
    throw err;
}
```

### URL requirements

The URL must be an `https://` URL that includes the host, port, and path, for
example `https://localhost:4433/echo`. There is no `http://` or `wss://` form:
WebTransport always runs over HTTP/3 (QUIC) on UDP.

---

## `WebTransportOptions`

Every field is optional.

```ts
interface WebTransportOptions {
    serverCertificateHashes?: { algorithm: 'sha-256'; value: BufferSource }[];
    insecure?: boolean;
    headers?: Record<string, string>;
    origin?: string;

    // Accepted for spec parity, currently informational (see below).
    allowPooling?: boolean;
    requireUnreliable?: boolean;
    congestionControl?: 'default' | 'throughput' | 'low-latency';
}
```

### `serverCertificateHashes`

Accept the server certificate by the SHA-256 fingerprint of its DER encoding,
exactly like the browser API. When set, normal CA-chain and hostname validation
are bypassed for a certificate whose fingerprint matches. This is the right
choice for self-signed or pinned certificates (local development, private
infrastructure).

```ts
import { readFileSync } from 'node:fs';

// A 32-byte SHA-256 fingerprint of the server's certificate (DER).
const fingerprint: Uint8Array = readFileSync('server-cert.sha256');

const wt = new WebTransport('https://localhost:4433/echo', {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: fingerprint }],
});
await wt.ready;
```

Notes:

- `algorithm` must be `'sha-256'`; it is the only supported algorithm.
- `value` is any `BufferSource` (a `Uint8Array`, `Buffer`, typed array, `DataView`,
  or `ArrayBuffer`) and must be exactly 32 bytes.
- You may pass more than one hash; a match against any entry is accepted.
- Browser WebTransport additionally caps the certificate validity period for this
  mode. This client does not impose that cap.

### `insecure` (Node extension)

Disables **all** certificate verification: no CA chain, no hostname, no
fingerprint check. Development only, and never against a server you do not
control. It is ignored when `serverCertificateHashes` is set (fingerprint pinning
takes precedence).

```ts
const wt = new WebTransport('https://localhost:4433/echo', { insecure: true });
```

### `headers` (Node extension)

Extra request headers to send on the Extended CONNECT that establishes the
session. Useful for auth tokens or routing hints your server reads from
`session.headers`.

```ts
const wt = new WebTransport('https://example.com:4433/api', {
    headers: { authorization: 'Bearer ' + token },
});
```

### `origin`

The value to send in the `Origin` request header. Defaults to none.

```ts
const wt = new WebTransport('https://example.com:4433/echo', {
    origin: 'https://app.example.com',
});
```

### Informational options

`allowPooling`, `requireUnreliable`, and `congestionControl` are accepted so that
code written against the W3C interface type-checks and runs unchanged, but they
are **currently not wired to any behavior**. Setting them has no effect today.
Treat them as forward-compatible hints, not guarantees.

### Certificate verification: which mode applies

| You set                                          | Verification used                                 |
| ------------------------------------------------ | ------------------------------------------------- |
| neither `serverCertificateHashes` nor `insecure` | Full PKI: CA chain **and** hostname validation    |
| `serverCertificateHashes`                        | Fingerprint match only (CA and hostname bypassed) |
| `insecure: true` (and no hashes)                 | None (accepts any certificate)                    |
| both `serverCertificateHashes` and `insecure`    | Fingerprint match (`insecure` is ignored)         |

---

## `ready`

```ts
readonly ready: Promise<void>
```

Resolves once the session is fully established (the QUIC handshake completed and
the server answered the Extended CONNECT with `200`). After it resolves you can
open streams, read incoming streams, and send datagrams.

It **rejects** with a `WebTransportError` when the session cannot be established,
including:

- a certificate that fails the active verification mode (bad CA, wrong hostname,
  or a fingerprint that does not match),
- the server rejecting the CONNECT (any non-`200`, for example a `4xx`),
- an unresolvable host, refused connection, or handshake timeout.

```ts
await wt.ready; // throws WebTransportError on any of the above
```

Always `await ready` (or attach a `.catch`) before using the session; an
unhandled rejection here is the most common client mistake.

---

## `closed`

```ts
readonly closed: Promise<WebTransportCloseInfo>

interface WebTransportCloseInfo {
    closeCode: number;
    reason: string;
}
```

Resolves when the session closes cleanly, with the peer's close code and reason
(or the values you passed to [`close`](#close)). It **rejects** with a
`WebTransportError` if the session dies before it ever became ready, or if it
terminates abnormally (the connection was lost or reset rather than closed
gracefully).

```ts
wt.closed.then(
    (info) => console.log(`closed: code=${info.closeCode} reason=${info.reason}`),
    (err) => console.error('session terminated abnormally:', err),
);
```

`closed` and `ready` are independent promises: a session that fails to establish
rejects both.

---

## `createBidirectionalStream()`

```ts
createBidirectionalStream(): Promise<WebTransportBidirectionalStream>
```

Opens a new bidirectional stream. The returned object has a `.readable`
(`WebTransportReceiveStream`, a `ReadableStream<Uint8Array>`) and a `.writable`
(`WebTransportSendStream`, a `WritableStream<Uint8Array>`). Both halves carry the
same QUIC stream id, available as `.readable.streamId` and `.writable.streamId`.

```ts
const stream = await wt.createBidirectionalStream();

const writer = stream.writable.getWriter();
await writer.write(new TextEncoder().encode('ping'));
await writer.close(); // sends FIN

const reader = stream.readable.getReader();
const { value, done } = await reader.read();
if (!done) console.log('reply:', new TextDecoder().decode(value));
```

`write()` resolves once the bytes are accepted into the QUIC send buffer, which
gives you natural backpressure against the peer's flow-control window. See
[`streams.md`](./streams.md) for the full read/write, backpressure, and
reset semantics.

---

## `createUnidirectionalStream()`

```ts
createUnidirectionalStream(): Promise<WebTransportSendStream>
```

Opens a send-only stream. The result is a `WebTransportSendStream` (a
`WritableStream<Uint8Array>` with a `.streamId`). The peer receives it as an
incoming unidirectional (receive) stream.

```ts
const send = await wt.createUnidirectionalStream();
const writer = send.getWriter();
await writer.write(new TextEncoder().encode('event: tick'));
await writer.close();
```

---

## `incomingBidirectionalStreams`

```ts
readonly incomingBidirectionalStreams: ReadableStream<WebTransportBidirectionalStream>
```

A stream of the bidirectional streams the peer opens. Consume it with a reader
loop; it closes when the session closes.

```ts
const reader = wt.incomingBidirectionalStreams.getReader();
for (;;) {
    const { value: stream, done } = await reader.read();
    if (done) break;
    // Echo: read the request, write it back. Read while you write.
    void echo(stream);
}

async function echo(stream: WebTransportBidirectionalStream): Promise<void> {
    await stream.readable.pipeTo(stream.writable);
}
```

---

## `incomingUnidirectionalStreams`

```ts
readonly incomingUnidirectionalStreams: ReadableStream<WebTransportReceiveStream>
```

A stream of the send-only streams the peer opens; each element is a
`WebTransportReceiveStream` (a `ReadableStream<Uint8Array>`). It closes when the
session closes.

```ts
const reader = wt.incomingUnidirectionalStreams.getReader();
for (;;) {
    const { value: recv, done } = await reader.read();
    if (done) break;
    for await (const chunk of iterate(recv)) {
        console.log('got', chunk.byteLength, 'bytes on stream', recv.streamId);
    }
}

async function* iterate(rs: ReadableStream<Uint8Array>): AsyncGenerator<Uint8Array> {
    const r = rs.getReader();
    try {
        for (;;) {
            const { value, done } = await r.read();
            if (done) return;
            yield value;
        }
    } finally {
        r.releaseLock();
    }
}
```

---

## `datagrams`

```ts
readonly datagrams: WebTransportDatagramDuplexStream
```

The session's unreliable, unordered datagram transport. `datagrams.readable` and
`datagrams.writable` are ordinary `ReadableStream`/`WritableStream` of
`Uint8Array`. Sending is fire-and-forget: `write()` resolves quickly, oversized
datagrams are dropped, and inbound datagrams that overflow the queue are dropped
rather than buffered.

```ts
const dwriter = wt.datagrams.writable.getWriter();
await dwriter.write(new Uint8Array([1, 2, 3]));

const dreader = wt.datagrams.readable.getReader();
const { value } = await dreader.read();
```

See [`datagrams.md`](./datagrams.md) for `maxDatagramSize`, the high-water-mark
and max-age tuning knobs, and the loss model.

---

## `close()`

```ts
close(info?: { closeCode?: number; reason?: string }): void
```

Closes the session and every stream on it. Both fields are optional and default
to `closeCode: 0` and `reason: ''`. Calling `close` more than once is a no-op.
After it is called, `closed` resolves with the code and reason you supplied.

```ts
wt.close(); // clean close, code 0
wt.close({ closeCode: 17, reason: 'done' }); // with an application code
```

---

## Full annotated example

A complete client that connects, runs one request/response over a bidirectional
stream, sends a datagram, and closes cleanly. Written as ESM; runnable with
Node 24 or 26.

```ts
import { WebTransport, WebTransportError } from 'rwebtransport';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

async function main(): Promise<void> {
    // 1. Construct. This is synchronous and does no I/O yet, but it validates
    //    the URL and any cert hashes and can throw a WebTransportError.
    const wt = new WebTransport('https://localhost:4433/echo', {
        // Pin a self-signed cert by its 32-byte SHA-256 fingerprint. In dev you
        // could instead pass `insecure: true`. With neither, full CA + hostname
        // validation applies.
        serverCertificateHashes: [{ algorithm: 'sha-256', value: loadFingerprint() }],
        headers: { 'x-client': 'demo' },
    });

    // 2. Surface abnormal termination without an unhandled rejection.
    wt.closed.then(
        (info) => console.log(`closed cleanly: ${info.closeCode} ${info.reason}`),
        (err) => console.error('session died:', err),
    );

    // 3. Wait for the handshake + CONNECT. Rejects on bad cert, non-200, or an
    //    unreachable host.
    try {
        await wt.ready;
    } catch (err) {
        if (err instanceof WebTransportError) {
            console.error('could not connect:', err.message);
            return;
        }
        throw err;
    }

    // 4. One request/response over a bidirectional stream. Note we close the
    //    writable (sending FIN) so the echo server knows the request is done,
    //    then drain the readable.
    const stream = await wt.createBidirectionalStream();

    const writer = stream.writable.getWriter();
    await writer.write(encoder.encode('hello over quic'));
    await writer.close();

    const reader = stream.readable.getReader();
    const chunks: Uint8Array[] = [];
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        chunks.push(value);
    }
    console.log('reply:', decoder.decode(concat(chunks)));

    // 5. Fire off an unreliable datagram (best effort, may be dropped).
    const dgram = wt.datagrams.writable.getWriter();
    await dgram.write(encoder.encode('ping'));
    dgram.releaseLock();

    // 6. Close the session with an application code and reason.
    wt.close({ closeCode: 0, reason: 'done' });
}

function concat(chunks: Uint8Array[]): Uint8Array {
    const total = chunks.reduce((n, c) => n + c.byteLength, 0);
    const out = new Uint8Array(total);
    let offset = 0;
    for (const c of chunks) {
        out.set(c, offset);
        offset += c.byteLength;
    }
    return out;
}

function loadFingerprint(): Uint8Array {
    // Replace with your server certificate's real 32-byte SHA-256 DER fingerprint.
    return new Uint8Array(32);
}

main().catch((err) => {
    console.error(err);
    process.exitCode = 1;
});
```

Runnable client and server programs live in [`../examples`](../examples).

---

## See also

- [`server.md`](./server.md) for `WebTransportServer` and the server-side session.
- [`streams.md`](./streams.md) for stream reading, writing, backpressure, and resets.
- [`datagrams.md`](./datagrams.md) for the datagram transport and its tuning knobs.
- [`errors.md`](./errors.md) for `WebTransportError` and its `source` / `streamErrorCode`.
- [`certificates.md`](./certificates.md) for TLS, fingerprints, and verification modes.
