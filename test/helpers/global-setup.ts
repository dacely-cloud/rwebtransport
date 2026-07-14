// SPDX-License-Identifier: Apache-2.0
//! Vitest global setup: mint a fresh short-lived EC P-256 test certificate
//! before the suite runs.
//!
//! The client enforces the WebTransport `serverCertificateHashes` contract,
//! which caps a pinned leaf at 14 days of validity, so a long-lived committed
//! fixture would be rejected. Generating a 13-day cert here (via the Rust
//! `gen-test-cert` binary, so no `openssl` CLI is required on any platform)
//! keeps the fixture valid on every run.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

export default function setup(): void {
    const exe = process.platform === 'win32' ? 'gen-test-cert.exe' : 'gen-test-cert';
    const bin = join(ROOT, 'target', 'release', exe);
    if (!existsSync(bin)) {
        throw new Error(
            `gen-test-cert binary not found at ${bin}; run ` +
                '`cargo build --release -p rwebtransport-echo-server` (npm run build:echo).',
        );
    }
    const fixtures = join(ROOT, 'test', 'fixtures');
    mkdirSync(fixtures, { recursive: true });
    execFileSync(bin, [join(fixtures, 'cert.pem'), join(fixtures, 'key.pem')], {
        stdio: 'inherit',
    });
}
