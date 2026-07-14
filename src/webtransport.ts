// SPDX-License-Identifier: Apache-2.0
//! The `WebTransport` class: the W3C entry point.

import { SessionCore, createClientSession, type ConnectConfig } from './native.js';
import { WebTransportError } from './errors.js';
import { WebTransportDatagramDuplexStream } from './datagrams.js';
import {
    WebTransportBidirectionalStream,
    WebTransportReceiveStream,
    WebTransportSendStream,
    WebTransportSendGroup,
    type WebTransportSendStreamOptions,
} from './streams.js';
import type {
    BinarySource,
    WebTransportCloseInfo,
    WebTransportCloseOptions,
    WebTransportOptions,
    WebTransportReliabilityMode,
    WebTransportConnectionStats,
} from './types.js';

/**
 * Normalize a {@link BinarySource} into a `Uint8Array` view over the same
 * memory, without copying.
 *
 * @param src - A typed array or `DataView`, or a raw `ArrayBuffer`.
 * @returns A `Uint8Array`: the whole buffer when `src` is an `ArrayBuffer`,
 * otherwise a view restricted to the source view's own `byteOffset` and
 * `byteLength` so a subview does not expose bytes outside its window.
 * @internal
 */
function toBytes(src: BinarySource): Uint8Array {
    if (src instanceof ArrayBuffer) return new Uint8Array(src);
    return new Uint8Array(src.buffer, src.byteOffset, src.byteLength);
}

/**
 * Validate a URL and {@link WebTransportOptions} and flatten them into the
 * {@link ConnectConfig} the native `connect` function expects.
 *
 * @param url - The session URL. Must be a `string` beginning with `https://`.
 * @param options - Caller-supplied session options.
 * @returns The normalized connect configuration (URL, SHA-256 hash pins,
 * insecure flag, origin, and parallel header name/value arrays).
 * @throws WebTransportError with `source: 'session'` if `url` is not an
 * `https://` string, if a certificate hash uses an algorithm other than
 * `sha-256`, or if a `sha-256` hash value is not exactly 32 bytes.
 * @remarks Each entry of `options.serverCertificateHashes` is checked: the
 * algorithm is compared case-insensitively against `sha-256` and the value is
 * converted with {@link toBytes} and required to be 32 bytes long.
 * `options.headers` is split into parallel `headerNames` and `headerValues`
 * arrays. Missing optional fields default to `insecure: false` and
 * `origin: null`.
 * @internal
 */
function buildConfig(url: string, options: WebTransportOptions): ConnectConfig {
    if (typeof url !== 'string' || !url.startsWith('https://')) {
        throw new WebTransportError(`invalid WebTransport URL: ${url}`, { source: 'session' });
    }

    const hashes: Uint8Array[] = [];
    for (const hash of options.serverCertificateHashes ?? []) {
        const algorithm = hash.algorithm.toLowerCase();
        if (algorithm !== 'sha-256') {
            throw new WebTransportError(
                `unsupported certificate hash algorithm: ${hash.algorithm} (only sha-256)`,
                { source: 'session' },
            );
        }
        const bytes = toBytes(hash.value);
        if (bytes.byteLength !== 32) {
            throw new WebTransportError('sha-256 certificate hash must be 32 bytes', {
                source: 'session',
            });
        }
        hashes.push(bytes);
    }

    const headerNames: string[] = [];
    const headerValues: string[] = [];
    for (const [name, value] of Object.entries(options.headers ?? {})) {
        headerNames.push(name);
        headerValues.push(value);
    }

    return {
        url,
        hashes,
        insecure: options.insecure ?? false,
        origin: options.origin ?? null,
        headerNames,
        headerValues,
    };
}

/**
 * The symmetric surface of an established WebTransport session (streams and
 * datagrams), shared by the client {@link WebTransport} and the server-side
 * session (`WebTransportServerSession` in `src/server.ts`).
 *
 * @remarks Wraps a {@link SessionCore}, which turns the callback-based native
 * ABI into promises and per-stream sinks. Peer-initiated streams surface on
 * {@link WebTransportSession.incomingBidirectionalStreams} and
 * {@link WebTransportSession.incomingUnidirectionalStreams}; datagrams are
 * exposed through {@link WebTransportSession.datagrams}.
 * @see {@link WebTransport} for the client entry point.
 */
export class WebTransportSession {
    /**
     * The event dispatcher and command surface backing this session. Shared with
     * the datagram and stream wrappers so they can issue native commands and
     * register their inbound sinks. Protected so the server-side subclass can
     * reach it while it stays hidden from public API consumers.
     * @internal
     */
    protected readonly core: SessionCore;
    /**
     * The session's datagram transport: a {@link WebTransportDatagramDuplexStream}
     * whose `readable` yields inbound datagrams and `writable` sends them.
     * Datagrams are unreliable and unordered; ones that do not fit the queue are
     * dropped rather than buffered.
     */
    public readonly datagrams: WebTransportDatagramDuplexStream;
    /**
     * A `ReadableStream` that yields a {@link WebTransportBidirectionalStream} for
     * every bidirectional stream the peer opens. Fed by the core's incoming
     * handler; it closes when the session closes cleanly and errors if the
     * session terminates abnormally.
     */
    public readonly incomingBidirectionalStreams: ReadableStream<WebTransportBidirectionalStream>;
    /**
     * A `ReadableStream` that yields a receive-only
     * {@link WebTransportReceiveStream} for every unidirectional stream the peer
     * opens. Fed by the core's incoming handler; it closes on clean session close
     * and errors on abnormal termination.
     */
    public readonly incomingUnidirectionalStreams: ReadableStream<WebTransportReceiveStream>;
    /**
     * Guards {@link WebTransportSession.close} so that repeated calls are no-ops
     * and the native close is issued at most once.
     * @internal
     */
    private closeCalled = false;

    /**
     * Wire up the session surface around an existing {@link SessionCore}.
     *
     * @param core - The event dispatcher and command core for this session. For
     * the client this is a freshly connected client session; for the server it is
     * a session-scoped core created by the server driver.
     * @remarks Creates the {@link WebTransportSession.datagrams} duplex, then
     * constructs the two incoming-stream `ReadableStream`s and captures their
     * controllers via the `start` callback. Registers an incoming handler on
     * `core` so each peer-opened bidi/uni stream is wrapped and enqueued;
     * `enqueue` is wrapped in try/catch because it throws once the consumer has
     * cancelled the reader, and that error must not escape native event dispatch.
     * Finally it observes `core.closed`: on clean close both controllers are
     * closed via {@link safeClose}, and on rejection both are errored with the
     * failure reason via {@link safeError}.
     */
    public constructor(core: SessionCore) {
        this.core = core;
        this.datagrams = new WebTransportDatagramDuplexStream(core);

        let bidiController!: ReadableStreamDefaultController<WebTransportBidirectionalStream>;
        let uniController!: ReadableStreamDefaultController<WebTransportReceiveStream>;
        this.incomingBidirectionalStreams = new ReadableStream<WebTransportBidirectionalStream>({
            start: (c) => {
                bidiController = c;
            },
        });
        this.incomingUnidirectionalStreams = new ReadableStream<WebTransportReceiveStream>({
            start: (c) => {
                uniController = c;
            },
        });

        core.setIncomingHandler({
            onBidi: (id) => {
                // Guard: enqueue throws if the consumer already cancelled the
                // reader, and that must not escape the native event dispatch.
                try {
                    bidiController.enqueue(new WebTransportBidirectionalStream(core, id));
                } catch {
                    // consumer no longer reading incoming bidi streams
                }
            },
            onUni: (id) => {
                try {
                    uniController.enqueue(new WebTransportReceiveStream(core, id));
                } catch {
                    // consumer no longer reading incoming uni streams
                }
            },
        });

        core.closed.promise.then(
            () => {
                safeClose(bidiController);
                safeClose(uniController);
            },
            (err: unknown) => {
                safeError(bidiController, err);
                safeError(uniController, err);
            },
        );
    }

    /**
     * A promise that resolves once the session handshake completes and the
     * session is established, and rejects if the session fails or closes before
     * it becomes ready.
     * @returns The core's `ready` promise.
     */
    public get ready(): Promise<void> {
        return this.core.ready.promise;
    }

    /**
     * A promise that resolves with the {@link WebTransportCloseInfo} (close code
     * and reason) when the session ends cleanly, and rejects with a
     * {@link WebTransportError} on abnormal termination or if the session never
     * became ready.
     * @returns The core's `closed` promise.
     */
    public get closed(): Promise<WebTransportCloseInfo> {
        return this.core.closed.promise;
    }

    /**
     * A promise that resolves when the peer signals it is draining the session
     * (a `DRAIN_WEBTRANSPORT_SESSION` capsule): it intends to close soon, so you
     * should stop opening new streams, but the session and its existing streams
     * stay usable until {@link closed}. Never rejects.
     * @returns The core's `draining` promise.
     */
    public get draining(): Promise<void> {
        return this.core.draining.promise;
    }

    /**
     * The reliability modes this session supports. `'pending'` until the session
     * is established, then `'supports-unreliable'` because this transport always
     * offers both reliable streams and unreliable datagrams.
     */
    public get reliability(): WebTransportReliabilityMode {
        return this.core.isReady ? 'supports-unreliable' : 'pending';
    }

    /**
     * Open a new outbound bidirectional stream.
     *
     * @returns A promise resolving to a {@link WebTransportBidirectionalStream}
     * (paired readable and writable halves) once the native side reports the new
     * stream id.
     * @throws WebTransportError (the returned promise rejects) if the session is
     * already closed or was never attached.
     */
    public async createBidirectionalStream(
        options: WebTransportSendStreamOptions = {},
    ): Promise<WebTransportBidirectionalStream> {
        const id = await this.core.openStream(true);
        return new WebTransportBidirectionalStream(this.core, id, options);
    }

    /**
     * Open a new outbound unidirectional (send-only) stream.
     *
     * @returns A promise resolving to a {@link WebTransportSendStream} once the
     * native side reports the new stream id.
     * @throws WebTransportError (the returned promise rejects) if the session is
     * already closed or was never attached.
     */
    public async createUnidirectionalStream(
        options: WebTransportSendStreamOptions = {},
    ): Promise<WebTransportSendStream> {
        const id = await this.core.openStream(false);
        return new WebTransportSendStream(this.core, id, options);
    }

    /**
     * Create a {@link WebTransportSendGroup} for scheduling several streams'
     * sends relative to one another via each stream's `sendOrder`.
     */
    public createSendGroup(): WebTransportSendGroup {
        return new WebTransportSendGroup();
    }

    /**
     * Close the session gracefully, notifying the peer with an application code
     * and reason.
     *
     * @param closeInfo - Close options. `closeInfo.closeCode` defaults to `0` and
     * `closeInfo.reason` defaults to `''`; both are forwarded to the native
     * close.
     * @defaultValue `closeInfo` defaults to `{}` (close code `0`, empty reason).
     * @remarks Idempotent: guarded by {@link WebTransportSession.closeCalled} so
     * only the first call takes effect and later calls are no-ops. Delegates to
     * the core, which issues the native `closeSession` (a graceful
     * CLOSE_WEBTRANSPORT_SESSION capsule plus FIN, then QUIC close) only while the
     * session is still attached and not already closed.
     */
    public close(closeInfo: WebTransportCloseOptions = {}): void {
        if (this.closeCalled) return;
        this.closeCalled = true;
        this.core.close(closeInfo.closeCode ?? 0, closeInfo.reason ?? '');
    }

    /**
     * Tell the peer this session is draining by sending a
     * `DRAIN_WEBTRANSPORT_SESSION` capsule: a graceful signal that you intend to
     * stop using it soon, while the session and its streams stay open until
     * {@link close}. The peer observes this through its {@link draining} promise.
     * A Node extension beyond the W3C API, useful for a server shedding load.
     */
    public drain(): void {
        this.core.drain();
    }

    /**
     * Snapshot connection statistics (bytes and packets transferred, RTT, and
     * datagram counters).
     * @returns A promise resolving to {@link WebTransportConnectionStats}.
     * @throws WebTransportError (the promise rejects) if the session is closed.
     */
    public getStats(): Promise<WebTransportConnectionStats> {
        return this.core.getStats();
    }
}

/**
 * A WebTransport client session, matching the
 * [W3C WebTransport](https://w3c.github.io/webtransport/) interface.
 *
 * @remarks Constructs and begins connecting a native client session
 * immediately; setup (DNS, socket bind, TLS/QUIC handshake) runs asynchronously
 * on the driver thread. Await {@link WebTransportSession.ready | ready} before
 * use, and observe {@link WebTransportSession.closed | closed} for termination.
 * @example
 * ```ts
 * const wt = new WebTransport('https://example.com:4433/wt');
 * await wt.ready;
 * const stream = await wt.createBidirectionalStream();
 * ```
 */
export class WebTransport extends WebTransportSession {
    /**
     * Create and begin connecting a WebTransport client session.
     *
     * @param url - The session URL; must be an `https://` URL.
     * @param options - Session options: certificate pinning, extra headers,
     * origin, and the Node-specific `insecure` flag.
     * @defaultValue `options` defaults to `{}`.
     * @throws WebTransportError synchronously if {@link buildConfig} rejects the
     * URL or a certificate hash. Asynchronous setup failures (DNS, bind,
     * handshake) instead reject {@link WebTransportSession.ready | ready} and
     * {@link WebTransportSession.closed | closed}.
     * @remarks Validates and normalizes inputs via {@link buildConfig}, then
     * calls {@link createClientSession} to spawn the native driver thread and
     * passes the resulting {@link SessionCore} to the base constructor.
     */
    public constructor(url: string, options: WebTransportOptions = {}) {
        super(createClientSession(buildConfig(url, options)));
    }
}

/**
 * Call a controller's `close()`, swallowing the error it throws when the
 * controller is already closed or errored.
 *
 * @param controller - Any object exposing a `close()` method (a readable stream
 * default controller).
 * @internal
 */
function safeClose(controller: { close(): void }): void {
    try {
        controller.close();
    } catch {
        // already closed
    }
}

/**
 * Call a controller's `error()` with a reason, swallowing the error it throws
 * when the controller is already closed or errored.
 *
 * @param controller - Any object exposing an `error()` method (a readable stream
 * default controller).
 * @param reason - The failure reason to surface to the stream's consumer.
 * @internal
 */
function safeError(controller: { error(reason?: unknown): void }, reason: unknown): void {
    try {
        controller.error(reason);
    } catch {
        // already closed/errored
    }
}
