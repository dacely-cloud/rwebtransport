// SPDX-License-Identifier: Apache-2.0
//! rwebtransport: a fully-compatible WebTransport client and server for Node.js.
//!
//! ```ts
//! import { WebTransport } from 'rwebtransport';
//! const wt = new WebTransport('https://example.com:4433/echo');
//! await wt.ready;
//! ```

/**
 * Client-side session classes forwarded from `./webtransport.js`.
 *
 * @remarks
 * {@link WebTransport} is the W3C client entry point. Its constructor takes a
 * URL plus optional {@link WebTransportOptions}, validates the URL (it must be
 * a string beginning with `https://`) and any pinned certificate hashes (only
 * `sha-256`, 32 bytes), then opens a native quiche/BoringSSL QUIC + HTTP/3
 * session. {@link WebTransportSession} is its base class and holds the surface
 * shared with the server side: the `datagrams` duplex, the
 * `incomingBidirectionalStreams` and `incomingUnidirectionalStreams`
 * ReadableStreams, the `ready` and `closed` promises, the
 * `createBidirectionalStream` and `createUnidirectionalStream` factories, and
 * `close(closeInfo)`. Both are part of the public package API.
 *
 * @see {@link WebTransport}
 * @see {@link WebTransportSession}
 */
export { WebTransport, WebTransportSession } from './webtransport.js';
/**
 * Server-side classes and their option/request types forwarded from
 * `./server.js`.
 *
 * @remarks
 * {@link WebTransportServer} binds a UDP port with a PEM certificate and key,
 * exposes the established peer sessions as the `incomingSessions`
 * ReadableStream, and provides `ready`, `closed`, `port`, and `close()`.
 * {@link WebTransportServerSession} extends {@link WebTransportSession} with the
 * request metadata carried by the client's Extended CONNECT (`authority`,
 * `path`, `origin`, `headers`). {@link WebTransportServerOptions} and
 * {@link WebTransportSessionRequest} are re-exported as type-only symbols:
 * the former is the shape accepted by the `WebTransportServer` constructor
 * (`port`, optional `host`, `cert`, `key`, optional `reusePort`), and the latter
 * is the parsed CONNECT metadata used to build a server session. All four are
 * part of the public package API.
 *
 * @see {@link WebTransportServer}
 * @see {@link WebTransportServerSession}
 * @see {@link WebTransportServerOptions}
 * @see {@link WebTransportSessionRequest}
 */
export {
    WebTransportServer,
    WebTransportServerSession,
    type WebTransportServerOptions,
    type WebTransportSessionRequest,
} from './server.js';
/**
 * The {@link WebTransportError} class forwarded from `./errors.js`.
 *
 * @remarks
 * A subclass of {@link Error} that mirrors the browser `WebTransportError`. It
 * carries a `source` (`'stream'` or `'session'`) and a nullable
 * `streamErrorCode`, and is the value used to reject session and stream
 * promises and to abort the WHATWG streams on failure. Part of the public
 * package API.
 *
 * @see {@link WebTransportError}
 */
export { WebTransportError } from './errors.js';
/**
 * WebTransport stream classes forwarded from `./streams.js`.
 *
 * @remarks
 * {@link WebTransportBidirectionalStream} pairs a `readable`
 * {@link WebTransportReceiveStream} with a `writable`
 * {@link WebTransportSendStream} over one QUIC stream id.
 * {@link WebTransportReceiveStream} is a `ReadableStream<Uint8Array>` whose
 * queue high-water mark drives native read backpressure onto quiche's
 * flow-control window. {@link WebTransportSendStream} is a
 * `WritableStream<Uint8Array>` whose `write()` resolves only once quiche has
 * accepted the bytes, giving flow-control-gated backpressure. All three are
 * part of the public package API.
 *
 * @see {@link WebTransportBidirectionalStream}
 * @see {@link WebTransportReceiveStream}
 * @see {@link WebTransportSendStream}
 */
export {
    WebTransportBidirectionalStream,
    WebTransportReceiveStream,
    WebTransportSendStream,
} from './streams.js';
/**
 * The {@link WebTransportDatagramDuplexStream} class forwarded from
 * `./datagrams.js`.
 *
 * @remarks
 * The per-session datagram transport: a `readable` of inbound datagrams and a
 * `writable` for outbound ones, both intentionally lossy (datagrams that do not
 * fit the bounded queue are dropped rather than buffered). It also exposes
 * `maxDatagramSize`, the `incomingHighWaterMark` / `outgoingHighWaterMark`
 * accessors, and the `incomingMaxAge` / `outgoingMaxAge` fields. Part of the
 * public package API.
 *
 * @see {@link WebTransportDatagramDuplexStream}
 */
export { WebTransportDatagramDuplexStream } from './datagrams.js';

/**
 * Public option and info types forwarded from `./types.js` (type-only exports).
 *
 * @remarks
 * These mirror the W3C WebTransport dictionaries plus a few Node-specific
 * extensions:
 * {@link WebTransportOptions} is the second argument to the {@link WebTransport}
 * constructor (`serverCertificateHashes`, `allowPooling`, `requireUnreliable`,
 * `congestionControl`, and the Node extensions `insecure`, `headers`,
 * `origin`);
 * {@link WebTransportHash} is a pinned server-certificate hash (`algorithm` plus
 * `value`, currently only `sha-256`);
 * {@link WebTransportCongestionControl} is the congestion-control hint union
 * (`'default' | 'throughput' | 'low-latency'`);
 * {@link WebTransportCloseInfo} describes how a session closed (`closeCode`,
 * `reason`) and is the resolved value of `session.closed`;
 * {@link WebTransportCloseOptions} is the optional argument to
 * `session.close()` (`closeCode`, `reason`);
 * and {@link BinarySource} is the accepted byte-source union
 * (`ArrayBufferView | ArrayBuffer`). All are part of the public package API.
 *
 * @see {@link WebTransportOptions}
 * @see {@link WebTransportHash}
 * @see {@link WebTransportCongestionControl}
 * @see {@link WebTransportCloseInfo}
 * @see {@link WebTransportCloseOptions}
 * @see {@link BinarySource}
 */
export type {
    WebTransportOptions,
    WebTransportHash,
    WebTransportCongestionControl,
    WebTransportCloseInfo,
    WebTransportCloseOptions,
    BinarySource,
} from './types.js';
/**
 * Error-related types forwarded from `./errors.js` (type-only exports).
 *
 * @remarks
 * {@link WebTransportErrorSource} is the `'stream' | 'session'` union naming
 * which part of the transport a {@link WebTransportError} came from, and
 * {@link WebTransportErrorOptions} is the optional dictionary accepted by the
 * `WebTransportError` constructor (`source`, defaulting to `'session'`, and
 * `streamErrorCode`, defaulting to `null`). Both are part of the public package
 * API.
 *
 * @see {@link WebTransportErrorSource}
 * @see {@link WebTransportErrorOptions}
 */
export type { WebTransportErrorSource, WebTransportErrorOptions } from './errors.js';
