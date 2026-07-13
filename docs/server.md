# The server API

`WebTransportServer` is the accepting side of `rwebtransport`: it binds a UDP port with a TLS certificate, speaks HTTP/3 and the WebTransport Extended CONNECT handshake, and hands you one `WebTransportServerSession` per client connection. A server session has the exact same stream and datagram surface as the client `WebTransport` (see [client.md](./client.md)), plus the request metadata from the client's CONNECT (`authority`, `path`, `origin`, `headers`).

One WebTransport session maps to one QUIC connection. The server auto-accepts any valid WebTransport CONNECT and answers `200`, so a session is already established by the time you receive it. To turn a client away, you close the session right after it is surfaced (see [Accepting and rejecting sessions](#accepting-and-rejecting-sessions)).

```ts
import { WebTransportServer } from 'rwebtransport';

const server = new WebTransportServer({
    port: 4433,
    host: '0.0.0.0',
    cert: '/path/to/cert.pem', // PEM certificate chain (a file path)
    key: '/path/to/key.pem', // PEM private key (a file path)
});

await server.ready;
console.log(`listening on udp/${server.port}`);
```

## Contents

- [Creating a server](#creating-a-server)
- [`WebTransportServerOptions`](#webtransportserveroptions)
- [`ready`, `closed`, and `port`](#ready-closed-and-port)
- [Consuming `incomingSessions`](#consuming-incomingsessions)
- [`WebTransportServerSession`](#webtransportserversession)
- [Accepting and rejecting sessions](#accepting-and-rejecting-sessions)
- [Server-initiated streams and datagrams](#server-initiated-streams-and-datagrams)
- [Closing the server](#closing-the-server)
- [Full example: a per-path router](#full-example-a-per-path-router)
- [See also](#see-also)

## Creating a server

```ts
import { WebTransportServer } from 'rwebtransport';

const server = new WebTransportServer(options);
```

The constructor takes a single `WebTransportServerOptions` object and starts binding immediately. Binding is asynchronous: wait for [`server.ready`](#ready-closed-and-port) before you rely on the socket being open or read `server.port`.

You need a certificate and a matching private key on disk. For local development you can generate a self-signed pair with the script in [`../examples/generate-cert.sh`](../examples/generate-cert.sh); for the trust models (full PKI, pinning a self-signed cert from the client, the `insecure` dev switch) see [certificates.md](./certificates.md).

## `WebTransportServerOptions`

```ts
interface WebTransportServerOptions {
    port: number; // UDP port to listen on; 0 lets the OS pick one
    host?: string; // interface to bind, default "0.0.0.0"
    cert: string; // path to the PEM certificate-chain file
    key: string; // path to the PEM private-key file
    reusePort?: boolean; // share the port across processes (Unix), default false
}
```

| Field       | Type      | Required | Meaning                                                                                                                                                                                                                                                                    |
| ----------- | --------- | -------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `port`      | `number`  | yes      | UDP port to bind. Pass `0` to let the OS assign a free port, then read the actual value back from [`server.port`](#ready-closed-and-port) after `ready`.                                                                                                                   |
| `host`      | `string`  | no       | Interface address to bind. Defaults to `"0.0.0.0"` (all IPv4 interfaces). Use `"127.0.0.1"` to stay on loopback.                                                                                                                                                           |
| `cert`      | `string`  | yes      | Filesystem path to a PEM file containing the server certificate chain (leaf first). This is a path, not the PEM text.                                                                                                                                                      |
| `key`       | `string`  | yes      | Filesystem path to a PEM file containing the private key for `cert`. This is a path, not the PEM text.                                                                                                                                                                     |
| `reusePort` | `boolean` | no       | Bind with `SO_REUSEPORT` so multiple processes (for example Node `cluster` workers) can share one listening port and have the kernel load-balance connections across them. Defaults to `false`. Unix-only; ignored on Windows. See [threading.md](./threading.md#cluster). |

Both `cert` and `key` are file paths that the native layer reads at bind time. A missing file, an unreadable key, or a cert and key that do not match are fatal: `ready` (and `closed`) reject with a `WebTransportError`.

## `ready`, `closed`, and `port`

```ts
server.ready; // Promise<void>
server.closed; // Promise<void>
server.port; // number
```

- **`ready: Promise<void>`** resolves once the server is listening and accepting packets. It rejects with a [`WebTransportError`](./errors.md) on a fatal startup error, for example the port is already in use, the bind address is invalid, or the certificate or key cannot be loaded.
- **`closed: Promise<void>`** resolves after the server has fully stopped, whether because you called [`close()`](#closing-the-server) or the process is shutting the server down. It rejects with a `WebTransportError` if the server dies from the same class of fatal error that rejects `ready`.
- **`port: number`** is the UDP port actually bound. It is only meaningful after `ready` has resolved. When you pass `port: 0`, this is where you read back the OS-assigned port.

```ts
const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert, key });

try {
    await server.ready;
    console.log(`listening on 127.0.0.1:${server.port}`);
} catch (err) {
    // bind failure, bad cert/key, etc.
    console.error('server failed to start:', err);
}
```

## Consuming `incomingSessions`

```ts
server.incomingSessions; // ReadableStream<WebTransportServerSession>
```

Established sessions arrive on `incomingSessions`, a standard WHATWG `ReadableStream`. Drain it with a reader loop, and hand each session off to your own handler without blocking the loop, so you keep accepting new clients while existing ones are being served:

```ts
import type { WebTransportServerSession } from 'rwebtransport';

const reader = server.incomingSessions.getReader();
for (;;) {
    const { value: session, done } = await reader.read();
    if (done) break; // the server has stopped
    if (!session) continue;
    handleSession(session); // fire and forget; do not await it here
}
```

`done` becomes `true` when the server stops (after `close()` or a fatal error), which lets the loop exit cleanly. Because each `handleSession` typically spawns its own long-running async work, keep it non-blocking: start the per-session tasks and return, rather than awaiting them inside the accept loop.

## `WebTransportServerSession`

`WebTransportServerSession extends WebTransportSession`, so it has the same members as a client `WebTransport` session, plus the request metadata. It is already established when you receive it: `session.ready` is already resolved, so you do not need to await it (awaiting is a harmless no-op).

Shared surface (identical to the client, documented in [client.md](./client.md), [streams.md](./streams.md), and [datagrams.md](./datagrams.md)):

| Member                          | Type                                              | Notes                                                                                                          |
| ------------------------------- | ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| `ready`                         | `Promise<void>`                                   | Already resolved for a server session.                                                                         |
| `closed`                        | `Promise<WebTransportCloseInfo>`                  | Resolves `{ closeCode, reason }` on a clean close, rejects with a `WebTransportError` on abnormal termination. |
| `datagrams`                     | `WebTransportDatagramDuplexStream`                | Unreliable, unordered datagrams.                                                                               |
| `incomingBidirectionalStreams`  | `ReadableStream<WebTransportBidirectionalStream>` | Bidi streams the client opens.                                                                                 |
| `incomingUnidirectionalStreams` | `ReadableStream<WebTransportReceiveStream>`       | Uni streams the client opens.                                                                                  |
| `createBidirectionalStream()`   | `Promise<WebTransportBidirectionalStream>`        | Open a bidi stream from the server.                                                                            |
| `createUnidirectionalStream()`  | `Promise<WebTransportSendStream>`                 | Open a send-only stream from the server.                                                                       |
| `close(closeInfo?)`             | `void`                                            | Close this session. `closeInfo` is `{ closeCode?: number; reason?: string }`.                                  |

Request metadata (only on `WebTransportServerSession`):

| Member      | Type                     | Meaning                                                                                                      |
| ----------- | ------------------------ | ------------------------------------------------------------------------------------------------------------ |
| `authority` | `string`                 | The `:authority` of the client's CONNECT (host and port the client asked for).                               |
| `path`      | `string`                 | The `:path` of the client's CONNECT, taken from the URL the client connected to (for example `/chat/room1`). |
| `origin`    | `string \| null`         | The `origin` request header, or `null` if the client sent none.                                              |
| `headers`   | `Record<string, string>` | Any additional (non-pseudo) request headers the client sent.                                                 |

Use `path`, `origin`, and `headers` to route and authorize each session, as shown next.

## Accepting and rejecting sessions

There is no explicit accept call. The server has already answered the CONNECT with `200` by the time a `WebTransportServerSession` reaches `incomingSessions`, so simply keeping the session (and starting to read its streams and datagrams) is how you accept it.

To reject a client, close the session as soon as you see it. You can decide based on `path`, `origin`, `headers`, or anything else. Closing a freshly surfaced session tears it down right away; the client observes its own `session.closed` settling. Passing a `closeCode` and `reason` lets you communicate why:

```ts
function handleSession(session: WebTransportServerSession): void {
    // Reject anything that is not a known path.
    if (session.path !== '/echo' && !session.path.startsWith('/chat/')) {
        session.close({ closeCode: 404, reason: 'no such path' });
        return;
    }

    // Reject an unauthorized origin on a sensitive path.
    if (session.path.startsWith('/chat/') && session.origin !== 'https://app.example') {
        session.close({ closeCode: 403, reason: 'forbidden origin' });
        return;
    }

    // Otherwise: accept by serving the session.
    serve(session);
}
```

`close()` is idempotent: calling it more than once is safe and does nothing after the first call. It takes an optional `{ closeCode?: number; reason?: string }`; both default to `0` and `""`.

It is good practice to watch each accepted session's `closed` so a client that disappears does not leave an unhandled rejection. `closed` rejects with a [`WebTransportError`](./errors.md) on abnormal termination:

```ts
session.closed.then(
    (info) => console.log(`session ${session.path} closed cleanly:`, info),
    (err) => console.warn(`session ${session.path} died:`, err),
);
```

## Server-initiated streams and datagrams

A server session can open streams and send datagrams to the client, exactly like the client can to the server. The APIs are identical; see [streams.md](./streams.md) and [datagrams.md](./datagrams.md) for the full detail.

```ts
// Bidirectional stream opened by the server:
const bidi = await session.createBidirectionalStream();
const bw = bidi.writable.getWriter();
await bw.write(new TextEncoder().encode('ping'));
await bw.close();
// bidi.readable is a WebTransportReceiveStream you read the reply from.

// Unidirectional (send-only) stream opened by the server:
const uni = await session.createUnidirectionalStream();
const uw = uni.getWriter();
await uw.write(new TextEncoder().encode('welcome'));
await uw.close();

// Datagram (fire and forget, unreliable):
const dw = session.datagrams.writable.getWriter();
await dw.write(new TextEncoder().encode('hello'));
dw.releaseLock();
```

`write()` on a stream resolves once the bytes are accepted into the QUIC send buffer, which gives you natural backpressure. One caveat that is universal to stream APIs: if you write a large bidirectional stream and never read its `readable` half, the peer can flow-control you and the write can wedge. Read (or cancel) the readable half concurrently. See [streams.md](./streams.md) for the backpressure model.

## Closing the server

```ts
server.close(); // void
```

`close()` stops the server and tears down every session it is hosting. It is idempotent. After a `close()`, `incomingSessions` ends (its reader loop sees `done: true`) and `server.closed` resolves.

```ts
process.on('SIGINT', () => {
    server.close();
});

await server.closed;
console.log('server stopped');
```

## Full example: a per-path router

A complete, runnable server that routes clients by `path`: an echo endpoint at `/echo`, a broadcast room at `/chat/<room>`, and a rejection for everything else. Each session runs its own concurrent handlers, guarded so one dead client cannot crash the process.

```ts
import { WebTransportServer, type WebTransportServerSession } from 'rwebtransport';

const enc = new TextEncoder();
const dec = new TextDecoder();

const server = new WebTransportServer({
    port: 4433,
    host: '0.0.0.0',
    cert: '/path/to/cert.pem',
    key: '/path/to/key.pem',
});

await server.ready;
console.log(`listening on udp/${server.port}`);

// One in-memory set of writers per chat room, for broadcast.
const rooms = new Map<string, Set<WritableStreamDefaultWriter<Uint8Array>>>();

const reader = server.incomingSessions.getReader();
for (;;) {
    const { value: session, done } = await reader.read();
    if (done) break;
    if (session) route(session);
}

function route(session: WebTransportServerSession): void {
    if (session.path === '/echo') {
        serveEcho(session);
    } else if (session.path.startsWith('/chat/')) {
        const room = session.path.slice('/chat/'.length);
        if (room.length === 0) {
            session.close({ closeCode: 400, reason: 'missing room' });
            return;
        }
        serveChat(session, room);
    } else {
        // Reject: unknown path. The client sees its session close.
        session.close({ closeCode: 404, reason: 'no such path' });
    }
}

// /echo: pipe each incoming bidi stream straight back, and echo datagrams.
function serveEcho(session: WebTransportServerSession): void {
    void (async () => {
        const streams = session.incomingBidirectionalStreams.getReader();
        for (;;) {
            const { value: stream, done } = await streams.read();
            if (done) break;
            if (stream) void stream.readable.pipeTo(stream.writable).catch(() => undefined);
        }
    })().catch(() => undefined);

    void (async () => {
        const r = session.datagrams.readable.getReader();
        const w = session.datagrams.writable.getWriter();
        for (;;) {
            const { value, done } = await r.read();
            if (done) break;
            if (value) await w.write(value);
        }
    })().catch(() => undefined);
}

// /chat/<room>: each datagram from a client is broadcast to every peer in the room.
function serveChat(session: WebTransportServerSession, room: string): void {
    const writer = session.datagrams.writable.getWriter();
    let members = rooms.get(room);
    if (!members) {
        members = new Set();
        rooms.set(room, members);
    }
    members.add(writer);

    // Greet the newcomer on a server-opened unidirectional stream.
    void (async () => {
        const uni = await session.createUnidirectionalStream();
        const w = uni.getWriter();
        await w.write(enc.encode(`welcome to ${room}`));
        await w.close();
    })().catch(() => undefined);

    // Fan every inbound datagram out to the rest of the room.
    void (async () => {
        const r = session.datagrams.readable.getReader();
        for (;;) {
            const { value, done } = await r.read();
            if (done) break;
            if (!value) continue;
            const line = enc.encode(`${session.authority}: ${dec.decode(value)}`);
            for (const peer of members) {
                if (peer !== writer) void peer.write(line).catch(() => undefined);
            }
        }
    })().catch(() => undefined);

    // On close, drop this member from the room.
    session.closed.finally(() => {
        members.delete(writer);
        if (members.size === 0) rooms.delete(room);
    });
}

// Graceful shutdown.
process.on('SIGINT', () => server.close());
await server.closed;
console.log('server stopped');
```

Notes on the pattern above:

- The accept loop never awaits per-session work; `route` returns immediately after spawning handlers, so new clients keep being accepted.
- Every long-running handler is wrapped so a stream error or a vanished client rejects only that task, not the whole server. Stream and session failures surface as [`WebTransportError`](./errors.md).
- Rejection is just an early `session.close({ closeCode, reason })`. There is no separate accept step to skip.

## See also

- [getting-started.md](./getting-started.md) A minimal client and server end to end.
- [client.md](./client.md) The `WebTransport` client, which shares the session surface with `WebTransportServerSession`.
- [streams.md](./streams.md) Reading and writing streams, backpressure, and cancel/abort.
- [datagrams.md](./datagrams.md) The datagram duplex, `maxDatagramSize`, high-water marks, and max age.
- [certificates.md](./certificates.md) Certificate and key formats, trust models, and generating a dev cert.
- [errors.md](./errors.md) `WebTransportError`, its `source` and `streamErrorCode`, and how failures surface.
- [threading.md](./threading.md) The per-session thread model, `worker_threads` support, and `cluster` port sharing via `reusePort: true` (`SO_REUSEPORT`, Unix-only).
- [`../examples`](../examples) Runnable client and server programs and a cert generator.
