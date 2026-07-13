// SPDX-License-Identifier: Apache-2.0
//! Test helper: spawn the Rust WebTransport echo server and derive its cert hash.

import { spawn } from 'node:child_process';
import { X509Certificate, createHash } from 'node:crypto';
import { existsSync, readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const CERT = join(ROOT, 'test', 'fixtures', 'cert.pem');
const KEY = join(ROOT, 'test', 'fixtures', 'key.pem');

function binPath(): string {
    const exe = process.platform === 'win32' ? 'wt-echo-server.exe' : 'wt-echo-server';
    return join(ROOT, 'target', 'release', exe);
}

/** SHA-256 of the fixture certificate's DER encoding (what serverCertificateHashes expects). */
export function certHash(): Uint8Array {
    const der = new X509Certificate(readFileSync(CERT)).raw;
    return new Uint8Array(createHash('sha256').update(der).digest());
}

/** Adversarial server behaviours (see crates/echo-server --mode). */
export type EchoServerMode =
    'echo' | 'reject' | 'malformed-headers' | 'garbage' | 'reset' | 'close';

export interface EchoServer {
    url: string;
    certHash: Uint8Array;
    port: number;
    stop(): void;
}

/** Spawn the echo server on an OS-assigned port and resolve once it is ready. */
export async function startEchoServer(mode: EchoServerMode = 'echo'): Promise<EchoServer> {
    const bin = binPath();
    if (!existsSync(bin)) {
        throw new Error(
            `echo server binary not found at ${bin}; run \`cargo build --release -p rwebtransport-echo-server\`.`,
        );
    }

    const proc = spawn(
        bin,
        ['--cert', CERT, '--key', KEY, '--host', '127.0.0.1', '--port', '0', '--mode', mode],
        { stdio: ['ignore', 'pipe', 'pipe'] },
    );

    const port = await new Promise<number>((resolve, reject) => {
        let buf = '';
        const timer = setTimeout(
            () => reject(new Error('echo server did not report a port')),
            10_000,
        );
        proc.stdout.on('data', (d: Buffer) => {
            buf += d.toString();
            const m = buf.match(/PORT (\d+)/);
            if (m) {
                clearTimeout(timer);
                resolve(Number(m[1]));
            }
        });
        proc.on('error', (e) => {
            clearTimeout(timer);
            reject(e);
        });
        proc.on('exit', (code) => {
            clearTimeout(timer);
            reject(new Error(`echo server exited early with code ${code}`));
        });
    });

    return {
        url: `https://127.0.0.1:${port}/echo`,
        certHash: certHash(),
        port,
        stop() {
            proc.kill('SIGKILL');
        },
    };
}
