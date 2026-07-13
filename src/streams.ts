// SPDX-License-Identifier: Apache-2.0
//! WebTransport stream classes, implemented as WHATWG Readable/Writable streams
//! with end-to-end backpressure onto the native QUIC flow-control window.

import { WebTransportError } from './errors.js';
import type { Session } from './native.js';

/** Derive a QUIC/WebTransport application error code from an abort/cancel reason. */
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
 */
export class WebTransportReceiveStream extends ReadableStream<Uint8Array> {
    readonly #streamId: number;

    constructor(session: Session, streamId: number) {
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
        });

        this.#streamId = streamId;
    }

    /** The underlying QUIC stream id. */
    get streamId(): number {
        return this.#streamId;
    }
}

/**
 * The writable half of a WebTransport stream. Each `write()` resolves only once
 * the bytes have been accepted into quiche's send buffer, giving natural
 * backpressure gated by the peer's flow-control window.
 */
export class WebTransportSendStream extends WritableStream<Uint8Array> {
    readonly #streamId: number;

    constructor(session: Session, streamId: number) {
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
        });

        this.#streamId = streamId;
    }

    /** The underlying QUIC stream id. */
    get streamId(): number {
        return this.#streamId;
    }
}

/** A bidirectional WebTransport stream: a readable + a writable half. */
export class WebTransportBidirectionalStream {
    readonly #readable: WebTransportReceiveStream;
    readonly #writable: WebTransportSendStream;

    constructor(session: Session, streamId: number) {
        this.#readable = new WebTransportReceiveStream(session, streamId);
        this.#writable = new WebTransportSendStream(session, streamId);
    }

    get readable(): WebTransportReceiveStream {
        return this.#readable;
    }

    get writable(): WebTransportSendStream {
        return this.#writable;
    }
}
