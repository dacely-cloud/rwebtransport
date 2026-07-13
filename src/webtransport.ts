// SPDX-License-Identifier: Apache-2.0
//! The `WebTransport` class — the W3C entry point.

import { Session, type ConnectConfig } from './native.js';
import { WebTransportError } from './errors.js';
import { WebTransportDatagramDuplexStream } from './datagrams.js';
import {
    WebTransportBidirectionalStream,
    WebTransportReceiveStream,
    WebTransportSendStream,
} from './streams.js';
import type {
    WebTransportCloseInfo,
    WebTransportCloseOptions,
    WebTransportOptions,
} from './types.js';

function toBytes(src: BufferSource): Uint8Array {
    if (src instanceof ArrayBuffer) return new Uint8Array(src);
    return new Uint8Array(src.buffer, src.byteOffset, src.byteLength);
}

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
 * A WebTransport session, matching the [W3C WebTransport](https://w3c.github.io/webtransport/)
 * interface.
 */
export class WebTransport {
    readonly #session: Session;
    readonly #datagrams: WebTransportDatagramDuplexStream;
    readonly #incomingBidi: ReadableStream<WebTransportBidirectionalStream>;
    readonly #incomingUni: ReadableStream<WebTransportReceiveStream>;
    #closeCalled = false;

    constructor(url: string, options: WebTransportOptions = {}) {
        const config = buildConfig(url, options);
        this.#session = new Session(config);
        this.#datagrams = new WebTransportDatagramDuplexStream(this.#session);

        let bidiController!: ReadableStreamDefaultController<WebTransportBidirectionalStream>;
        let uniController!: ReadableStreamDefaultController<WebTransportReceiveStream>;
        this.#incomingBidi = new ReadableStream<WebTransportBidirectionalStream>({
            start: (c) => {
                bidiController = c;
            },
        });
        this.#incomingUni = new ReadableStream<WebTransportReceiveStream>({
            start: (c) => {
                uniController = c;
            },
        });

        this.#session.setIncomingHandler({
            onBidi: (id) => {
                bidiController.enqueue(new WebTransportBidirectionalStream(this.#session, id));
            },
            onUni: (id) => {
                uniController.enqueue(new WebTransportReceiveStream(this.#session, id));
            },
        });

        this.#session.closed.promise.then(
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

    /** Resolves when the session is established. */
    get ready(): Promise<void> {
        return this.#session.ready.promise;
    }

    /** Resolves when the session closes cleanly, rejects on abnormal termination. */
    get closed(): Promise<WebTransportCloseInfo> {
        return this.#session.closed.promise;
    }

    /** The session's datagram transport. */
    get datagrams(): WebTransportDatagramDuplexStream {
        return this.#datagrams;
    }

    /** Streams the peer opens bidirectionally. */
    get incomingBidirectionalStreams(): ReadableStream<WebTransportBidirectionalStream> {
        return this.#incomingBidi;
    }

    /** Streams the peer opens unidirectionally. */
    get incomingUnidirectionalStreams(): ReadableStream<WebTransportReceiveStream> {
        return this.#incomingUni;
    }

    /** Open a new bidirectional stream. */
    async createBidirectionalStream(): Promise<WebTransportBidirectionalStream> {
        const id = await this.#session.openStream(true);
        return new WebTransportBidirectionalStream(this.#session, id);
    }

    /** Open a new unidirectional (send) stream. */
    async createUnidirectionalStream(): Promise<WebTransportSendStream> {
        const id = await this.#session.openStream(false);
        return new WebTransportSendStream(this.#session, id);
    }

    /** Close the session. */
    close(closeInfo: WebTransportCloseOptions = {}): void {
        if (this.#closeCalled) return;
        this.#closeCalled = true;
        this.#session.close(closeInfo.closeCode ?? 0, closeInfo.reason ?? '');
    }
}

function safeClose(controller: { close(): void }): void {
    try {
        controller.close();
    } catch {
        // already closed
    }
}

function safeError(controller: { error(reason?: unknown): void }, reason: unknown): void {
    try {
        controller.error(reason);
    } catch {
        // already closed/errored
    }
}
