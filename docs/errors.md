# Error handling

Every failure in `rwebtransport` surfaces as a **`WebTransportError`**, the same
type the browser throws. It reaches you through one of four channels: a
synchronous `throw` from the constructor, a rejected `ready` promise, a rejected
`closed` promise, or a rejected `read()` / `write()` on a stream. This page maps
each failure to the channel it comes out of and gives you copy-paste try/catch
patterns for `ready`, reads, and writes.

## The `WebTransportError` type

`WebTransportError extends Error`. It is a named export, so you can use
`instanceof` to tell it apart from ordinary errors.

```ts
import { WebTransportError } from 'rwebtransport';
```

| Field             | Type                       | Meaning                                                                                   |
| ----------------- | -------------------------- | ----------------------------------------------------------------------------------------- |
| `name`            | `"WebTransportError"`      | Always this string.                                                                        |
| `message`         | `string`                   | Human-readable cause (`"stream reset by peer"`, a TLS error, a connect failure, and so on).|
| `source`          | `"stream" \| "session"`    | Whether the error came from a single stream or from the session as a whole.               |
| `streamErrorCode` | `number \| null`           | The peer's 32-bit application error code when `source` is `"stream"`; otherwise `null`.    |

Read `source` first: it tells you which layer failed. A `"stream"` error means one
stream was reset or stopped and the rest of the session is fine. A `"session"`
error means the whole connection is gone and every stream on it is finished too.
`streamErrorCode` is only meaningful when `source === "stream"`.

## Where each failure surfaces

| Situation                                              | Channel                        | `source`   |
| ------------------------------------------------------ | ------------------------------ | ---------- |
| Malformed URL or bad option shape                      | `throw` from `new WebTransport`| `"session"`|
| Connect fails (bad cert, 4xx CONNECT, host unreachable)| `ready` rejects                | `"session"`|
| Session terminates abnormally after it was established | `closed` rejects               | `"session"`|
| Session closes cleanly                                 | `closed` resolves (no error)   | n/a        |
| Peer sends `RESET_STREAM`                              | the readable's `read()` rejects| `"stream"` |
| Peer sends `STOP_SENDING`                              | the writable's `write()` rejects| `"stream"`|
| `createBidirectionalStream` / `write` after close      | that call's promise rejects    | `"session"`|

Datagrams never surface a `WebTransportError`: writes resolve fire-and-forget
(and silently drop when they cannot be sent), and the datagram readable simply
stops yielding when the session ends. See [datagrams.md](./datagrams.md).

## Constructor throws synchronously

The `WebTransport` constructor validates its arguments before it does any
network work, so a bad URL or a malformed option throws **synchronously**, before
`ready` even exists. Guard the constructor itself if the URL or hashes come from
untrusted input.

```ts
import { WebTransport, WebTransportError } from 'rwebtransport';

try {
    // Must be https://host:port/path. A wrong scheme, a non-32-byte
    // sha-256 hash, or a non-"sha-256" algorithm all throw here.
    const wt = new WebTransport('http://example.com:4433/echo');
} catch (err) {
    if (err instanceof WebTransportError) {
        console.error('bad WebTransport setup:', err.message); // source === "session"
    }
}
```

## Connect failures: `ready` rejects

Once the constructor returns, the handshake runs in the background. If it fails,
`ready` rejects with a `WebTransportError` whose `source` is `"session"`. Typical
causes: a certificate that fails PKI or hostname validation (see
[certificates.md](./certificates.md)), a server that answers the Extended CONNECT
with a 4xx, an unresolvable host, or a refused or timed-out UDP connection.

When a session fails before it is established, **both** `ready` and `closed`
reject with the same error. Await `ready` to find out whether you have a usable
session; there is no need to also await `closed` just to detect a connect
failure.

```ts
const wt = new WebTransport('https://example.com:4433/echo');

try {
    await wt.ready;
} catch (err) {
    if (err instanceof WebTransportError) {
        console.error('connect failed:', err.message); // e.g. TLS or CONNECT error
    }
    return; // no session; nothing else to do
}
// From here on the session is established and usable.
```

## Clean close vs abnormal termination: `closed`

`closed` is a `Promise<WebTransportCloseInfo>` where
`WebTransportCloseInfo = { closeCode: number; reason: string }`.

- **Clean close** (either side calls `close()`, or the peer closes gracefully):
  `closed` **resolves** with the close code and reason.
- **Abnormal termination** after the session was established (the connection is
  lost, idle-times out, or the peer aborts): `closed` **rejects** with a
  `WebTransportError`, `source` `"session"`.
- **Never established** (a connect failure): `closed` rejects, as described
  above.

```ts
import type { WebTransportCloseInfo } from 'rwebtransport';

try {
    const info: WebTransportCloseInfo = await wt.closed;
    console.log(`closed cleanly: code=${info.closeCode} reason=${info.reason}`);
} catch (err) {
    if (err instanceof WebTransportError) {
        console.error('session terminated abnormally:', err.message);
    }
}
```

Calling `close(info?)` yourself is the clean path: `wt.close({ closeCode: 0,
reason: 'done' })` leads to `closed` resolving with those values. `close()` is
idempotent, so calling it more than once is harmless.

## Stream reset and `STOP_SENDING`

Stream-level failures are delivered through the stream you are using, not through
`closed`. The session stays alive; only that one stream ends.

**A peer resets a stream (`RESET_STREAM`).** The stream's `readable` errors. Your
in-flight or next `read()` rejects with a `WebTransportError`, `source`
`"stream"`, and `streamErrorCode` set to the code the peer chose.

```ts
const bidi = await wt.createBidirectionalStream();
const reader = bidi.readable.getReader();

try {
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        handle(value);
    }
} catch (err) {
    if (err instanceof WebTransportError && err.source === 'stream') {
        console.error('peer reset the stream, code =', err.streamErrorCode);
    } else {
        throw err;
    }
} finally {
    reader.releaseLock();
}
```

**A peer stops reading (`STOP_SENDING`).** The stream's `writable` errors. Your
`write()` (or `close()`) on that stream rejects with a `WebTransportError`,
`source` `"stream"`, and `streamErrorCode` set to the peer's code.

```ts
const writer = bidi.writable.getWriter();

try {
    await writer.write(new TextEncoder().encode('ping'));
    await writer.close();
} catch (err) {
    if (err instanceof WebTransportError && err.source === 'stream') {
        console.error('peer sent STOP_SENDING, code =', err.streamErrorCode);
    } else if (err instanceof WebTransportError && err.source === 'session') {
        console.error('session ended while writing');
    } else {
        throw err;
    }
} finally {
    writer.releaseLock();
}
```

When the **session** ends (clean or abnormal), any streams that are still open
are errored too: their pending reads and writes reject, and any pending
`createBidirectionalStream()`, `createUnidirectionalStream()`, or `write()`
promises reject with a `"session"`-source error. So a read or write loop can also
observe a `source === "session"` error, which is why the write example above
handles both cases.

## Sending your own error codes (cancel / abort)

To end a stream from your side with a specific application code, pass a
`WebTransportError` carrying a `streamErrorCode`. Cancelling a readable sends
`STOP_SENDING`; aborting a writable sends `RESET_STREAM`. The library reads
`streamErrorCode` off the reason (any other reason maps to code `0`).

```ts
// Tell the peer to stop sending on this readable, with code 7.
await bidi.readable.cancel(
    new WebTransportError('not needed', { source: 'stream', streamErrorCode: 7 }),
);

// Reset this writable with code 7.
await bidi.writable.abort(
    new WebTransportError('giving up', { source: 'stream', streamErrorCode: 7 }),
);
```

Codes travel on the wire as 32-bit unsigned integers. See
[streams.md](./streams.md) for the full stream lifecycle.

## Operations after the session is closed

Once the session is closed (by either side), stream-opening and stream-writing
operations reject immediately rather than hanging:

```ts
wt.close();

try {
    await wt.createBidirectionalStream();
} catch (err) {
    // WebTransportError: "session is closed", source === "session"
}
```

`createUnidirectionalStream()` and a stream `write()` behave the same way. In
contrast, a datagram `write()` after close resolves without error (the datagram
is simply dropped), and calling `close()` again is a no-op.

## Server-side errors

The server surfaces failures through the same `WebTransportError` type, on the
analogous promises (see [server.md](./server.md)):

- `WebTransportServer.ready` rejects with a `WebTransportError` (`source`
  `"session"`) on a fatal startup error such as a bind failure or an unreadable
  or invalid cert/key.
- `WebTransportServer.closed` is a `Promise<void>`: it resolves when the server
  stops cleanly, and rejects with a `WebTransportError` on a fatal server error.
- `WebTransportServer.incomingSessions` is **closed** (not errored) when the
  server stops or fails, so its reader observes `done: true`. To learn *why* the
  server stopped, await `server.closed`.
- A `WebTransportServerSession` has the same `ready`, `closed`, streams, and
  datagrams as a client session, so every pattern on this page applies to it. A
  server session is already established when you receive it, so its `ready` is
  already resolved; its `closed` resolves or rejects exactly like the client's.

```ts
const server = new WebTransportServer({ port: 4433, cert, key });

try {
    await server.ready;
    console.log('listening on', server.port);
} catch (err) {
    if (err instanceof WebTransportError) {
        console.error('server failed to start:', err.message);
    }
}
```

## Unobserved rejections

`rwebtransport` attaches an internal no-op `catch` to its `ready` and `closed`
promises, so forgetting to handle one will not crash your process with an
`unhandledRejection`. That safety net is not a substitute for handling the error:
if you never await or `.catch()` these promises, you silently miss the failure.
Always attach a handler to `ready` (to know the session is usable) and, where it
matters, to `closed` (to know how and why it ended).

## Recommended shape

Putting it together, a robust client typically does:

```ts
import { WebTransport, WebTransportError } from 'rwebtransport';

async function connect(url: string): Promise<WebTransport | null> {
    let wt: WebTransport;
    try {
        wt = new WebTransport(url); // synchronous validation
    } catch (err) {
        console.error('bad URL/options:', (err as Error).message);
        return null;
    }

    // Observe the session lifetime without blocking the caller.
    wt.closed
        .then((info) => console.log('closed:', info.closeCode, info.reason))
        .catch((err: WebTransportError) => console.error('session died:', err.message));

    try {
        await wt.ready;
    } catch (err) {
        console.error('connect failed:', (err as Error).message);
        return null;
    }
    return wt;
}
```

Then wrap each read loop and each write in their own try/catch, as shown above,
so a single stream reset or `STOP_SENDING` is handled locally without tearing
down the rest of your session.

## See also

- [client.md](./client.md) The `ready` / `closed` promises and `close()`.
- [streams.md](./streams.md) Stream lifecycle, cancel/abort, and backpressure.
- [certificates.md](./certificates.md) TLS trust and the certificate errors that make `ready` reject.
- [server.md](./server.md) Server startup, `incomingSessions`, and per-session errors.
- [troubleshooting.md](./troubleshooting.md) Concrete failures (unsupported Node, cert/hostname, 4xx CONNECT) and fixes.
