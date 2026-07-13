# Streams and backpressure

A WebTransport session multiplexes many independent streams over a single QUIC
connection. Unlike a WebSocket, a stall on one stream never blocks the others:
there is no head-of-line blocking across streams. This guide covers the two
kinds of stream, the fact that they are ordinary WHATWG streams, the read and
write patterns, how backpressure flows end to end onto QUIC flow control, how
`cancel()` and `abort()` map to `STOP_SENDING` and `RESET_STREAM`, and the one
rule you must follow to avoid wedging a large bidirectional stream.

For the session itself (opening, `ready`, `closed`, closing) see the
[README](../README.md). For unreliable messages see
[datagrams.md](./datagrams.md), and for the error type see
[errors.md](./errors.md).

---

## The two kinds of stream

| | Bidirectional | Unidirectional |
|---|---|---|
| Open it | `session.createBidirectionalStream()` | `session.createUnidirectionalStream()` |
| You get | `WebTransportBidirectionalStream` (read + write) | `WebTransportSendStream` (write only) |
| Peer accepts it via | `incomingBidirectionalStreams` | `incomingUnidirectionalStreams` |
| Peer gets | `WebTransportBidirectionalStream` (read + write) | `WebTransportReceiveStream` (read only) |

Either side (client or server) can open either kind. A
`WebTransportBidirectionalStream` bundles a `.readable`
(`WebTransportReceiveStream`) and a `.writable` (`WebTransportSendStream`); the
two halves carry data in opposite directions but share one underlying QUIC
stream id. A unidirectional stream is one-way: the opener writes, the peer
reads.

```ts
import { WebTransport } from 'rwebtransport';

const wt = new WebTransport('https://localhost:4433/demo', {
  serverCertificateHashes: [{ algorithm: 'sha-256', value: certHashBytes }],
});
await wt.ready;

// Bidirectional: you can both write and read.
const bidi = await wt.createBidirectionalStream();
//   bidi.readable  -> WebTransportReceiveStream (a ReadableStream<Uint8Array>)
//   bidi.writable  -> WebTransportSendStream    (a WritableStream<Uint8Array>)

// Unidirectional: you can only write.
const send = await wt.createUnidirectionalStream(); // WebTransportSendStream
```

---

## They ARE WHATWG streams

There is nothing new to learn about the stream objects themselves. They extend
the standard classes:

- `WebTransportReceiveStream extends ReadableStream<Uint8Array>`
- `WebTransportSendStream extends WritableStream<Uint8Array>`

So every method you already know works exactly as specified: `getReader()`,
`getWriter()`, `pipeTo()`, `pipeThrough()`, `tee()`, `cancel()`, `abort()`,
`close()`, `for await ... of` on a reader, `writer.ready`, `writer.closed`,
`writer.desiredSize`, and so on. Each half also exposes a numeric `.streamId`
(the QUIC stream id), which is the only addition on top of the standard surface.

```ts
bidi.readable.streamId === bidi.writable.streamId; // same QUIC stream
send.streamId; // the id of a unidirectional send stream
```

Because these are real WHATWG streams, you can pipe a receive stream through a
`TransformStream`, `tee()` it to two consumers, or `pipeTo()` a send stream. No
adapters required.

---

## Accepting streams the peer opens

Incoming streams arrive as readable streams of stream objects. Drain them in a
loop with a reader:

```ts
// Bidirectional streams the peer opens.
const bidiReader = wt.incomingBidirectionalStreams.getReader();
for (;;) {
  const { value: stream, done } = await bidiReader.read();
  if (done) break; // session closed
  handleBidi(stream); // stream is a WebTransportBidirectionalStream
}

// Unidirectional streams the peer opens (read-only).
const uniReader = wt.incomingUnidirectionalStreams.getReader();
for (;;) {
  const { value: recv, done } = await uniReader.read();
  if (done) break;
  handleReceive(recv); // recv is a WebTransportReceiveStream
}
```

When the session closes cleanly these readers report `done: true`. If the
session dies abnormally, `read()` rejects with a `WebTransportError`.

---

## Reading

Get a reader and loop until `done`. Each chunk is a `Uint8Array`.

```ts
async function readAll(readable: ReadableStream<Uint8Array>): Promise<Uint8Array> {
  const reader = readable.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break; // the peer sent FIN: stream finished normally
      chunks.push(value);
      total += value.length;
    }
  } finally {
    reader.releaseLock();
  }
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}
```

Chunk boundaries are not message boundaries. QUIC delivers a byte stream, so a
single `write()` on the peer may arrive as several `read()` chunks or be
coalesced with others. If you need framing, add a length prefix or a delimiter
yourself.

---

## Writing

Get a writer, `write()` your chunks, then `close()` to send a clean FIN.

```ts
const writer = send.getWriter();
await writer.write(new TextEncoder().encode('hello'));
await writer.write(new TextEncoder().encode(' over QUIC'));
await writer.close(); // sends FIN; the peer's reader observes done: true
writer.releaseLock();
```

`write()` accepts a `Uint8Array` (or any `ArrayBufferView` / `ArrayBuffer`). It
resolves once the bytes have been accepted into quiche's send buffer, which is
the backpressure signal described below. `close()` flushes and finishes the
stream; after it resolves the peer sees end of stream.

---

## Echo with pipeTo

`pipeTo()` reads a readable and writes a writable concurrently, with
backpressure wired through for you. That makes it the cleanest way to echo a
bidirectional stream (and, importantly, it reads while it writes, so it cannot
deadlock, see the rule below):

```ts
// Server side: echo every incoming bidi stream straight back to the peer.
const streams = session.incomingBidirectionalStreams.getReader();
for (;;) {
  const { value: stream, done } = await streams.read();
  if (done) break;
  // readable -> writable: bytes the peer sends come right back.
  void stream.readable.pipeTo(stream.writable).catch(() => {
    // the stream was reset or the session closed; nothing to clean up.
  });
}
```

`pipeTo()` also propagates completion: when the readable finishes (FIN), it
closes the writable; if the readable errors (a peer reset), it aborts the
writable. Use the `preventClose`, `preventAbort`, and `preventCancel` options if
you want to override that.

---

## Backpressure, end to end onto QUIC flow control

Backpressure is automatic in both directions and bottoms out in QUIC's
flow-control windows (`MAX_STREAM_DATA` per stream and `MAX_DATA` per
connection). You never manage windows by hand; you just read and write, and the
library throttles the wire to match.

### Write backpressure

Each `write()` resolves only when the bytes have been buffered into quiche for
sending. quiche can only accept bytes up to the peer's advertised flow-control
window, and that window only grows as the peer reads. So if the peer stops
reading, the window stops opening, quiche stops accepting, and your `write()`
stops resolving. The producer is paused until the peer catches up.

The send stream has a byte-based high-water mark (1 MiB by default). You can
observe the pressure through the writer without awaiting every write:

```ts
const writer = send.getWriter();
for (const chunk of chunks) {
  // writer.ready pends while more than the high-water mark is buffered,
  // and resolves once the peer has read enough for QUIC to accept more.
  await writer.ready;
  void writer.write(chunk); // do not await: ready already gates the pace
}
await writer.ready;
await writer.close();
```

`await writer.write(chunk)` gives you the same throttling more simply: it will
not resolve while the stream is over its high-water mark and the peer is not
draining. `writer.desiredSize` is the remaining room before the mark (it goes
zero or negative under pressure).

### Read backpressure

Inbound bytes are enqueued into the receive stream's internal queue (a count
high-water mark of 32 chunks by default). While you keep reading, the queue
stays low and the native side keeps draining quiche, which keeps issuing
flow-control credit to the peer. The moment you stop reading, the queue fills,
the native side stops draining quiche, quiche stops extending the peer's window,
and the peer is flow-controlled: it cannot send more until you resume. When you
call `read()` again the queue drains, the window reopens, and data flows. You do
nothing to make this happen; simply not reading is the backpressure.

The practical consequence: to relieve a peer that is sending fast, read faster
(or `pipeTo()` a sink). To slow a peer down deliberately, read slower. And if you
never consume `incomingBidirectionalStreams` / `incomingUnidirectionalStreams`
or never read an accepted stream, the peer is flow-controlled on that data.

---

## Cancelling a read: STOP_SENDING

Cancelling a `WebTransportReceiveStream` tells the peer to stop sending on that
stream. It sends a QUIC `STOP_SENDING` frame.

```ts
const reader = bidi.readable.getReader();
const first = await reader.read();
// I have what I need; ask the peer to stop.
await reader.cancel(); // sends STOP_SENDING with code 0
```

To send a specific application error code, cancel with a `WebTransportError`
whose `streamErrorCode` is set. The library uses that code on the wire; any
other reason maps to code 0.

```ts
import { WebTransportError } from 'rwebtransport';

await bidi.readable.cancel(
  new WebTransportError('not needed', { source: 'stream', streamErrorCode: 7 }),
); // sends STOP_SENDING(7)
```

---

## Aborting a write: RESET_STREAM

Aborting a `WebTransportSendStream` discards any unsent buffered data and resets
the stream. It sends a QUIC `RESET_STREAM` frame.

```ts
import { WebTransportError } from 'rwebtransport';

const writer = bidi.writable.getWriter();
await writer.abort(
  new WebTransportError('give up', { source: 'stream', streamErrorCode: 3 }),
); // sends RESET_STREAM(3)
```

As with `cancel()`, the error code comes from a `WebTransportError.streamErrorCode`
on the abort reason, and defaults to 0 otherwise. `writer.close()` is the clean
alternative: it finishes the stream normally with a FIN instead of a reset.

---

## Observing the peer's cancel and abort

The two frames surface as errors on your end:

- The peer sending `RESET_STREAM` errors your `WebTransportReceiveStream`. The
  pending or next `read()` rejects with a `WebTransportError` whose
  `source` is `"stream"` and whose `streamErrorCode` is the peer's code.
- The peer sending `STOP_SENDING` errors your `WebTransportSendStream`. A pending
  or subsequent `write()` (and `writer.closed`) rejects with a `WebTransportError`
  carrying the code.

```ts
import { WebTransportError } from 'rwebtransport';

// Reading: catch a peer reset.
try {
  const reader = bidi.readable.getReader();
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    handle(value);
  }
} catch (err) {
  if (err instanceof WebTransportError) {
    console.log('peer reset the stream, code', err.streamErrorCode);
  }
}

// Writing: catch a peer STOP_SENDING.
try {
  await writer.write(chunk);
} catch (err) {
  if (err instanceof WebTransportError) {
    console.log('peer stopped receiving, code', err.streamErrorCode);
  }
}
```

See [errors.md](./errors.md) for the full `WebTransportError` shape.

---

## The one deadlock rule: read while you write a large bidi stream

There is exactly one way to wedge a stream, and it is universal to duplex stream
APIs, not specific to this library: **writing a large amount to a bidirectional
stream without concurrently reading its readable half.**

Why it happens: suppose you write a payload larger than the flow-control windows
and the peer echoes (or otherwise replies on the same stream). The peer reads
your bytes and writes the reply back. Those reply bytes land in your receive
queue. If you are not reading, that queue fills and, per the read-backpressure
rule above, you flow-control the peer. Now the peer cannot write its reply, so it
stops reading your data, so it stops extending your send window, so your
`write()` stops resolving. Both sides are blocked waiting for the other. Deadlock.

Do NOT do this:

```ts
// DEADLOCK RISK: write everything, then read.
const stream = await wt.createBidirectionalStream();
const writer = stream.writable.getWriter();
await writer.write(hugePayload); // may never resolve if the peer replies
await writer.close();
const reply = await readAll(stream.readable); // never reached
```

Do this instead: read and write concurrently, so neither side can starve the
other.

```ts
const stream = await wt.createBidirectionalStream();

const writing = (async () => {
  const writer = stream.writable.getWriter();
  await writer.write(hugePayload);
  await writer.close();
})();

const reading = readAll(stream.readable); // drains while writing proceeds

const [, reply] = await Promise.all([writing, reading]);
```

`pipeTo()` follows this rule for free (it reads and writes at once), which is why
the echo example above is safe at any payload size. Unidirectional streams and
datagrams cannot deadlock this way, because there is no read half to starve.

Every hand-off between the JS event loop and the native driver thread is
non-blocking, so the library never deadlocks internally; this application-level
pattern is the only thing that can stall a stream. Read while you write.

---

## A complete request and response

Putting it together: open a bidi stream, send a request, read the full response,
with read and write running concurrently so it is safe for any size.

```ts
import { WebTransport } from 'rwebtransport';

const wt = new WebTransport('https://localhost:4433/rpc', {
  serverCertificateHashes: [{ algorithm: 'sha-256', value: certHashBytes }],
});
await wt.ready;

async function request(payload: Uint8Array): Promise<Uint8Array> {
  const stream = await wt.createBidirectionalStream();

  const writing = (async () => {
    const writer = stream.writable.getWriter();
    await writer.write(payload);
    await writer.close();
  })();

  const reading = readAll(stream.readable);

  const [, response] = await Promise.all([writing, reading]);
  return response;
}

const reply = await request(new TextEncoder().encode('ping'));
console.log(new TextDecoder().decode(reply));

wt.close({ closeCode: 0, reason: 'done' });
```

---

## See also

- [../README.md](../README.md): session lifecycle, connecting, and the server API.
- [datagrams.md](./datagrams.md): unreliable, unordered messages.
- [errors.md](./errors.md): the `WebTransportError` type and error handling.
- Runnable programs live in [../examples/](../examples).
