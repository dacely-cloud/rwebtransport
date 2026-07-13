// SPDX-License-Identifier: Apache-2.0
//! Threading + deadlock-freedom:
//! * the native addon runs correctly inside Node `worker_threads`;
//! * concurrent read+write of a payload larger than every flow-control window
//!   completes without deadlock (the library only ever back-pressures, never
//!   blocks a thread).

import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Worker } from 'node:worker_threads';

import { afterEach, describe, expect, it } from 'vitest';

import { WebTransport } from '../src/index.js';
import { startEchoServer, type EchoServer } from './helpers/echo-server.js';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const WORKER = join(ROOT, 'test', 'helpers', 'wt-worker.cjs');

let servers: EchoServer[] = [];
const clients: WebTransport[] = [];
afterEach(() => {
    for (const c of clients) c.close();
    for (const s of servers) s.stop();
    clients.length = 0;
    servers = [];
});

async function echo(): Promise<EchoServer> {
    const s = await startEchoServer('echo');
    servers.push(s);
    return s;
}

interface WorkerResult {
    ok: boolean;
    echoed?: string;
    error?: string;
}

function runWorker(server: EchoServer, message: string): Promise<WorkerResult> {
    return new Promise<WorkerResult>((resolve, reject) => {
        const worker = new Worker(WORKER, {
            workerData: {
                url: server.url,
                certHash: Array.from(server.certHash),
                message,
            },
        });
        worker.once('message', (m: WorkerResult) => {
            // Wait for the worker (and its native driver thread) to fully exit
            // before resolving, so no worker outlives the test into teardown.
            worker.terminate().then(
                () => resolve(m),
                () => resolve(m),
            );
        });
        worker.once('error', reject);
    });
}

describe('worker_threads', () => {
    it('runs a WebTransport client inside a worker thread', async () => {
        const server = await echo();
        const result = await runWorker(server, 'from-worker');
        expect(result.ok).toBe(true);
        expect(result.echoed).toBe('from-worker');
    });

    it('runs clients in multiple workers concurrently', async () => {
        const server = await echo();
        const results = await Promise.all([
            runWorker(server, 'worker-a'),
            runWorker(server, 'worker-b'),
            runWorker(server, 'worker-c'),
        ]);
        expect(results.map((r) => r.echoed)).toEqual(['worker-a', 'worker-b', 'worker-c']);
    });
});

describe('Deadlock freedom', () => {
    it('echoes a payload larger than all flow-control windows with concurrent read+write', async () => {
        const server = await echo();
        const wt = new WebTransport(server.url, {
            serverCertificateHashes: [{ algorithm: 'sha-256', value: server.certHash }],
        });
        clients.push(wt);
        await wt.ready;

        const SIZE = 4 * 1024 * 1024; // 4 MiB — far exceeds the ~1 MiB stream / 10 MiB conn windows
        const CHUNK = 64 * 1024;
        const byte = (i: number) => (i * 31 + 7) & 0xff;

        const stream = await wt.createBidirectionalStream();
        const writer = stream.writable.getWriter();
        const reader = stream.readable.getReader();

        // Read concurrently with writing. Doing so is what keeps a bidirectional
        // transfer from wedging: if the library blocked a thread instead of
        // back-pressuring, this would deadlock and time out.
        let received = 0;
        let mismatch = -1;
        const readerDone = (async () => {
            for (;;) {
                const { value, done } = await reader.read();
                if (done) break;
                if (value) {
                    for (let i = 0; i < value.byteLength; i++) {
                        if (value[i] !== byte(received + i)) {
                            if (mismatch < 0) mismatch = received + i;
                        }
                    }
                    received += value.byteLength;
                }
            }
        })();

        for (let off = 0; off < SIZE; off += CHUNK) {
            const end = Math.min(off + CHUNK, SIZE);
            const chunk = new Uint8Array(end - off);
            for (let i = 0; i < chunk.length; i++) chunk[i] = byte(off + i);
            await writer.write(chunk);
        }
        await writer.close();
        await readerDone;

        expect(mismatch).toBe(-1);
        expect(received).toBe(SIZE);

        wt.close();
    }, 30_000);
});
