// SPDX-License-Identifier: Apache-2.0
// Loopback micro-benchmark: our client <-> our server (both rwebtransport).
// Measures handshake rate, bidi stream throughput, request/response RTT, and
// datagram rate. Numbers reflect library + crypto + loopback overhead (no real
// network), so they characterise CPU cost, not internet performance.

import { X509Certificate, createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { fileURLToPath } from 'node:url';

import { WebTransport, WebTransportServer } from '../dist/index.mjs';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const CERT = join(ROOT, 'test', 'fixtures', 'cert.pem');
const KEY = join(ROOT, 'test', 'fixtures', 'key.pem');
const certHash = new Uint8Array(
    createHash('sha256').update(new X509Certificate(readFileSync(CERT)).raw).digest(),
);

// Watchdog: never let the benchmark hang the box.
const watchdog = setTimeout(() => {
    console.error('bench watchdog: timed out, exiting');
    process.exit(1);
}, 120_000);
watchdog.unref();

const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert: CERT, key: KEY });
await server.ready;
const url = `https://127.0.0.1:${server.port}/bench`;

// Server: echo every bidi stream and every datagram.
void (async () => {
    const reader = server.incomingSessions.getReader();
    for (;;) {
        const { value: session, done } = await reader.read();
        if (done) break;
        void (async () => {
            const r = session.incomingBidirectionalStreams.getReader();
            for (;;) {
                const { value: s, done } = await r.read();
                if (done) break;
                if (s) void s.readable.pipeTo(s.writable).catch(() => {});
            }
        })();
        void (async () => {
            const r = session.datagrams.readable.getReader();
            const w = session.datagrams.writable.getWriter();
            for (;;) {
                const { value, done } = await r.read();
                if (done) break;
                if (value) await w.write(value);
            }
        })();
    }
})();

function connect() {
    return new WebTransport(url, {
        serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash }],
    });
}

const fmt = (n, d = 1) => n.toLocaleString('en-US', { maximumFractionDigits: d });

// --- Handshake rate -------------------------------------------------------
// Close each session right after it establishes so we do not accumulate live
// connections (that is a load test, not a handshake-latency measurement).
async function benchHandshake(n) {
    const t0 = performance.now();
    for (let i = 0; i < n; i++) {
        const wt = connect();
        await wt.ready;
        wt.close();
        await wt.closed.catch(() => {});
    }
    const ms = performance.now() - t0;
    return { rate: n / (ms / 1000), each: ms / n };
}

// --- Bidi throughput (round-trip echo) ------------------------------------
async function benchThroughput(totalBytes, chunkSize) {
    const wt = connect();
    await wt.ready;
    const stream = await wt.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    const reader = stream.readable.getReader();
    const chunk = new Uint8Array(chunkSize);
    let received = 0;

    const t0 = performance.now();
    const readP = (async () => {
        for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            if (value) received += value.byteLength;
        }
    })();
    for (let off = 0; off < totalBytes; off += chunkSize) await writer.write(chunk);
    await writer.close();
    await readP;
    const secs = (performance.now() - t0) / 1000;
    wt.close();
    return {
        received,
        oneWayMBs: totalBytes / 1e6 / secs,
        biMBs: (2 * totalBytes) / 1e6 / secs,
    };
}

// --- RTT (sequential request/response on one stream) ----------------------
async function benchRtt(n, size) {
    const wt = connect();
    await wt.ready;
    const stream = await wt.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    const reader = stream.readable.getReader();
    const msg = new Uint8Array(size);
    const t0 = performance.now();
    for (let i = 0; i < n; i++) {
        await writer.write(msg);
        let got = 0;
        while (got < size) {
            const { value, done } = await reader.read();
            if (done) break;
            if (value) got += value.byteLength;
        }
    }
    const ms = performance.now() - t0;
    wt.close();
    return { rttMs: ms / n, rate: n / (ms / 1000) };
}

// --- Datagram rate --------------------------------------------------------
async function benchDatagrams(n, size) {
    const wt = connect();
    await wt.ready;
    const writer = wt.datagrams.writable.getWriter();
    const reader = wt.datagrams.readable.getReader();
    const dg = new Uint8Array(size);
    let echoed = 0;
    const readP = (async () => {
        for (;;) {
            const { value, done } = await reader.read();
            if (done) break;
            if (value) echoed++;
        }
    })();
    const t0 = performance.now();
    for (let i = 0; i < n; i++) await writer.write(dg);
    const sendMs = performance.now() - t0;
    await new Promise((r) => setTimeout(r, 400)); // let echoes drain
    wt.close();
    await readP.catch(() => {});
    return { sendRate: n / (sendMs / 1000), echoed, sent: n };
}

console.log(`\nrwebtransport benchmark: node ${process.version}, ${process.platform}/${process.arch}`);
console.log('peer: our WebTransportServer over loopback (127.0.0.1)\n');

const hs = await benchHandshake(100);
console.log(`handshake       ${fmt(hs.rate, 0)} sessions/s   (${fmt(hs.each, 2)} ms each)`);

const tp = await benchThroughput(64 * 1024 * 1024, 256 * 1024);
console.log(
    `bidi throughput ${fmt(tp.oneWayMBs, 0)} MB/s one-way   (${fmt(tp.biMBs, 0)} MB/s bidirectional)`,
);

const rtt = await benchRtt(2000, 16);
console.log(`rtt (16 B)      ${fmt(rtt.rttMs, 3)} ms          (${fmt(rtt.rate, 0)} req/s)`);

const dg = await benchDatagrams(20000, 1200);
console.log(
    `datagrams       ${fmt(dg.sendRate, 0)} sent/s      (${fmt(dg.echoed, 0)}/${fmt(dg.sent, 0)} echoed back)`,
);

console.log('');
server.close();
process.exit(0);
