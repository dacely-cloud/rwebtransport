// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests of the WebTransport **server**: our own client talks to our
//! own server (both native quiche), no external process.

import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { afterEach, describe, expect, it } from 'vitest';

import {
    WebTransport,
    WebTransportError,
    WebTransportServer,
    type WebTransportServerSession,
} from '../src/index.js';
import { certHash } from './helpers/echo-server.js';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const CERT = join(ROOT, 'test', 'fixtures', 'cert.pem');
const KEY = join(ROOT, 'test', 'fixtures', 'key.pem');

const enc = new TextEncoder();
const dec = new TextDecoder();

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

const servers: WebTransportServer[] = [];
const clients: WebTransport[] = [];

afterEach(() => {
    for (const c of clients) c.close();
    for (const s of servers) s.close();
    clients.length = 0;
    servers.length = 0;
});

/** An echoing server: bidi via pipe, uni re-emitted on a new uni, datagrams back. */
async function echoServer(): Promise<string> {
    const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
    servers.push(server);
    await server.ready;

    void (async () => {
        const reader = server.incomingSessions.getReader();
        for (;;) {
            const { value: session, done } = await reader.read();
            if (done) break;
            if (session) handleSession(session);
        }
    })();

    return `https://127.0.0.1:${server.port}/echo`;
}

function handleSession(session: WebTransportServerSession): void {
    // Each loop below runs detached for the session's lifetime. Tearing the
    // session down mid-read/mid-write rejects the in-flight stream op, so every
    // loop swallows that terminal rejection instead of leaking it as unhandled.

    // Bidirectional streams: echo by piping readable → writable.
    void (async () => {
        const reader = session.incomingBidirectionalStreams.getReader();
        for (;;) {
            const { value: stream, done } = await reader.read();
            if (done) break;
            if (stream) void stream.readable.pipeTo(stream.writable).catch(() => undefined);
        }
    })().catch(() => undefined);

    // Unidirectional streams: read the payload, echo on a fresh uni stream.
    void (async () => {
        const reader = session.incomingUnidirectionalStreams.getReader();
        for (;;) {
            const { value: recv, done } = await reader.read();
            if (done) break;
            if (!recv) continue;
            const data = await readAll(recv);
            const send = await session.createUnidirectionalStream();
            const w = send.getWriter();
            await w.write(data);
            await w.close();
        }
    })().catch(() => undefined);

    // Datagrams: echo back.
    void (async () => {
        const r = session.datagrams.readable.getReader();
        const w = session.datagrams.writable.getWriter();
        for (;;) {
            const { value, done } = await r.read();
            if (done) break;
            if (value) await w.write(value);
        }
    })().catch(() => undefined);
}

function connect(url: string): WebTransport {
    const wt = new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash() }],
    });
    clients.push(wt);
    return wt;
}

describe('WebTransportServer', () => {
    it('becomes ready and reports a bound port', async () => {
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
        servers.push(server);
        await expect(server.ready).resolves.toBeUndefined();
        expect(server.port).toBeGreaterThan(0);
    });

    it('accepts a session and exposes the request path', async () => {
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
        servers.push(server);
        await server.ready;

        const sessionPromise = (async () => {
            const reader = server.incomingSessions.getReader();
            const { value } = await reader.read();
            return value;
        })();

        const wt = connect(`https://127.0.0.1:${server.port}/chat/room1`);
        await wt.ready;

        const session = await sessionPromise;
        expect(session).toBeDefined();
        expect(session!.path).toBe('/chat/room1');
        // The client connects over loopback, so the server sees 127.0.0.1 and
        // the client's ephemeral UDP port.
        expect(session!.remoteAddress).toBe('127.0.0.1');
        expect(session!.remotePort).toBeGreaterThan(0);
    });

    it('echoes a bidirectional stream client → server → client', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;

        const stream = await wt.createBidirectionalStream();
        const writer = stream.writable.getWriter();
        await writer.write(enc.encode('hello server'));
        await writer.close();

        expect(dec.decode(await readAll(stream.readable))).toBe('hello server');
    });

    it('echoes a large bidirectional payload', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;

        const SIZE = 128 * 1024;
        const payload = new Uint8Array(SIZE);
        for (let i = 0; i < SIZE; i++) payload[i] = i & 0xff;

        const stream = await wt.createBidirectionalStream();
        const writer = stream.writable.getWriter();
        for (let off = 0; off < SIZE; off += 16 * 1024) {
            await writer.write(payload.subarray(off, Math.min(off + 16 * 1024, SIZE)));
        }
        await writer.close();

        const received = await readAll(stream.readable);
        expect(received.byteLength).toBe(SIZE);
        expect(received).toEqual(payload);
    });

    it('resolves the client draining promise when the server drains the session', async () => {
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
        servers.push(server);
        await server.ready;
        void (async () => {
            const sreader = server.incomingSessions.getReader();
            const { value: session } = await sreader.read();
            if (session) session.drain(); // send DRAIN_WEBTRANSPORT_SESSION
        })();

        const wt = connect(`https://127.0.0.1:${server.port}/drain`);
        await wt.ready;
        // The session stays usable; draining just signals intent to close soon.
        await expect(wt.draining).resolves.toBeUndefined();
        expect(wt.reliability).toBe('supports-unreliable');
    });

    it('delivers the WebTransport close code and reason to the peer on graceful close', async () => {
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
        servers.push(server);
        await server.ready;
        const serverSessionP = (async () => {
            const reader = server.incomingSessions.getReader();
            const { value } = await reader.read();
            return value!;
        })();

        const wt = connect(`https://127.0.0.1:${server.port}/close`);
        await wt.ready;
        const serverSession = await serverSessionP;

        wt.close({ closeCode: 0x1234, reason: 'bye now' });

        // Local closeInfo carries the WebTransport code/reason (the wire close
        // itself uses H3_NO_ERROR, not the raw code).
        await expect(wt.closed).resolves.toEqual({ closeCode: 0x1234, reason: 'bye now' });
        // The peer receives the CLOSE_WEBTRANSPORT_SESSION capsule (proving it is
        // serialized before the QUIC close) with the same code/reason.
        await expect(serverSession.closed).resolves.toEqual({
            closeCode: 0x1234,
            reason: 'bye now',
        });
    });

    it('settles the draining promise when the session closes without a prior DRAIN', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;
        // No DRAIN capsule is ever sent; closing must still settle `draining` so
        // an `await session.draining` cannot hang past the session's end.
        wt.close();
        await expect(wt.draining).resolves.toBeUndefined();
    });

    it('counts droppedIncoming datagrams when the inbound queue overflows', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;
        // Never read wt.datagrams.readable: echoed datagrams pile into the
        // bounded inbound queue (high-water mark 64) and overflow, which must be
        // counted as droppedIncoming.
        const writer = wt.datagrams.writable.getWriter();
        const payload = enc.encode('flood');
        for (let i = 0; i < 400; i++) await writer.write(payload);
        await new Promise((r) => setTimeout(r, 600));
        const stats = await wt.getStats();
        expect(stats.datagrams.droppedIncoming).toBeGreaterThan(0);
    });

    it('accepts sendOrder / sendGroup on stream creation and exposes them', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;
        const group = wt.createSendGroup();
        const stream = await wt.createBidirectionalStream({ sendOrder: 7, sendGroup: group });
        expect(stream.writable.sendOrder).toBe(7);
        expect(stream.writable.sendGroup).toBe(group);
        // Mutable per the W3C API.
        stream.writable.sendOrder = 3;
        expect(stream.writable.sendOrder).toBe(3);

        const uni = await wt.createUnidirectionalStream({ sendOrder: 1 });
        expect(uni.sendOrder).toBe(1);
        expect(uni.sendGroup).toBeNull();
    });

    it('getStats reports connection statistics', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;
        // Exchange data so the counters are non-zero.
        const stream = await wt.createBidirectionalStream();
        const w = stream.writable.getWriter();
        await w.write(enc.encode('collect some stats'));
        await w.close();
        await readAll(stream.readable);

        const stats = await wt.getStats();
        expect(stats.bytesSent).toBeGreaterThan(0);
        expect(stats.bytesReceived).toBeGreaterThan(0);
        expect(stats.packetsSent).toBeGreaterThan(0);
        expect(stats.packetsReceived).toBeGreaterThan(0);
        expect(typeof stats.smoothedRtt).toBe('number');
        expect(stats.minRtt).toBeGreaterThanOrEqual(0);
        expect(stats.datagrams).toEqual({
            expiredOutgoing: 0,
            droppedIncoming: 0,
            lostOutgoing: 0,
            expiredIncoming: 0,
        });
    });

    it('reports reliability pending before ready, supports-unreliable after', async () => {
        const url = await echoServer();
        const wt = connect(url);
        expect(wt.reliability).toBe('pending');
        await wt.ready;
        expect(wt.reliability).toBe('supports-unreliable');
    });

    it('exportKeyingMaterial derives identical bytes on both endpoints', async () => {
        // Both peers share one TLS session, so RFC 5705 export with the same
        // label/context/length MUST yield identical bytes end to end.
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
        servers.push(server);
        await server.ready;
        const serverSessionP = (async () => {
            const reader = server.incomingSessions.getReader();
            const { value } = await reader.read();
            return value!;
        })();

        const wt = connect(`https://127.0.0.1:${server.port}/ekm`);
        await wt.ready;
        const serverSession = await serverSessionP;

        const eq = (a: Uint8Array, b: Uint8Array) =>
            a.length === b.length && a.every((v, i) => v === b[i]);

        const label = enc.encode('rwebtransport exporter test');
        const context = enc.encode('context-abcd');
        const clientKm = await wt.exportKeyingMaterial(label, context, 32);
        const serverKm = await serverSession.exportKeyingMaterial(label, context, 32);

        expect(clientKm).toBeInstanceOf(Uint8Array);
        expect(clientKm.byteLength).toBe(32);
        expect(eq(clientKm, serverKm)).toBe(true);

        // A different label yields unrelated material.
        const other = await wt.exportKeyingMaterial(enc.encode('other label'), context, 32);
        expect(eq(other, clientKm)).toBe(false);
    });

    it('round-trips a stream reset code through the HTTP/3 error range', async () => {
        // The server resets its send side with application code 42. The code is
        // mapped into the HTTP/3 WT_APPLICATION_ERROR range on the wire and back,
        // so the client must observe exactly 42 (proving the mapping composes to
        // identity end to end, not just that some error surfaced).
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
        servers.push(server);
        await server.ready;
        void (async () => {
            const sreader = server.incomingSessions.getReader();
            const { value: session } = await sreader.read();
            if (!session) return;
            const streams = session.incomingBidirectionalStreams.getReader();
            const { value: stream } = await streams.read();
            if (!stream) return;
            void stream.writable.abort(
                new WebTransportError('server reset', { streamErrorCode: 42 }),
            );
        })();

        const wt = connect(`https://127.0.0.1:${server.port}/reset`);
        await wt.ready;
        const stream = await wt.createBidirectionalStream();
        await stream.writable
            .getWriter()
            .write(enc.encode('hi'))
            .catch(() => undefined);

        await expect(stream.readable.getReader().read()).rejects.toMatchObject({
            streamErrorCode: 42,
        });
    });

    it('echoes a unidirectional stream via a server-opened uni stream', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;

        const send = await wt.createUnidirectionalStream();
        const w = send.getWriter();
        await w.write(enc.encode('one way to server'));
        await w.close();

        const reader = wt.incomingUnidirectionalStreams.getReader();
        const { value: incoming } = await reader.read();
        expect(dec.decode(await readAll(incoming!))).toBe('one way to server');
    });

    it('echoes datagrams', async () => {
        const url = await echoServer();
        const wt = connect(url);
        await wt.ready;

        const writer = wt.datagrams.writable.getWriter();
        const reader = wt.datagrams.readable.getReader();
        const payload = enc.encode('server-datagram');

        const received = await new Promise<Uint8Array>((resolve, reject) => {
            const timer = setTimeout(() => reject(new Error('no datagram echo')), 8000);
            reader.read().then(({ value }) => {
                clearTimeout(timer);
                resolve(value!);
            }, reject);
            void (async () => {
                for (let i = 0; i < 20; i++) {
                    await writer.write(payload);
                    await new Promise((r) => setTimeout(r, 100));
                }
            })().catch(() => undefined);
        });
        expect(dec.decode(received)).toBe('server-datagram');
    });

    it('handles multiple concurrent client sessions', async () => {
        const url = await echoServer();
        const results = await Promise.all(
            ['s1', 's2', 's3'].map(async (msg) => {
                const wt = connect(url);
                await wt.ready;
                const stream = await wt.createBidirectionalStream();
                const w = stream.writable.getWriter();
                await w.write(enc.encode(msg));
                await w.close();
                return dec.decode(await readAll(stream.readable));
            }),
        );
        expect(results).toEqual(['s1', 's2', 's3']);
    });

    // SO_REUSEPORT is Unix-only; on Windows binding the same port twice fails.
    it.skipIf(process.platform === 'win32')(
        'shares one port across servers with reusePort (cluster mode)',
        async () => {
            // First server lets the OS pick a port, with SO_REUSEPORT enabled.
            const a = new WebTransportServer({
                port: 0,
                host: '127.0.0.1',
                cert: CERT,
                key: KEY,
                reusePort: true,
            });
            servers.push(a);
            await a.ready;
            const port = a.port;

            // Second server binds the SAME port. Without SO_REUSEPORT this would
            // reject with EADDRINUSE; with it, both bind and the kernel
            // load-balances connections across them (Node `cluster` model).
            const b = new WebTransportServer({
                port,
                host: '127.0.0.1',
                cert: CERT,
                key: KEY,
                reusePort: true,
            });
            servers.push(b);
            await expect(b.ready).resolves.toBeUndefined();
            expect(b.port).toBe(port);

            for (const s of [a, b]) {
                void (async () => {
                    const reader = s.incomingSessions.getReader();
                    for (;;) {
                        const { value: session, done } = await reader.read();
                        if (done) break;
                        if (session) handleSession(session);
                    }
                })();
            }

            // Every client is echoed, whichever worker the kernel routed it to.
            const url = `https://127.0.0.1:${port}/echo`;
            const results = await Promise.all(
                Array.from({ length: 8 }, async (_unused, i) => {
                    const wt = connect(url);
                    await wt.ready;
                    const stream = await wt.createBidirectionalStream();
                    const w = stream.writable.getWriter();
                    await w.write(enc.encode(`c${i}`));
                    await w.close();
                    return dec.decode(await readAll(stream.readable));
                }),
            );
            expect(results.sort()).toEqual(['c0', 'c1', 'c2', 'c3', 'c4', 'c5', 'c6', 'c7']);
        },
    );
});
