// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests: the real client against the real quiche echo server.

import { afterAll, beforeAll, describe, expect, it } from 'vitest';

import { WebTransport, WebTransportError } from '../src/index.js';
import { startEchoServer, type EchoServer } from './helpers/echo-server.js';

const enc = new TextEncoder();
const dec = new TextDecoder();

function bytes(s: string): Uint8Array {
    return enc.encode(s);
}
function text(b: Uint8Array): string {
    return dec.decode(b);
}

/** Read a whole ReadableStream to a single Uint8Array. */
async function readAll(stream: ReadableStream<Uint8Array>): Promise<Uint8Array> {
    const reader = stream.getReader();
    const chunks: Uint8Array[] = [];
    let total = 0;
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) {
            chunks.push(value);
            total += value.byteLength;
        }
    }
    const out = new Uint8Array(total);
    let off = 0;
    for (const c of chunks) {
        out.set(c, off);
        off += c.byteLength;
    }
    return out;
}

let server: EchoServer;

beforeAll(async () => {
    server = await startEchoServer();
});

afterAll(() => {
    server?.stop();
});

function connect(): WebTransport {
    return new WebTransport(server.url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: server.certHash }],
    });
}

describe('WebTransport session', () => {
    it('connects and resolves ready', async () => {
        const wt = connect();
        await expect(wt.ready).resolves.toBeUndefined();
        wt.close();
        await wt.closed;
    });

    it('resolves closed after close()', async () => {
        const wt = connect();
        await wt.ready;
        wt.close({ closeCode: 7, reason: 'done' });
        const info = await wt.closed;
        expect(info).toBeTypeOf('object');
    });

    it('rejects ready with a wrong certificate hash', async () => {
        const wrong = new Uint8Array(32).fill(0xab);
        const wt = new WebTransport(server.url, {
            serverCertificateHashes: [{ algorithm: 'sha-256', value: wrong }],
        });
        await expect(wt.ready).rejects.toBeInstanceOf(WebTransportError);
    });

    it('throws synchronously on a non-https URL', () => {
        expect(() => new WebTransport('http://example.com/x')).toThrow(WebTransportError);
    });
});

describe('Bidirectional streams', () => {
    it('echoes a small message', async () => {
        const wt = connect();
        await wt.ready;

        const stream = await wt.createBidirectionalStream();
        const writer = stream.writable.getWriter();
        await writer.write(bytes('hello quic'));
        await writer.close();

        const received = await readAll(stream.readable);
        expect(text(received)).toBe('hello quic');

        wt.close();
    });

    it('echoes a large payload with correct ordering', async () => {
        const wt = connect();
        await wt.ready;

        const stream = await wt.createBidirectionalStream();
        const writer = stream.writable.getWriter();

        const SIZE = 256 * 1024;
        const payload = new Uint8Array(SIZE);
        for (let i = 0; i < SIZE; i++) payload[i] = i & 0xff;

        // Write in chunks to exercise flow control + reassembly.
        const CHUNK = 16 * 1024;
        for (let off = 0; off < SIZE; off += CHUNK) {
            await writer.write(payload.subarray(off, Math.min(off + CHUNK, SIZE)));
        }
        await writer.close();

        const received = await readAll(stream.readable);
        expect(received.byteLength).toBe(SIZE);
        expect(received).toEqual(payload);

        wt.close();
    });

    it('handles multiple concurrent bidirectional streams', async () => {
        const wt = connect();
        await wt.ready;

        const messages = ['alpha', 'bravo', 'charlie', 'delta', 'echo'];
        const results = await Promise.all(
            messages.map(async (msg) => {
                const stream = await wt.createBidirectionalStream();
                const writer = stream.writable.getWriter();
                await writer.write(bytes(msg));
                await writer.close();
                return text(await readAll(stream.readable));
            }),
        );
        expect(results).toEqual(messages);

        wt.close();
    });
});

describe('Unidirectional streams', () => {
    it('echoes a uni stream back on an incoming uni stream', async () => {
        const wt = connect();
        await wt.ready;

        const send = await wt.createUnidirectionalStream();
        const writer = send.getWriter();
        await writer.write(bytes('one way'));
        await writer.close();

        const reader = wt.incomingUnidirectionalStreams.getReader();
        const { value: incoming, done } = await reader.read();
        expect(done).toBe(false);
        expect(incoming).toBeDefined();
        const echoed = await readAll(incoming!);
        expect(text(echoed)).toBe('one way');

        wt.close();
    });
});

describe('Datagrams', () => {
    it('echoes datagrams', async () => {
        const wt = connect();
        await wt.ready;

        const writer = wt.datagrams.writable.getWriter();
        const reader = wt.datagrams.readable.getReader();

        const payload = bytes('ping-datagram');

        // Datagrams are unreliable; send a few and wait for the first echo.
        const received = await new Promise<Uint8Array>((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error('no datagram echo received')), 8000);
            reader.read().then(({ value }) => {
                clearTimeout(timer);
                resolve(value!);
            }, reject);
            const pump = async () => {
                for (let i = 0; i < 20; i++) {
                    await writer.write(payload);
                    await new Promise((r) => setTimeout(r, 100));
                }
            };
            void pump();
        });

        expect(text(received)).toBe('ping-datagram');
        expect(wt.datagrams.maxDatagramSize).toBeGreaterThan(0);

        wt.close();
    });
});
