// SPDX-License-Identifier: Apache-2.0
//
// examples/client.mjs
//
// Standalone runnable WebTransport client example.
//
// Usage:
//   node examples/client.mjs <url> [certHashHex]
//
//   <url>          required, e.g. https://localhost:4433/echo
//   [certHashHex]  optional SHA-256 DER fingerprint of the server certificate
//                  as hex (with or without colons). When given, the cert is
//                  pinned via serverCertificateHashes (browser-style: bypasses
//                  the CA + hostname checks). When omitted, full system PKI +
//                  hostname validation is used.
//
// Pairs with the echo server in examples/server.mjs, which pipes each incoming
// bidirectional stream and datagram straight back to the sender.

import { WebTransport } from '../dist/index.mjs';

const encoder = new TextEncoder();
const decoder = new TextDecoder();

// Decode a hex string (optionally colon-separated, e.g. "a1:b2:c3") into bytes.
function hexToBytes(hex) {
  const clean = hex.replace(/[^0-9a-fA-F]/g, '');
  if (clean.length === 0 || clean.length % 2 !== 0) {
    throw new Error(`invalid hex cert hash: ${JSON.stringify(hex)}`);
  }
  const out = new Uint8Array(clean.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(clean.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

async function main() {
  const [url, certHashHex] = process.argv.slice(2);
  if (!url) {
    console.error('usage: node examples/client.mjs <url> [certHashHex]');
    process.exit(1);
  }

  // Build the client options. If a hex fingerprint was supplied, pin it;
  // otherwise leave options empty so the default PKI + hostname validation runs.
  const options = {};
  if (certHashHex) {
    options.serverCertificateHashes = [
      { algorithm: 'sha-256', value: hexToBytes(certHashHex) },
    ];
    console.log('pinning server certificate via SHA-256 fingerprint');
  } else {
    console.log('using system PKI + hostname validation');
  }

  const transport = new WebTransport(url, options);

  // .closed rejects if the session dies abnormally; surface it so an early
  // failure does not turn into an unhandled rejection.
  transport.closed
    .then((info) =>
      console.log(`session closed: code=${info.closeCode} reason=${JSON.stringify(info.reason)}`),
    )
    .catch((err) => console.error('session closed abnormally:', err.message));

  await transport.ready;
  console.log(`connected to ${url}`);

  // ---- Bidirectional stream: send a message, read the echo ----
  const stream = await transport.createBidirectionalStream();
  console.log(`opened bidirectional stream (streamId=${stream.streamId})`);

  const writer = stream.writable.getWriter();
  const message = 'hello from rwebtransport';
  await writer.write(encoder.encode(message));
  // Close our send side so a pipe-through echo server also ends its send side,
  // which lets the read loop below terminate on done.
  await writer.close();
  console.log(`sent on stream: ${JSON.stringify(message)}`);

  const reader = stream.readable.getReader();
  const chunks = [];
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    chunks.push(value);
  }
  reader.releaseLock();

  let total = 0;
  for (const c of chunks) total += c.length;
  const echoBytes = new Uint8Array(total);
  let offset = 0;
  for (const c of chunks) {
    echoBytes.set(c, offset);
    offset += c.length;
  }
  console.log(`stream echo: ${JSON.stringify(decoder.decode(echoBytes))}`);

  // ---- Datagram: fire one off, wait briefly for any echo ----
  // Datagrams are unreliable and unordered, so an echo may never arrive; race
  // the read against a short timeout instead of blocking forever.
  const dgramWriter = transport.datagrams.writable.getWriter();
  const dgramPayload = 'ping';
  await dgramWriter.write(encoder.encode(dgramPayload));
  dgramWriter.releaseLock();
  console.log(`sent datagram: ${JSON.stringify(dgramPayload)}`);

  const dgramReader = transport.datagrams.readable.getReader();
  const timeout = new Promise((resolve) => setTimeout(() => resolve('timeout'), 1000));
  const result = await Promise.race([dgramReader.read(), timeout]);
  if (result === 'timeout') {
    console.log('no datagram echo received within 1s');
  } else if (result.done) {
    console.log('datagram stream ended without an echo');
  } else {
    console.log(`datagram echo: ${JSON.stringify(decoder.decode(result.value))}`);
  }
  dgramReader.releaseLock();

  // ---- Clean shutdown ----
  transport.close({ closeCode: 0, reason: 'done' });
  console.log('closed transport');
}

main().catch((err) => {
  console.error('client error:', err);
  process.exit(1);
});
