// SPDX-License-Identifier: Apache-2.0
//! Edge-case behaviours: async DNS failure, the insecure option, opening streams
//! before `ready`, and datagram flood resilience.

import { afterEach, describe, expect, it } from 'vitest';

import { WebTransport, WebTransportError } from '../src/index.js';
import { startEchoServer, type EchoServer } from './helpers/echo-server.js';

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

async function echoServer(): Promise<EchoServer> {
    const s = await startEchoServer('echo');
    servers.push(s);
    return s;
}

describe('Connection setup', () => {
    it('rejects ready (never throws synchronously) for an unresolvable host', async () => {
        // DNS resolution happens on the driver thread, so the constructor must
        // not throw or block; the failure surfaces via ready/closed.
        let wt: WebTransport;
        expect(() => {
            wt = new WebTransport('https://nonexistent.invalid.example:4433/x', {
                insecure: true,
            });
        }).not.toThrow();
        await expect(wt!.ready).rejects.toBeInstanceOf(WebTransportError);
    });

    it('connects with the insecure option (no certificate verification)', async () => {
        const server = await echoServer();
        const wt = new WebTransport(server.url, { insecure: true });
        await expect(wt.ready).resolves.toBeUndefined();
        const stream = await wt.createBidirectionalStream();
        const w = stream.writable.getWriter();
        await w.write(enc.encode('insecure-ok'));
        await w.close();
        expect(dec.decode(await readAll(stream.readable))).toBe('insecure-ok');
        wt.close();
    });

    it('allows opening a stream before ready resolves', async () => {
        const server = await echoServer();
        const wt = new WebTransport(server.url, {
            serverCertificateHashes: [{ algorithm: 'sha-256', value: server.certHash }],
        });
        // Do NOT await ready first — the stream open is queued and completes once
        // the session establishes.
        const streamPromise = wt.createBidirectionalStream();
        const stream = await streamPromise;
        const w = stream.writable.getWriter();
        await w.write(enc.encode('early'));
        await w.close();
        expect(dec.decode(await readAll(stream.readable))).toBe('early');
        wt.close();
    });
});

describe('Datagram flood resilience', () => {
    it('stays responsive while receiving a large datagram burst', async () => {
        const server = await echoServer();
        const wt = new WebTransport(server.url, {
            serverCertificateHashes: [{ algorithm: 'sha-256', value: server.certHash }],
        });
        await wt.ready;

        // Fire a burst of datagrams (each is echoed back); if the client did not
        // bound its event queue this would balloon memory. We just require it to
        // remain responsive and able to do a normal stream exchange afterwards.
        const writer = wt.datagrams.writable.getWriter();
        const payload = new Uint8Array(1000).fill(7);
        for (let i = 0; i < 500; i++) {
            await writer.write(payload);
        }

        const stream = await wt.createBidirectionalStream();
        const w = stream.writable.getWriter();
        await w.write(enc.encode('after-flood'));
        await w.close();
        expect(dec.decode(await readAll(stream.readable))).toBe('after-flood');
        wt.close();
    });
});
