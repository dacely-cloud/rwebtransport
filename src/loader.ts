// SPDX-License-Identifier: Apache-2.0
//! Locates and loads the rwebtransport native addon.
//!
//! Resolution order:
//!   1. A prebuilt binary at `prebuilds/<platform>-<arch>/rwebtransport.node`.
//!   2. If none is found, compile the Rust crate on the fly via `scripts/build.js`
//!      (requires a Rust toolchain + cmake + C/C++ compiler) and load the result.
//!
//! Only Node 24 and Node 26 are supported.

import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import type { NativeAddon } from './native.js';

const require = createRequire(import.meta.url);
const moduleDir = dirname(fileURLToPath(import.meta.url));

/** The package root (one level up from the bundled `dist/` directory). */
const PACKAGE_ROOT = join(moduleDir, '..');

function assertSupportedRuntime(): void {
    const major = Number(process.versions.node.split('.')[0]);
    if (major !== 24 && major !== 26) {
        throw new Error(
            `rwebtransport supports Node 24 and Node 26 only; this is Node ${process.versions.node}.`,
        );
    }
}

function prebuiltPath(): string {
    return join(
        PACKAGE_ROOT,
        'prebuilds',
        `${process.platform}-${process.arch}`,
        'rwebtransport.node',
    );
}

let cached: NativeAddon | undefined;

export function loadNative(): NativeAddon {
    if (cached) return cached;
    assertSupportedRuntime();

    const prebuilt = prebuiltPath();
    if (existsSync(prebuilt)) {
        cached = require(prebuilt) as NativeAddon;
        return cached;
    }

    // Fallback: compile from source.
    const builder = join(PACKAGE_ROOT, 'scripts', 'build.js');
    if (!existsSync(builder)) {
        throw new Error(
            `rwebtransport: no prebuilt binary at ${prebuilt} and no build script to compile one. ` +
                `Reinstall the package or build it manually (npm run build:rust).`,
        );
    }
    process.stderr.write(
        'rwebtransport: no prebuilt binary for this platform; compiling the native addon (this happens once)...\n',
    );
    execFileSync(process.execPath, [builder], { cwd: PACKAGE_ROOT, stdio: 'inherit' });

    if (!existsSync(prebuilt)) {
        throw new Error(`rwebtransport: build completed but ${prebuilt} was not produced.`);
    }
    cached = require(prebuilt) as NativeAddon;
    return cached;
}
