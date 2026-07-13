// SPDX-License-Identifier: Apache-2.0
// Runs a WebTransport client inside a Node worker_thread and echoes one bidi
// stream, to prove the native addon loads and works off the main thread.
'use strict';

const { parentPort, workerData } = require('node:worker_threads');
const { WebTransport } = require('../../dist/index.js');

async function main() {
    const wt = new WebTransport(workerData.url, {
        serverCertificateHashes: [
            { algorithm: 'sha-256', value: new Uint8Array(workerData.certHash) },
        ],
    });
    await wt.ready;

    const stream = await wt.createBidirectionalStream();
    const writer = stream.writable.getWriter();
    await writer.write(new TextEncoder().encode(workerData.message));
    await writer.close();

    const reader = stream.readable.getReader();
    let out = '';
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) out += new TextDecoder().decode(value);
    }
    wt.close();
    return out;
}

main().then(
    (echoed) => parentPort.postMessage({ ok: true, echoed }),
    (err) => parentPort.postMessage({ ok: false, error: String((err && err.message) || err) }),
);
