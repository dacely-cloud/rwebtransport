// SPDX-License-Identifier: Apache-2.0
//! The WebTransport `serverCertificateHashes` contract: a pinned leaf must be
//! currently valid and span at most 14 days. The suite's fixture is minted fresh
//! each run by the vitest global setup; these tests lock in that it stays within
//! the window and that the client rejects a pinned cert whose validity is too
//! long.

import { execFileSync } from 'node:child_process';
import { X509Certificate, createHash } from 'node:crypto';
import { mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { afterEach, describe, expect, it } from 'vitest';

import { WebTransport, WebTransportServer } from '../src/index.js';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const CERT = join(ROOT, 'test', 'fixtures', 'cert.pem');
const FOURTEEN_DAYS_MS = 14 * 24 * 60 * 60 * 1000;

function genCert(
    days: number,
    keyType: 'ec' | 'rsa' = 'ec',
): { cert: string; key: string; hash: Uint8Array } {
    const exe = process.platform === 'win32' ? 'gen-test-cert.exe' : 'gen-test-cert';
    const bin = join(ROOT, 'target', 'release', exe);
    const dir = mkdtempSync(join(tmpdir(), 'rwt-cert-'));
    const cert = join(dir, 'cert.pem');
    const key = join(dir, 'key.pem');
    execFileSync(bin, [cert, key, String(days), keyType]);
    const der = new X509Certificate(readFileSync(cert)).raw;
    return { cert, key, hash: new Uint8Array(createHash('sha256').update(der).digest()) };
}

const servers: WebTransportServer[] = [];
const clients: WebTransport[] = [];
afterEach(() => {
    for (const c of clients) c.close();
    for (const s of servers) s.close();
    clients.length = 0;
    servers.length = 0;
});

describe('serverCertificateHashes certificate validity', () => {
    it('fixture is currently valid and within the 14-day window', () => {
        const cert = new X509Certificate(readFileSync(CERT));
        const from = new Date(cert.validFrom).getTime();
        const to = new Date(cert.validTo).getTime();
        const now = Date.now();
        expect(from).toBeLessThanOrEqual(now);
        expect(to).toBeGreaterThan(now);
        expect(to - from).toBeLessThanOrEqual(FOURTEEN_DAYS_MS);
    });

    it('rejects a pinned cert whose validity exceeds 14 days', async () => {
        // A 400-day cert: the fingerprint still matches, but the client must
        // reject it because it violates the 14-day pinning ceiling.
        const { cert, key, hash } = genCert(400);
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert, key });
        servers.push(server);
        await server.ready;

        const wt = new WebTransport(`https://127.0.0.1:${server.port}/`, {
            serverCertificateHashes: [{ algorithm: 'sha-256', value: hash }],
        });
        clients.push(wt);
        await expect(wt.ready).rejects.toBeDefined();
    });

    it('rejects a pinned cert that is not ECDSA P-256', async () => {
        // A valid, in-window cert but with an RSA key: the fingerprint matches,
        // yet the client must reject it because serverCertificateHashes requires
        // an ECDSA P-256 key.
        const { cert, key, hash } = genCert(13, 'rsa');
        const server = new WebTransportServer({ port: 0, host: '127.0.0.1', cert, key });
        servers.push(server);
        await server.ready;

        const wt = new WebTransport(`https://127.0.0.1:${server.port}/`, {
            serverCertificateHashes: [{ algorithm: 'sha-256', value: hash }],
        });
        clients.push(wt);
        await expect(wt.ready).rejects.toBeDefined();
    });
});
