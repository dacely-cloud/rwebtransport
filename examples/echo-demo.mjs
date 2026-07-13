// SPDX-License-Identifier: Apache-2.0
//
// examples/echo-demo.mjs
//
// Self-contained, runnable WebTransport round-trip demo (plain ESM).
//
// One process starts a WebTransportServer that echoes back every
// bidirectional stream and every datagram, then connects a WebTransport
// client to it, exercises both transports, prints what came back, and exits.
//
// The client trusts the server by pinning the certificate's SHA-256 DER
// fingerprint via serverCertificateHashes (browser-style: this bypasses the
// CA + hostname checks without disabling verification entirely), so no real
// PKI is needed. The fingerprint is computed here from examples/cert.pem.
//
// Run it from a checkout after `npm run build`:
//   node examples/echo-demo.mjs
//
// Pairs with the longer-form examples in ./server.mjs and ./client.mjs.

import { X509Certificate, createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { WebTransport, WebTransportServer } from '../dist/index.mjs';

const HERE = dirname(fileURLToPath(import.meta.url));
const CERT = join(HERE, 'cert.pem');
const KEY = join(HERE, 'key.pem');

const encoder = new TextEncoder();
const decoder = new TextDecoder();
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// SHA-256 of the certificate's DER encoding: exactly what serverCertificateHashes wants.
function certHash(certPath) {
  const der = new X509Certificate(readFileSync(certPath)).raw;
  return new Uint8Array(createHash('sha256').update(der).digest());
}

// Echo a single accepted session: pipe every incoming bidi stream and every
// datagram straight back to the sender. pipeTo also carries close, abort, and
// backpressure across, so the client's read side ends cleanly when we do.
function echoSession(session) {
  (async () => {
    const reader = session.incomingBidirectionalStreams.getReader();
    for (;;) {
      const { value: stream, done } = await reader.read();
      if (done) break;
      if (stream) void stream.readable.pipeTo(stream.writable).catch(() => {});
    }
  })().catch(() => {});

  // Datagrams are unreliable and unordered; this echo is best-effort.
  void session.datagrams.readable.pipeTo(session.datagrams.writable).catch(() => {});
}

// Accept sessions in the background until the server stops.
function acceptSessions(server) {
  (async () => {
    const reader = server.incomingSessions.getReader();
    for (;;) {
      const { value: session, done } = await reader.read();
      if (done) break;
      if (session) echoSession(session);
    }
  })().catch(() => {});
}

// Read the reliable, ordered bidi readable to completion and return its bytes.
async function drain(readable) {
  const reader = readable.getReader();
  const chunks = [];
  let total = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(value);
    total += value.length;
  }
  reader.releaseLock();
  const out = new Uint8Array(total);
  let offset = 0;
  for (const chunk of chunks) {
    out.set(chunk, offset);
    offset += chunk.length;
  }
  return out;
}

async function main() {
  // ---- Server: bind on an OS-assigned loopback port and echo everything ----
  const server = new WebTransportServer({ host: '127.0.0.1', port: 0, cert: CERT, key: KEY });
  await server.ready;
  acceptSessions(server);
  const url = `https://127.0.0.1:${server.port}/echo`;
  console.log(`server listening at ${url}`);

  // ---- Client: connect with the pinned certificate fingerprint ----
  const client = new WebTransport(url, {
    serverCertificateHashes: [{ algorithm: 'sha-256', value: certHash(CERT) }],
  });
  await client.ready;
  console.log('client connected (certificate pinned via SHA-256)');

  // ---- Bidirectional stream: send a message, read the echo back ----
  const stream = await client.createBidirectionalStream();
  const writer = stream.writable.getWriter();
  const message = 'hello over a bidirectional stream';
  await writer.write(encoder.encode(message));
  // Close our send side so the pipe-through echo ends and our read side sees done.
  await writer.close();
  console.log(`bidi sent: ${JSON.stringify(message)} (streamId=${stream.writable.streamId})`);
  const echoed = decoder.decode(await drain(stream.readable));
  console.log(`bidi echo: ${JSON.stringify(echoed)} (match=${echoed === message})`);

  // ---- Datagram: fire one off and wait for the echo ----
  // There is a brief window right after connect before the server's datagram
  // echo is wired up, and datagrams may be dropped, so resend until one returns.
  const dgWriter = client.datagrams.writable.getWriter();
  const dgReader = client.datagrams.readable.getReader();
  const dgMessage = 'hello over a datagram';
  const dgPayload = encoder.encode(dgMessage);
  const dgRead = dgReader.read();
  const TIMEOUT = Symbol('timeout');
  let dgResult = TIMEOUT;
  for (let attempt = 0; attempt < 40; attempt++) {
    await dgWriter.write(dgPayload);
    const winner = await Promise.race([dgRead, sleep(25).then(() => TIMEOUT)]);
    if (winner !== TIMEOUT) {
      dgResult = winner;
      break;
    }
  }
  console.log(`datagram sent: ${JSON.stringify(dgMessage)}`);
  if (dgResult === TIMEOUT || dgResult.done) {
    console.log('datagram echo: none received (datagrams are unreliable)');
  } else {
    const dgEcho = decoder.decode(dgResult.value);
    console.log(`datagram echo: ${JSON.stringify(dgEcho)} (match=${dgEcho === dgMessage})`);
  }
  dgReader.releaseLock();
  dgWriter.releaseLock();

  // ---- Clean shutdown of both ends ----
  client.close({ closeCode: 0, reason: 'demo complete' });
  server.close();
  console.log('closed client and server');
  process.exit(0);
}

main().catch((err) => {
  console.error('echo-demo failed:', err);
  process.exit(1);
});
