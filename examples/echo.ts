// SPDX-License-Identifier: Apache-2.0
//
// examples/echo.ts
//
// Self-contained, runnable WebTransport round-trip in TypeScript.
//
// One process starts a WebTransportServer that echoes everything back, connects
// a WebTransport client to it, and exercises BOTH transports WebTransport gives
// you:
//
//   * a reliable, ordered bidirectional stream (guaranteed delivery, in order),
//   * an unreliable, unordered datagram (best-effort, may be dropped).
//
// The client trusts the server by pinning the certificate's SHA-256 fingerprint
// (browser-style, so no real PKI is needed).
//
// Run it from a checkout after `npm run build` (Node 24+ runs TypeScript directly):
//   node examples/echo.ts

import { X509Certificate, createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

// In your own project this import is `from 'rwebtransport'`. This in-repo copy
// points at the built output so it runs straight from a checkout.
import {
    WebTransport,
    WebTransportServer,
    type WebTransportServerSession,
} from '../dist/index.js';

const HERE = dirname(fileURLToPath(import.meta.url));
const CERT = join(HERE, 'cert.pem');
const KEY = join(HERE, 'key.pem');

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const sleep = (ms: number): Promise<void> => new Promise((r) => setTimeout(r, ms));

// SHA-256 of the certificate's DER encoding: exactly what serverCertificateHashes wants.
function certHash(path: string): Uint8Array {
    const der = new X509Certificate(readFileSync(path)).raw;
    return new Uint8Array(createHash('sha256').update(der).digest());
}

// Echo an accepted session: pipe every reliable stream and every datagram back.
function echo(session: WebTransportServerSession): void {
    void (async () => {
        const reader = session.incomingBidirectionalStreams.getReader();
        for (;;) {
            const { value: stream, done } = await reader.read();
            if (done) break;
            if (stream) void stream.readable.pipeTo(stream.writable).catch(() => {});
        }
    })();
    void session.datagrams.readable.pipeTo(session.datagrams.writable).catch(() => {});
}

// Read a reliable readable stream to end-of-stream and return all its bytes.
async function readAll(readable: ReadableStream<Uint8Array>): Promise<Uint8Array> {
    const reader = readable.getReader();
    const chunks: Uint8Array[] = [];
    let total = 0;
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) {
            chunks.push(value);
            total += value.length;
        }
    }
    const out = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
        out.set(chunk, offset);
        offset += chunk.length;
    }
    return out;
}

async function main(): Promise<void> {
    // ---- Server: bind an OS-assigned loopback port and echo everything ----
    const server = new WebTransportServer({ host: '127.0.0.1', port: 0, cert: CERT, key: KEY });
    await server.ready;
    void (async () => {
        const reader = server.incomingSessions.getReader();
        for (;;) {
            const { value: session, done } = await reader.read();
            if (done) break;
            if (session) echo(session);
        }
    })();

    const url = `https://127.0.0.1:${server.port}/echo`;
    console.log(`server listening at ${url}`);

    // ---- Client: connect, trusting the server's pinned certificate ----
    const client = new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash(CERT) }],
    });
    await client.ready;
    console.log('client connected');

    // ---- RELIABLE: a bidirectional stream. Delivery is guaranteed and ordered. ----
    const stream = await client.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    await writer.write(encoder.encode('hello over a reliable stream'));
    await writer.close(); // ends our send side so the echo's read side sees `done`
    const reply = decoder.decode(await readAll(stream.readable));
    console.log(`reliable stream echo: ${JSON.stringify(reply)}`);

    // ---- UNRELIABLE: a datagram. Best-effort: it may be dropped, so we retry. ----
    const dgWriter = client.datagrams.writable.getWriter();
    const dgReader = client.datagrams.readable.getReader();
    const payload = encoder.encode('hello over an unreliable datagram');
    const inbound = dgReader.read().then((r) => r.value);
    let echoed: Uint8Array | undefined;
    for (let attempt = 0; attempt < 40 && !echoed; attempt++) {
        await dgWriter.write(payload);
        echoed = await Promise.race([inbound, sleep(25).then(() => undefined)]);
    }
    if (echoed) {
        console.log(`unreliable datagram echo: ${JSON.stringify(decoder.decode(echoed))}`);
    } else {
        console.log('unreliable datagram echo: none (datagrams are lossy by design)');
    }

    // ---- Clean shutdown of both ends ----
    client.close({ closeCode: 0, reason: 'done' });
    server.close();
    process.exit(0);
}

main().catch((err: unknown) => {
    console.error('echo failed:', err);
    process.exit(1);
});
