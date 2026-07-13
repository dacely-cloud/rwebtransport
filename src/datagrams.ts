// SPDX-License-Identifier: Apache-2.0
//! `WebTransportDatagramDuplexStream` — unreliable, unordered datagrams.

import type { SessionCore } from './native.js';

/**
 * The datagram transport for a session. `readable` yields inbound datagrams and
 * `writable` sends them; both are lossy by design (datagrams that don't fit the
 * queue are dropped rather than buffered indefinitely).
 */
export class WebTransportDatagramDuplexStream {
    private readonly session: SessionCore;
    public readonly readable: ReadableStream<Uint8Array>;
    public readonly writable: WritableStream<Uint8Array>;

    private incomingHwm = 64;
    private outgoingHwm = 64;

    /** Max age (ms) an inbound datagram is retained before being dropped, or null. */
    public incomingMaxAge: number | null = null;
    /** Max age (ms) an outbound datagram waits to be sent before being dropped, or null. */
    public outgoingMaxAge: number | null = null;

    public constructor(session: SessionCore) {
        this.session = session;

        let rController!: ReadableStreamDefaultController<Uint8Array>;
        this.readable = new ReadableStream<Uint8Array>(
            {
                start(c) {
                    rController = c;
                },
            },
            new CountQueuingStrategy({ highWaterMark: this.incomingHwm }),
        );

        session.setDatagramSink((data) => {
            // Unreliable: drop when the inbound queue is already full.
            if ((rController.desiredSize ?? 1) > 0) {
                rController.enqueue(data);
            }
        });

        this.writable = new WritableStream<Uint8Array>(
            {
                write: (chunk) => session.sendDatagram(chunk).then(() => undefined),
            },
            new CountQueuingStrategy({ highWaterMark: this.outgoingHwm }),
        );
    }

    /** The largest datagram payload that currently fits in a single packet. */
    public get maxDatagramSize(): number {
        return this.session.maxDatagramSize();
    }

    public get incomingHighWaterMark(): number {
        return this.incomingHwm;
    }
    public set incomingHighWaterMark(value: number) {
        this.incomingHwm = Math.max(1, value | 0);
    }

    public get outgoingHighWaterMark(): number {
        return this.outgoingHwm;
    }
    public set outgoingHighWaterMark(value: number) {
        this.outgoingHwm = Math.max(1, value | 0);
    }
}
