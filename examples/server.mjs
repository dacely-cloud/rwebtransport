// SPDX-License-Identifier: Apache-2.0
//
// Standalone WebTransport echo server (plain ESM).
//
// For each session it opens, it logs the request path, greets the peer with a
// server-opened unidirectional stream, echoes every incoming bidirectional
// stream byte-for-byte, and echoes every datagram back to the sender.
//
// Usage:
//   node examples/server.mjs [port] [cert.pem] [key.pem]
//
// Defaults: port 4433, cert ./cert.pem, key ./key.pem. The cert and key are
// PEM file paths (a certificate chain and its private key).

import { WebTransportServer } from '../dist/index.mjs';

const [portArg, certArg, keyArg] = process.argv.slice(2);
const port = portArg ? Number(portArg) : 4433;
const cert = certArg ?? './cert.pem';
const key = keyArg ?? './key.pem';

if (!Number.isInteger(port) || port < 0 || port > 65535) {
    console.error(`invalid port: ${portArg}`);
    process.exit(1);
}

const encoder = new TextEncoder();

// Greet the peer over a fresh server-opened unidirectional (send) stream.
async function greet(session) {
    const stream = await session.createUnidirectionalStream();
    const writer = stream.getWriter();
    await writer.write(encoder.encode('hello from the rwebtransport echo server\n'));
    await writer.close();
}

// Echo every bidirectional stream the peer opens: pipe its readable straight
// back into its writable. pipeTo also propagates close, abort, and backpressure.
async function echoBidiStreams(session) {
    const reader = session.incomingBidirectionalStreams.getReader();
    for (;;) {
        const { value: stream, done } = await reader.read();
        if (done) break;
        if (stream) void stream.readable.pipeTo(stream.writable).catch(() => {});
    }
}

// Echo every datagram back to the sender. Datagrams are unreliable and
// unordered, so this is best-effort: overflowing datagrams are simply dropped.
async function echoDatagrams(session) {
    const reader = session.datagrams.readable.getReader();
    const writer = session.datagrams.writable.getWriter();
    for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        if (value) await writer.write(value);
    }
}

async function handleSession(session) {
    console.log(`session opened: path=${session.path} authority=${session.authority}`);
    try {
        await greet(session);
    } catch (err) {
        console.error('greeting failed:', err);
    }
    void echoBidiStreams(session).catch(() => {});
    void echoDatagrams(session).catch(() => {});
    session.closed
        .then((info) => {
            console.log(`session closed: path=${session.path} code=${info.closeCode} reason="${info.reason}"`);
        })
        .catch((err) => {
            console.log(`session terminated: path=${session.path} (${err?.message ?? err})`);
        });
}

const server = new WebTransportServer({ port, cert, key });
await server.ready;
console.log(`listening on https://0.0.0.0:${server.port}/ (echo server)`);

// Close cleanly on Ctrl+C.
let closing = false;
process.on('SIGINT', () => {
    if (closing) return;
    closing = true;
    console.log('\nSIGINT: shutting down...');
    server.close();
});

// Accept sessions until the server stops.
const reader = server.incomingSessions.getReader();
for (;;) {
    const { value: session, done } = await reader.read();
    if (done) break;
    if (session) void handleSession(session);
}

await server.closed;
console.log('server stopped');
