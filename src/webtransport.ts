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
    BinarySource,
    WebTransportCloseInfo,
    WebTransportCloseOptions,
    WebTransportOptions,
} from './types.js';

function toBytes(src: BinarySource): Uint8Array {
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
    private readonly session: Session;
    /** The session's datagram transport. */
    public readonly datagrams: WebTransportDatagramDuplexStream;
    /** Streams the peer opens bidirectionally. */
    public readonly incomingBidirectionalStreams: ReadableStream<WebTransportBidirectionalStream>;
    /** Streams the peer opens unidirectionally. */
    public readonly incomingUnidirectionalStreams: ReadableStream<WebTransportReceiveStream>;
    private closeCalled = false;

    public constructor(url: string, options: WebTransportOptions = {}) {
        const config = buildConfig(url, options);
        this.session = new Session(config);
        this.datagrams = new WebTransportDatagramDuplexStream(this.session);

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

        this.session.setIncomingHandler({
            onBidi: (id) => {
                bidiController.enqueue(new WebTransportBidirectionalStream(this.session, id));
            },
            onUni: (id) => {
                uniController.enqueue(new WebTransportReceiveStream(this.session, id));
            },
        });

        this.session.closed.promise.then(
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
    public get ready(): Promise<void> {
        return this.session.ready.promise;
    }

    /** Resolves when the session closes cleanly, rejects on abnormal termination. */
    public get closed(): Promise<WebTransportCloseInfo> {
        return this.session.closed.promise;
    }

    /** Open a new bidirectional stream. */
    public async createBidirectionalStream(): Promise<WebTransportBidirectionalStream> {
        const id = await this.session.openStream(true);
        return new WebTransportBidirectionalStream(this.session, id);
    }

    /** Open a new unidirectional (send) stream. */
    public async createUnidirectionalStream(): Promise<WebTransportSendStream> {
        const id = await this.session.openStream(false);
        return new WebTransportSendStream(this.session, id);
    }

    /** Close the session. */
    public close(closeInfo: WebTransportCloseOptions = {}): void {
        if (this.closeCalled) return;
        this.closeCalled = true;
        this.session.close(closeInfo.closeCode ?? 0, closeInfo.reason ?? '');
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
