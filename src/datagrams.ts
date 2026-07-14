// SPDX-License-Identifier: Apache-2.0
//! `WebTransportDatagramDuplexStream`: unreliable, unordered datagrams.

import type { SessionCore } from './native.js';

/**
 * The datagram transport for a session. `readable` yields inbound datagrams and
 * `writable` sends them; both are lossy by design (datagrams that don't fit the
 * queue are dropped rather than buffered indefinitely).
 *
 * @remarks
 * This mirrors the WHATWG `WebTransportDatagramDuplexStream` interface. One
 * instance is created per {@link SessionCore} and exposed as the `datagrams`
 * member of a session. Loss happens in two places: inbound datagrams that
 * arrive while the `readable` queue is already full are discarded (see the
 * datagram sink installed in the constructor), and outbound datagrams are
 * subject to the queue backpressure of `writable` plus whatever the native
 * QUIC layer chooses to drop when a packet cannot carry them.
 *
 * @see WebTransport for how a session exposes this stream.
 */
export class WebTransportDatagramDuplexStream {
    /**
     * The underlying session used to send datagrams, register the inbound sink,
     * query the maximum datagram size, and observe session closure.
     *
     * @internal
     */
    private readonly session: SessionCore;
    /**
     * Stream of inbound datagrams. Each chunk is one received datagram payload.
     * Backed by a {@link CountQueuingStrategy} whose high water mark is the
     * value of {@link incomingHighWaterMark} captured when this instance was
     * constructed. The stream is closed automatically when the session ends.
     */
    public readonly readable: ReadableStream<Uint8Array>;
    /**
     * Stream for sending outbound datagrams. Writing a chunk forwards it to
     * {@link SessionCore.sendDatagram}, whose boolean result (whether the
     * datagram was actually transmitted rather than dropped) is discarded, so
     * that `write()` resolves once the native transport reports back on the
     * send attempt. Backed by a {@link CountQueuingStrategy} whose high water
     * mark is the value of {@link outgoingHighWaterMark} captured when this
     * instance was constructed.
     */
    public readonly writable: WritableStream<Uint8Array>;

    /**
     * High water mark (in datagrams) for the inbound `readable` queue. Read
     * back through {@link incomingHighWaterMark}. Only the value present at
     * construction time is applied to the actual {@link readable} strategy;
     * later changes update the reported number but do not resize the live
     * queue.
     *
     * @defaultValue 64
     * @internal
     */
    private incomingHwm = 64;
    /**
     * High water mark (in datagrams) for the outbound `writable` queue. Read
     * back through {@link outgoingHighWaterMark}. Only the value present at
     * construction time is applied to the actual {@link writable} strategy;
     * later changes update the reported number but do not resize the live
     * queue.
     *
     * @defaultValue 64
     * @internal
     */
    private outgoingHwm = 64;

    /**
     * Max age (ms) an inbound datagram is retained before being dropped, or
     * `null` for no limit.
     *
     * @remarks
     * Present to match the WHATWG interface. This class stores the value but
     * does not currently enforce it: inbound loss here is driven only by the
     * `readable` queue being full, not by datagram age.
     *
     * @defaultValue null
     */
    public incomingMaxAge: number | null = null;
    /**
     * Max age (ms) an outbound datagram waits to be sent before being dropped,
     * or `null` for no limit.
     *
     * @remarks
     * Present to match the WHATWG interface. This class stores the value but
     * does not currently enforce it; outbound loss is governed by the
     * `writable` queue backpressure and the native QUIC layer.
     *
     * @defaultValue null
     */
    public outgoingMaxAge: number | null = null;

    /**
     * Build the datagram duplex stream over a session.
     *
     * @param session - The session that carries the datagrams. Its
     * {@link SessionCore.setDatagramSink} is used to receive inbound datagrams,
     * {@link SessionCore.sendDatagram} to send them, and its
     * {@link SessionCore.closed} promise to know when to close the inbound
     * stream.
     *
     * @remarks
     * Wiring performed here, in order:
     *
     * 1. Creates {@link readable} and captures its controller from `start`.
     * 2. Installs a datagram sink that enqueues each received datagram only
     *    while the controller's `desiredSize` is positive (treating an unknown
     *    `desiredSize` of `null` as `1`, i.e. room available); datagrams that
     *    arrive when the queue is full are dropped.
     * 3. Subscribes to the session's `closed` promise so the inbound stream is
     *    closed on both clean close and error, preventing a pending `read()`
     *    from hanging forever. A double-close is swallowed.
     * 4. Creates {@link writable} whose `write` forwards each chunk to
     *    `session.sendDatagram` and maps the boolean ack to `undefined`.
     */
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

        // Close the inbound datagram stream when the session ends, otherwise a
        // reader awaiting read() would hang forever after close.
        void session.closed.promise.then(
            () => {
                try {
                    rController.close();
                } catch {
                    // already closed
                }
            },
            () => {
                try {
                    rController.close();
                } catch {
                    // already closed
                }
            },
        );

        this.writable = new WritableStream<Uint8Array>(
            {
                write: (chunk) => session.sendDatagram(chunk).then(() => undefined),
            },
            new CountQueuingStrategy({ highWaterMark: this.outgoingHwm }),
        );
    }

    /**
     * The largest datagram payload that currently fits in a single packet.
     *
     * @returns The maximum datagram payload size in bytes as reported by the
     * native transport, or `0` when the session has no transport attached yet.
     * The value can change over the life of the session as the path MTU is
     * discovered.
     */
    public get maxDatagramSize(): number {
        return this.session.maxDatagramSize();
    }

    /**
     * The high water mark (in datagrams) reported for the inbound `readable`
     * queue.
     *
     * @returns The most recently set incoming high water mark.
     */
    public get incomingHighWaterMark(): number {
        return this.incomingHwm;
    }
    /**
     * Set the reported inbound high water mark.
     *
     * @param value - Desired high water mark; coerced to an integer with
     * `value | 0` and clamped to a minimum of `1`.
     *
     * @remarks
     * This updates only the number returned by {@link incomingHighWaterMark}.
     * The live {@link readable} queue keeps the strategy fixed at construction
     * time, so changing this after construction does not resize it.
     */
    public set incomingHighWaterMark(value: number) {
        this.incomingHwm = Math.max(1, value | 0);
    }

    /**
     * The high water mark (in datagrams) reported for the outbound `writable`
     * queue.
     *
     * @returns The most recently set outgoing high water mark.
     */
    public get outgoingHighWaterMark(): number {
        return this.outgoingHwm;
    }
    /**
     * Set the reported outbound high water mark.
     *
     * @param value - Desired high water mark; coerced to an integer with
     * `value | 0` and clamped to a minimum of `1`.
     *
     * @remarks
     * This updates only the number returned by {@link outgoingHighWaterMark}.
     * The live {@link writable} queue keeps the strategy fixed at construction
     * time, so changing this after construction does not resize it.
     */
    public set outgoingHighWaterMark(value: number) {
        this.outgoingHwm = Math.max(1, value | 0);
    }
}
