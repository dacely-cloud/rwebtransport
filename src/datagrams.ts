// SPDX-License-Identifier: Apache-2.0
//! `WebTransportDatagramDuplexStream` — unreliable, unordered datagrams.

import type { Session } from './native.js';

/**
 * The datagram transport for a session. `readable` yields inbound datagrams and
 * `writable` sends them; both are lossy by design (datagrams that don't fit the
 * queue are dropped rather than buffered indefinitely).
 */
export class WebTransportDatagramDuplexStream {
  readonly #session: Session;
  readonly #readable: ReadableStream<Uint8Array>;
  readonly #writable: WritableStream<Uint8Array>;

  #incomingHighWaterMark = 64;
  #outgoingHighWaterMark = 64;

  /** Max age (ms) an inbound datagram is retained before being dropped, or null. */
  incomingMaxAge: number | null = null;
  /** Max age (ms) an outbound datagram waits to be sent before being dropped, or null. */
  outgoingMaxAge: number | null = null;

  constructor(session: Session) {
    this.#session = session;

    let rController!: ReadableStreamDefaultController<Uint8Array>;
    this.#readable = new ReadableStream<Uint8Array>(
      {
        start(c) {
          rController = c;
        },
      },
      new CountQueuingStrategy({ highWaterMark: this.#incomingHighWaterMark }),
    );

    session.setDatagramSink((data) => {
      // Unreliable: drop when the inbound queue is already full.
      if ((rController.desiredSize ?? 1) > 0) {
        rController.enqueue(data);
      }
    });

    this.#writable = new WritableStream<Uint8Array>(
      {
        write: (chunk) => session.sendDatagram(chunk).then(() => undefined),
      },
      new CountQueuingStrategy({ highWaterMark: this.#outgoingHighWaterMark }),
    );
  }

  get readable(): ReadableStream<Uint8Array> {
    return this.#readable;
  }

  get writable(): WritableStream<Uint8Array> {
    return this.#writable;
  }

  /** The largest datagram payload that currently fits in a single packet. */
  get maxDatagramSize(): number {
    return this.#session.maxDatagramSize();
  }

  get incomingHighWaterMark(): number {
    return this.#incomingHighWaterMark;
  }
  set incomingHighWaterMark(value: number) {
    this.#incomingHighWaterMark = Math.max(1, value | 0);
  }

  get outgoingHighWaterMark(): number {
    return this.#outgoingHighWaterMark;
  }
  set outgoingHighWaterMark(value: number) {
    this.#outgoingHighWaterMark = Math.max(1, value | 0);
  }
}
