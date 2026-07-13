# Datagrams

Datagrams are the low-latency, best-effort message channel of a WebTransport
session. Every session (client `WebTransport` and `WebTransportServerSession`
alike) exposes exactly one datagram transport at `session.datagrams`, a
[`WebTransportDatagramDuplexStream`](../src/datagrams.ts).

For the reliable, ordered alternative see [streams](./streams.md). This guide
covers what datagrams are, the shape of the duplex stream, how and when data is
dropped, sending and receiving patterns, and how to choose between datagrams and
streams.

## Semantics: unreliable and unordered

A datagram is a single, self-contained payload that maps to (at most) one QUIC
packet. Unlike a stream, a datagram has no delivery guarantee and no sequencing:

- **Unreliable.** A datagram may be lost in the network and is never
  retransmitted. There is no acknowledgement surfaced to your code and no
  automatic recovery.
- **Unordered.** Datagrams can arrive in a different order than they were sent.
  Two datagrams sent back to back may be delivered reversed, or one may not
  arrive at all.
- **Bounded size.** A datagram must fit in one packet. Anything larger than the
  current path limit (`maxDatagramSize`) is dropped rather than fragmented.
- **Lossy queues.** Both the inbound and the outbound queue are bounded. When a
  queue is full, the _datagram_ is dropped, never buffered indefinitely. This is
  by design: datagrams trade reliability for freshness and latency.

If you need every byte to arrive, in order, use a stream instead.

## The duplex stream

`session.datagrams` is a `WebTransportDatagramDuplexStream` with the following
surface.

| Member                  | Type                         | Description                                                                                 |
| ----------------------- | ---------------------------- | ------------------------------------------------------------------------------------------- |
| `readable`              | `ReadableStream<Uint8Array>` | Inbound datagrams, each chunk is one datagram payload.                                      |
| `writable`              | `WritableStream<Uint8Array>` | Outbound datagrams, each written chunk is sent as one datagram.                             |
| `maxDatagramSize`       | `number` (get)               | Largest payload, in bytes, that currently fits one packet.                                  |
| `incomingHighWaterMark` | `number` (get/set)           | Max number of inbound datagrams queued before overflow drops begin.                         |
| `outgoingHighWaterMark` | `number` (get/set)           | Max number of outbound datagrams buffered before `write()` applies backpressure.            |
| `incomingMaxAge`        | `number \| null` (get/set)   | Max age in ms an inbound datagram is retained before being dropped, or `null` for no limit. |
| `outgoingMaxAge`        | `number \| null` (get/set)   | Max age in ms an outbound datagram waits before being dropped, or `null` for no limit.      |

Notes:

- `readable` and `writable` are standard WHATWG streams, so you use
  `getReader()` / `getWriter()` / `pipeTo()` exactly as you would elsewhere.
- Each read yields a `Uint8Array` that is exactly one datagram. Datagrams are
  message-framed: you never have to reassemble or split them the way you do with
  a byte stream.
- `incomingHighWaterMark` and `outgoingHighWaterMark` are counts of datagrams,
  not bytes. Both default to `64`. Setting a value below `1` is clamped to `1`.

## Drop behavior

Datagrams are dropped in three situations. None of them throw: a dropped
datagram is silently discarded, consistent with the unreliable contract.

### 1. Oversize outbound payloads

A payload larger than `maxDatagramSize` cannot fit in a single packet and is
dropped on send. `write()` still resolves normally, so check the size yourself
before writing if a silent drop would be a problem:

```ts
const writer = session.datagrams.writable.getWriter();

function trySend(payload: Uint8Array): boolean {
    if (payload.byteLength > session.datagrams.maxDatagramSize) {
        return false; // too big for one packet, would be dropped
    }
    void writer.write(payload); // fire and forget
    return true;
}
```

`maxDatagramSize` reflects the current path MTU and can change over the life of
the session, so read it fresh each time rather than caching it. Before the
session is established it reads as `0`, so query it after `await session.ready`.

### 2. Outbound queue overflow

If you write faster than the connection can flush, the outbound queue fills to
`outgoingHighWaterMark`. Past that point `writer.write()` returns a promise that
stays pending (backpressure). Awaiting it lets you pace sends to the link.
Because datagrams are meant to be current, prefer sending the freshest value
rather than buffering a long backlog.

### 3. Inbound queue overflow (and staleness)

Inbound datagrams are enqueued into `readable` only while there is room, up to
`incomingHighWaterMark`. If your consumer reads too slowly and the queue is
full, newly arrived datagrams are dropped. Read promptly to minimize loss. When
set, `incomingMaxAge` also causes datagrams that have waited too long to be
discarded instead of delivered stale.

## Sending

Get a writer once and reuse it. `write()` resolves quickly (fire and forget):
it does not wait for the peer to receive anything, only for the datagram to be
accepted for sending.

```ts
import { WebTransport } from 'rwebtransport';

const session = new WebTransport('https://example.com:4433/telemetry', {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash }],
});
await session.ready;

const writer = session.datagrams.writable.getWriter();
const encoder = new TextEncoder();

setInterval(() => {
    const payload = encoder.encode(JSON.stringify({ t: Date.now(), fps: 60 }));
    if (payload.byteLength <= session.datagrams.maxDatagramSize) {
        void writer.write(payload);
    }
}, 16);
```

To pace a high-rate producer against a slow link, await the write so
backpressure can slow you down:

```ts
async function sendPaced(writer: WritableStreamDefaultWriter<Uint8Array>, payload: Uint8Array) {
    await writer.ready; // resolves when the outbound queue has room
    await writer.write(payload);
}
```

## Receiving

Read `readable` in a loop. Each chunk is one complete datagram.

```ts
const reader = session.datagrams.readable.getReader();
const decoder = new TextDecoder();

try {
    while (true) {
        const { value, done } = await reader.read();
        if (done) break; // session closed
        const message = decoder.decode(value);
        handle(message);
    }
} finally {
    reader.releaseLock();
}
```

If you only care about the most recent value (for example the latest cursor
position or sensor reading), keep the read loop tight and just overwrite your
last-known state on each iteration. Do not try to buffer datagrams for later,
older ones are exactly what the transport is designed to discard.

## Tuning the queues

The high-water marks and max-age properties let you size the queues for your
workload. Set them right after `await session.ready`, before you start reading
or writing heavily.

```ts
const dg = session.datagrams;

// Buffer more inbound datagrams before dropping (bursty producer, spiky reader).
dg.incomingHighWaterMark = 256;

// Allow a deeper outbound queue before write() applies backpressure.
dg.outgoingHighWaterMark = 128;

// Drop anything that has been waiting longer than 100 ms as stale.
dg.incomingMaxAge = 100;
dg.outgoingMaxAge = 100;
```

Guidance:

- Larger high-water marks tolerate bursts but increase latency and memory under
  load. For latency-critical data, keep them small and read/flush aggressively.
- Set `incomingMaxAge` / `outgoingMaxAge` when a late datagram is worse than no
  datagram (live positions, real-time audio/video control). Leave them `null`
  (the default) when age does not matter.

## Datagrams vs streams

| Use datagrams when                                                            | Use [streams](./streams.md) when                      |
| ----------------------------------------------------------------------------- | ----------------------------------------------------- |
| Data is time-sensitive and a late copy is useless                             | Every byte must arrive                                |
| You send frequent small updates where the newest supersedes the old           | You send ordered, arbitrarily large payloads          |
| Occasional loss is acceptable (positions, telemetry, real-time media control) | Loss is not acceptable (files, RPC, control messages) |
| You want the lowest possible latency and no head-of-line blocking             | You want ordering and reliable delivery               |
| Each message fits in one packet (`<= maxDatagramSize`)                        | Messages exceed one packet or are of unbounded length |

Mixing both is normal: send bulk or must-arrive data over a stream, and use
datagrams for the fast-moving updates alongside it. A single session carries
both at once.

## End-to-end example: server echoing datagrams

A server that echoes each datagram back to the sender, and a client that sends a
few and prints the replies. Both sides use the same `session.datagrams` surface.

Server (`server.mjs`):

```js
import { WebTransportServer } from 'rwebtransport';

const server = new WebTransportServer({
    port: 4433,
    cert: './cert.pem',
    key: './key.pem',
});
await server.ready;
console.log(`listening on ${server.port}`);

const sessions = server.incomingSessions.getReader();
while (true) {
    const { value: session, done } = await sessions.read();
    if (done) break;

    // A surfaced server session is already established.
    const reader = session.datagrams.readable.getReader();
    const writer = session.datagrams.writable.getWriter();

    (async () => {
        try {
            while (true) {
                const { value, done } = await reader.read();
                if (done) break;
                if (value.byteLength <= session.datagrams.maxDatagramSize) {
                    await writer.write(value); // echo it straight back
                }
            }
        } catch {
            // session ended
        }
    })();
}
```

Client (`client.mjs`):

```js
import { WebTransport } from 'rwebtransport';

const session = new WebTransport('https://localhost:4433/echo', {
    // Dev only: skip cert validation on a self-signed loopback server.
    insecure: true,
});
await session.ready;

const writer = session.datagrams.writable.getWriter();
const reader = session.datagrams.readable.getReader();
const encoder = new TextEncoder();
const decoder = new TextDecoder();

for (let i = 0; i < 5; i++) {
    await writer.write(encoder.encode(`ping ${i}`));
}

// Datagrams are unordered and lossy: replies may arrive in any order, and some
// may never arrive, so read for a short window rather than expecting all five.
const deadline = Date.now() + 500;
while (Date.now() < deadline) {
    const { value, done } = await reader.read();
    if (done) break;
    console.log('got', decoder.decode(value));
}

session.close();
```

The client loop deliberately reads on a timer instead of counting exactly five
replies: with an unreliable transport you must never assume a specific datagram
arrived. If you need that guarantee, put the exchange on a stream.

## Errors and shutdown

Datagram operations do not raise per-datagram errors, drops are silent. The
streams do end, however, when the session goes away:

- `readable` closes (`done: true`) on a clean session close and errors with a
  [`WebTransportError`](./errors.md) on abnormal termination.
- `writer.write()` / `writer.ready` reject once the session is closed.

Watch `session.closed` (resolves with `{ closeCode, reason }` on a clean close,
rejects with a `WebTransportError` otherwise) to learn why the datagram channel
stopped. See [error handling](./errors.md) for the full model.

## See also

- [Streams and backpressure](./streams.md) for reliable, ordered data.
- [Error handling](./errors.md) for `WebTransportError` and session close.
- The [project README](../README.md) for the full API overview.
