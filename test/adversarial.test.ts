// SPDX-License-Identifier: Apache-2.0
//! Adversarial tests: a hostile / misbehaving server must never crash or hang
//! the client — it must fail cleanly (reject ready, error a stream, or resolve
//! closed) and the Node process must survive.

import { afterEach, describe, expect, it } from 'vitest';

import { WebTransport, WebTransportError } from '../src/index.js';
import { startEchoServer, type EchoServer, type EchoServerMode } from './helpers/echo-server.js';

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

let servers: EchoServer[] = [];

afterEach(() => {
    for (const s of servers) s.stop();
    servers = [];
});

async function open(mode: EchoServerMode): Promise<WebTransport> {
    const server = await startEchoServer(mode);
    servers.push(server);
    return new WebTransport(server.url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: server.certHash }],
    });
}

describe('Hostile server', () => {
    it('rejects ready when the server refuses the CONNECT (non-2xx)', async () => {
        const wt = await open('reject');
        await expect(wt.ready).rejects.toBeInstanceOf(WebTransportError);
    });

    it('rejects ready on a malformed QPACK response and does not crash', async () => {
        const wt = await open('malformed-headers');
        await expect(wt.ready).rejects.toBeInstanceOf(WebTransportError);
        // Prove the process is still healthy afterwards.
        const wt2 = await open('echo');
        await expect(wt2.ready).resolves.toBeUndefined();
        wt2.close();
    });

    it('survives garbage bytes dumped on the session stream', async () => {
        const wt = await open('garbage');
        await expect(wt.ready).resolves.toBeUndefined();
        // Let the garbage arrive; the session must NOT die on its own.
        await new Promise((r) => setTimeout(r, 300));
        let closedEarly = false;
        void wt.closed.then(
            () => {
                closedEarly = true;
            },
            () => {
                closedEarly = true;
            },
        );
        await new Promise((r) => setTimeout(r, 100));
        expect(closedEarly).toBe(false);
        // And a clean close still works.
        wt.close();
        await wt.closed;
    });

    it('surfaces a server stream reset as a stream error', async () => {
        const wt = await open('reset');
        await wt.ready;
        const stream = await wt.createBidirectionalStream();
        const writer = stream.writable.getWriter();
        await writer.write(enc.encode('will be reset'));
        await writer.close().catch(() => undefined);
        await expect(readAll(stream.readable)).rejects.toBeInstanceOf(WebTransportError);
        wt.close();
    });

    it('resolves closed when the server ends the session', async () => {
        const wt = await open('close');
        await wt.ready;
        const info = await wt.closed;
        expect(info).toBeTypeOf('object');
        expect(info.closeCode).toBeTypeOf('number');
    });
});

describe('Hostile / careless caller', () => {
    it('rejects operations after the session is closed', async () => {
        const wt = await open('echo');
        await wt.ready;
        wt.close();
        await wt.closed.catch(() => undefined);
        // Opening a stream on a closed session must reject, not hang or crash.
        await expect(wt.createBidirectionalStream()).rejects.toBeInstanceOf(Error);
    });

    it('drops an oversized datagram without crashing', async () => {
        const wt = await open('echo');
        await wt.ready;
        const huge = new Uint8Array(100_000); // far larger than any QUIC datagram
        const writer = wt.datagrams.writable.getWriter();
        await writer.write(huge); // resolves (dropped), does not throw/crash
        const stream = await wt.createBidirectionalStream();
        const w = stream.writable.getWriter();
        await w.write(enc.encode('still working'));
        await w.close();
        expect(dec.decode(await readAll(stream.readable))).toBe('still working');
        wt.close();
    });

    it('handles many sequential sessions without leaking (process stays responsive)', async () => {
        for (let i = 0; i < 5; i++) {
            const wt = await open('echo');
            await wt.ready;
            const stream = await wt.createBidirectionalStream();
            const w = stream.writable.getWriter();
            await w.write(enc.encode(`session-${i}`));
            await w.close();
            expect(dec.decode(await readAll(stream.readable))).toBe(`session-${i}`);
            wt.close();
        }
    });
});
