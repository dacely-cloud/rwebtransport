# Threading, worker_threads, and cluster

This guide covers how `rwebtransport` uses threads internally, why it cannot
deadlock on its own, the single application-level way to wedge a stream, and how
the library behaves inside `worker_threads` and `cluster`.

## The driver-thread architecture

Every WebTransport session (client `WebTransport` or `WebTransportServerSession`)
runs its QUIC and HTTP/3 work on its own dedicated background thread, the
_driver thread_. Your JavaScript never touches quiche or BoringSSL directly. It
talks to the driver thread across a boundary that is non-blocking in both
directions:

- **JS to driver: an unbounded mpsc command channel.** When you call
  `createBidirectionalStream()`, `write()`, `close()`, and so on, the request is
  pushed onto an unbounded multi-producer, single-consumer queue and returns
  immediately. The JS event loop never blocks waiting for the driver to pick the
  command up.
- **Driver to JS: a non-blocking neon `Channel`.** Incoming streams, readable
  chunks, datagrams, and lifecycle events (`ready`, `closed`) are delivered to
  the event loop through neon's `Channel`, which schedules callbacks without
  blocking the driver thread.
- **Shared state: atomics.** Flow-control counters and status flags that both
  sides read are plain atomics, so neither side takes a lock the other could be
  holding.

Because no hand-off in either direction ever blocks on the other side making
progress, there is no lock-ordering cycle and **the library cannot deadlock
internally**. When a peer stops reading, or a send buffer fills, the effect is
_backpressure_, not a hang: `write()` simply resolves later, once the bytes are
accepted into the QUIC send buffer. Read backpressure is symmetric, if you stop
reading a stream, the peer is flow-controlled until you resume.

A panic inside the driver thread is contained and surfaced as a
`WebTransportError` on the affected session's `closed` promise (and on pending
stream operations) rather than crashing the process.

## The one application-level wedge: write without read

There is exactly one way to stall, and it is universal to all bidirectional
stream APIs, not specific to this library: **writing a large amount to a
bidirectional stream while never reading its readable side.**

If the peer is echoing or otherwise sending data back on the same stream, that
inbound data accumulates in the flow-control window. Once the window is full the
peer stops sending, and if your own writes depend (directly or indirectly) on
the peer draining what it received, both directions can come to rest. The fix is
always the same: **read while you write.** Drive the readable and the writable
concurrently instead of finishing one before starting the other.

```ts
import { WebTransport } from 'rwebtransport';

async function drain(readable: ReadableStream<Uint8Array>): Promise<void> {
    const reader = readable.getReader();
    for (;;) {
        const { done } = await reader.read();
        if (done) return;
    }
}

async function sendAll(writable: WritableStream<Uint8Array>, payload: Uint8Array): Promise<void> {
    const writer = writable.getWriter();
    // write() resolves once the bytes are accepted into the QUIC send buffer,
    // applying backpressure rather than blocking.
    await writer.write(payload);
    await writer.close();
}

const session = new WebTransport('https://localhost:4433/echo', {
    insecure: true, // dev only; see the certificates guide for real setups
});
await session.ready;

const stream = await session.createBidirectionalStream();
const bigPayload = new Uint8Array(64 * 1024 * 1024);

// Correct: neither side can stall the other because both run concurrently.
await Promise.all([sendAll(stream.writable, bigPayload), drain(stream.readable)]);

session.close();
```

Sending on a _unidirectional_ stream (`createUnidirectionalStream()`) has no
readable side to drain, so this wedge does not apply there.

## worker_threads

`worker_threads` is **fully supported**. The native addon is context-aware, so:

- A **client or a server** can be created inside any Worker.
- Each Worker gets its **own instance**, bound to that Worker's own event loop.
  Instances are not shared across threads, do not construct a `WebTransport` or
  `WebTransportServer` in one thread and pass it to another. Create it where you
  use it.
- **Multiple Workers run concurrently.** A common pattern is one server on the
  main thread and a pool of Workers each driving its own client sessions, or a
  pool of Workers each accepting sessions from separate servers on separate
  ports.

Sessions, streams, and datagrams are not transferable objects. Move raw bytes
(`Uint8Array`, `ArrayBuffer`) or plain results across the `postMessage` boundary,
and keep the WebTransport objects inside the thread that created them.

### Example: a client inside a Worker

This single file runs as both the main thread and the Worker. The main thread
spawns a pool of Workers, each of which opens its own client session, exchanges a
message on a bidirectional stream, and posts the reply back.

```js
// worker-client.mjs  (run with: node worker-client.mjs)
import { Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';
import { WebTransport } from 'rwebtransport';

async function runClient(url, certHash) {
    // Each Worker builds its own instance on its own event loop.
    const wt = new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash }],
    });
    await wt.ready;

    const stream = await wt.createBidirectionalStream();

    const writer = stream.writable.getWriter();
    await writer.write(new TextEncoder().encode('ping'));
    await writer.close();

    // Read while the peer replies; drain to end-of-stream.
    const reader = stream.readable.getReader();
    const chunks = [];
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        chunks.push(value);
    }

    wt.close();
    return Buffer.concat(chunks).toString();
}

if (isMainThread) {
    // The server's SHA-256 DER fingerprint (32 bytes). See the certificates
    // guide for how to obtain this; here it is passed in as a Uint8Array.
    const certHash = Uint8Array.from(Buffer.from(process.env.CERT_SHA256_HEX, 'hex'));
    const url = 'https://localhost:4433/echo';

    const replies = await Promise.all(
        Array.from(
            { length: 4 },
            () =>
                new Promise((resolve, reject) => {
                    const worker = new Worker(new URL(import.meta.url), {
                        workerData: { url, certHash },
                    });
                    worker.once('message', resolve);
                    worker.once('error', reject);
                }),
        ),
    );

    console.log('replies from 4 concurrent worker clients:', replies);
} else {
    const { url, certHash } = workerData;
    runClient(url, certHash).then(
        (reply) => parentPort.postMessage(reply),
        (err) => {
            throw err;
        },
    );
}
```

The `certHash` (a `Uint8Array`) is structured-cloned across the `postMessage`
boundary via `workerData`, which is fine. Only the raw bytes cross the boundary,
the `WebTransport` object itself is built and used entirely inside each Worker.

## cluster

The story splits by role.

- **Clients: fine as-is.** `cluster` forks independent processes, and a
  WebTransport client is fully self-contained, so each worker process can create
  and use its own clients with no coordination.
- **Server: share one port with `reusePort`.** Pass `reusePort: true` and every
  `cluster` worker can bind the **same** UDP port. The library sets
  `SO_REUSEPORT` on the socket and the kernel load-balances inbound QUIC
  connections across the workers. Because every packet of a given QUIC connection
  carries the same 4-tuple, the kernel routes a connection and all its follow-up
  packets to the same worker, so a session always stays on the worker that
  accepted it.

```js
// cluster-server.mjs  (run with: node cluster-server.mjs)
import cluster from 'node:cluster';
import { availableParallelism } from 'node:os';
import { WebTransportServer } from 'rwebtransport';

const PORT = 4433;

if (cluster.isPrimary) {
    const workers = availableParallelism();
    for (let i = 0; i < workers; i++) cluster.fork();
} else {
    const server = new WebTransportServer({
        port: PORT,
        cert: './cert.pem',
        key: './key.pem',
        reusePort: true, // every worker binds the same port
    });
    await server.ready;
    console.log(`worker ${process.pid} listening on udp/${server.port}`);

    const reader = server.incomingSessions.getReader();
    for (;;) {
        const { value: session, done } = await reader.read();
        if (done) break;
        // The server auto-accepts and this session is already established.
        // Handle it here (read incomingBidirectionalStreams, datagrams, etc.),
        // or call session.close() to reject it.
        session.closed.catch(() => {});
    }
}
```

Notes and alternatives:

- **`reusePort` is Unix-only.** `SO_REUSEPORT` exists on Linux and the BSDs
  (including macOS). On Windows the flag is ignored, so fall back to one distinct
  port per worker (for example `4433`, `4434`, ...), each with its own
  `WebTransportServer`, and distribute across them at a higher layer, or put a
  UDP-aware load balancer in front of a fleet of single-port processes.
- **Load balancing is per-connection, not per-packet.** The kernel hashes the
  4-tuple, so an even spread depends on having many client addresses. A handful
  of clients may land on the same worker.

If you only need in-process concurrency for a server, prefer `worker_threads`
(each Worker binds its own port, or share one with `reusePort`) or simply run
several `WebTransportServer` instances on different ports in one process, since
each session already has its own driver thread.

## See also

- [`./streams.md`](./streams.md) for stream backpressure and the read/write
  concurrency rules referenced above.
- [`./errors.md`](./errors.md) for how session termination and driver-thread
  failures surface as `WebTransportError`.
- [`../README.md`](../README.md) for the top-level overview.
- [`../examples/`](../examples) for runnable client and server programs.
