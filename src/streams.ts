// SPDX-License-Identifier: Apache-2.0
//! WebTransport stream classes, implemented as WHATWG Readable/Writable streams
//! with end-to-end backpressure onto the native QUIC flow-control window.

import { WebTransportError } from './errors.js';
import type { SessionCore } from './native.js';

/**
 * Derive a QUIC/WebTransport application error code from an abort or cancel reason.
 *
 * @param reason - The value handed to a stream `cancel(reason)` or `abort(reason)`.
 * @returns The stream's application error code as an unsigned 32-bit integer: it is
 *   {@link WebTransportError.streamErrorCode} (coerced with `>>> 0`) when `reason` is
 *   a {@link WebTransportError} carrying a non-null code, and `0` for every other
 *   value, including plain errors and `undefined`.
 * @remarks Translates a JS-level abort or cancel into the numeric code carried by the
 *   QUIC `STOP_SENDING` and `RESET_STREAM` frames sent to the peer.
 * @internal
 */
function errorCode(reason: unknown): number {
    if (reason instanceof WebTransportError && reason.streamErrorCode != null) {
        return reason.streamErrorCode >>> 0;
    }
    return 0;
}

/**
 * The readable half of a WebTransport stream. A `ReadableStream<Uint8Array>`
 * whose queue high-water mark drives native read backpressure: when the JS queue
 * fills, the native side stops draining quiche, flow-controlling the peer.
 *
 * @remarks Subclasses the WHATWG `ReadableStream`, so all standard reader APIs
 *   (`getReader`, async iteration, `pipeTo`, `cancel`, and so on) work as usual.
 *   Inbound bytes arrive as native `streamData` events routed through the session's
 *   {@link ReceiveSink}; a peer FIN closes the stream and a peer reset errors it with
 *   a {@link WebTransportError}.
 */
export class WebTransportReceiveStream extends ReadableStream<Uint8Array> {
    /**
     * The QUIC stream id of the underlying stream. Stable for the lifetime of the
     * stream and shared with the paired {@link WebTransportSendStream} when this is
     * one half of a {@link WebTransportBidirectionalStream}.
     */
    public readonly streamId: number;

    /**
     * Wire a `ReadableStream` up to a native receive stream.
     *
     * @param session - The session core that dispatches native events and issues
     *   flow-control and teardown commands for this stream.
     * @param streamId - The QUIC stream id to receive from.
     * @remarks Installs the `ReadableStream` underlying source and registers a
     *   {@link ReceiveSink} on `session` for `streamId`:
     *
     *   - `pull` unpauses the native stream via `setPaused(streamId, false)`, so
     *     quiche resumes draining inbound bytes when the consumer wants more.
     *   - `cancel(reason)` sends `STOP_SENDING` to the peer with the code from
     *     {@link errorCode} and then unregisters the receive sink.
     *   - The sink's `onData` enqueues each chunk and, once the controller's
     *     `desiredSize` falls to zero or below (a nullish `desiredSize` is treated as
     *     `1`, so it does not pause), pauses the native stream to flow-control the
     *     peer.
     *   - The sink's `onFin` closes the controller (swallowing the error if it was
     *     already closed or errored) and unregisters.
     *   - The sink's `onReset(code)` errors the controller with a `'stream'`-sourced
     *     {@link WebTransportError} ('stream reset by peer') carrying the peer's code,
     *     then unregisters.
     *
     *   A {@link CountQueuingStrategy} with a high-water mark of 32 chunks bounds the
     *   JS-side queue and thereby the pause threshold.
     */
    public constructor(session: SessionCore, streamId: number) {
        let controller!: ReadableStreamDefaultController<Uint8Array>;
        super(
            {
                start(c) {
                    controller = c;
                },
                pull() {
                    session.setPaused(streamId, false);
                },
                cancel(reason) {
                    session.stopSending(streamId, errorCode(reason));
                    session.unregisterReceive(streamId);
                },
            },
            new CountQueuingStrategy({ highWaterMark: 32 }),
        );

        session.registerReceive(streamId, {
            onData(chunk) {
                controller.enqueue(chunk);
                if ((controller.desiredSize ?? 1) <= 0) {
                    session.setPaused(streamId, true);
                }
            },
            onFin() {
                try {
                    controller.close();
                } catch {
                    // already closed/errored
                }
                session.unregisterReceive(streamId);
            },
            onReset(code) {
                controller.error(
                    new WebTransportError('stream reset by peer', {
                        source: 'stream',
                        streamErrorCode: code,
                    }),
                );
                session.unregisterReceive(streamId);
            },
            onSessionClose(error) {
                try {
                    controller.error(error);
                } catch {
                    // already closed/errored
                }
                session.unregisterReceive(streamId);
            },
        });

        this.streamId = streamId;
    }
}

/**
 * The writable half of a WebTransport stream. Each `write()` resolves only once
 * the bytes have been accepted into quiche's send buffer, giving natural
 * backpressure gated by the peer's flow-control window.
 *
 * @remarks Subclasses the WHATWG `WritableStream`, so all standard writer APIs
 *   (`getWriter`, `write`, `close`, `abort`, `pipeTo` targets, and so on) work as
 *   usual. A peer `STOP_SENDING` errors the stream through the session's
 *   {@link SendSink}.
 */
/**
 * An opaque handle for grouping streams so their sends can be scheduled relative
 * to one another via each stream's {@link WebTransportSendStream.sendOrder}.
 * Create one with `WebTransport.createSendGroup()`. Accepted for W3C API parity;
 * the grouping is a best-effort hint (quiche schedules by its own urgency).
 */
export class WebTransportSendGroup {}

/** Options for creating a send stream (W3C `WebTransportSendStreamOptions`). */
export interface WebTransportSendStreamOptions {
    /** The {@link WebTransportSendGroup} to associate the new stream with. */
    sendGroup?: WebTransportSendGroup;
    /** Relative send priority; higher is nominally sent first. Defaults to `0`. */
    sendOrder?: number;
    /**
     * Wait for stream-creation credit instead of failing when it is exhausted.
     * Accepted for parity; stream creation here does not currently fail on credit.
     */
    waitUntilAvailable?: boolean;
}

export class WebTransportSendStream extends WritableStream<Uint8Array> {
    /**
     * The QUIC stream id of the underlying stream. Stable for the lifetime of the
     * stream and shared with the paired {@link WebTransportReceiveStream} when this is
     * one half of a {@link WebTransportBidirectionalStream}.
     */
    public readonly streamId: number;
    /**
     * The {@link WebTransportSendGroup} this stream belongs to, or `null`.
     * Mutable per the W3C API; grouping is a best-effort scheduling hint.
     */
    public sendGroup: WebTransportSendGroup | null;
    /**
     * Relative send priority within the send group. Higher values are nominally
     * sent first. Mutable per the W3C API; the effect on the wire is best-effort.
     */
    public sendOrder: number;

    /**
     * Wire a `WritableStream` up to a native send stream.
     *
     * @param session - The session core that dispatches native events and issues
     *   write, FIN, reset, and teardown commands for this stream.
     * @param streamId - The QUIC stream id to send on.
     * @remarks Installs the `WritableStream` underlying sink and registers a
     *   {@link SendSink} on `session` for `streamId`:
     *
     *   - `write(chunk)` forwards to `session.write` and returns its promise, which
     *     settles only once the bytes are accepted into quiche's send buffer; awaiting
     *     that promise is what applies end-to-end backpressure.
     *   - `close` sends a FIN via `finStream` and unregisters the send sink.
     *   - `abort(reason)` resets the stream via `resetStream` with the code from
     *     {@link errorCode} and unregisters the send sink.
     *   - The sink's `onStopSending(code)` errors the controller with a
     *     `'stream'`-sourced {@link WebTransportError} ('peer sent STOP_SENDING')
     *     carrying the peer's code (swallowing the error if the stream was already
     *     errored or closed) and unregisters.
     *
     *   A {@link ByteLengthQueuingStrategy} with a high-water mark of 1 MiB
     *   (`1024 * 1024` bytes) bounds the JS-side write queue.
     */
    public constructor(
        session: SessionCore,
        streamId: number,
        options: WebTransportSendStreamOptions = {},
    ) {
        let controller!: WritableStreamDefaultController;
        super(
            {
                start(c) {
                    controller = c;
                },
                write(chunk) {
                    return session.write(streamId, chunk);
                },
                close() {
                    session.finStream(streamId);
                    session.unregisterSend(streamId);
                },
                abort(reason) {
                    session.resetStream(streamId, errorCode(reason));
                    session.unregisterSend(streamId);
                },
            },
            new ByteLengthQueuingStrategy({ highWaterMark: 1024 * 1024 }),
        );

        session.registerSend(streamId, {
            onStopSending(code) {
                try {
                    controller.error(
                        new WebTransportError('peer sent STOP_SENDING', {
                            source: 'stream',
                            streamErrorCode: code,
                        }),
                    );
                } catch {
                    // already errored/closed
                }
                session.unregisterSend(streamId);
            },
            onSessionClose(error) {
                try {
                    controller.error(error);
                } catch {
                    // already errored/closed
                }
                session.unregisterSend(streamId);
            },
        });

        this.streamId = streamId;
        this.sendGroup = options.sendGroup ?? null;
        this.sendOrder = options.sendOrder ?? 0;
    }
}

/**
 * A bidirectional WebTransport stream: a {@link WebTransportReceiveStream} and a
 * {@link WebTransportSendStream} over a single QUIC stream id, exposed as `readable`
 * and `writable` halves. Mirrors the shape of the browser
 * `WebTransportBidirectionalStream`.
 */
export class WebTransportBidirectionalStream {
    /** The readable half, delivering bytes the peer sends on this stream. */
    public readonly readable: WebTransportReceiveStream;
    /** The writable half, sending bytes to the peer on this stream. */
    public readonly writable: WebTransportSendStream;

    /**
     * Construct both halves over the same QUIC stream.
     *
     * @param session - The session core backing both halves.
     * @param streamId - The QUIC stream id shared by the readable and writable halves.
     */
    public constructor(
        session: SessionCore,
        streamId: number,
        options: WebTransportSendStreamOptions = {},
    ) {
        this.readable = new WebTransportReceiveStream(session, streamId);
        this.writable = new WebTransportSendStream(session, streamId, options);
    }
}
