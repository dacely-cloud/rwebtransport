#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
'use strict';

// Ensures a usable native addon after install. If a prebuilt binary already
// exists for this platform, we do nothing. Otherwise, if a Rust toolchain is
// available, we compile it now. Failures here are non-fatal: the loader will
// retry the build lazily on first use and surface a clear error if it can't.

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');

function log(msg) {
    process.stderr.write(`rwebtransport: ${msg}\n`);
}

function hasPrebuilt() {
    const p = path.join(root, 'prebuilds', `${process.platform}-${process.arch}`, 'rwebtransport.node');
    return fs.existsSync(p);
}

function hasCargo() {
    try {
        execFileSync('cargo', ['--version'], { stdio: 'ignore' });
        return true;
    } catch {
        return false;
    }
}

function main() {
    // Skip during the package's own repo install / CI (source-tree development).
    if (process.env.RWEBTRANSPORT_SKIP_POSTINSTALL === '1') return;

    const major = Number(process.versions.node.split('.')[0]);
    if (major !== 24 && major !== 26) {
        log(`warning: Node ${process.versions.node} is unsupported (need 24 or 26).`);
        return;
    }

    if (hasPrebuilt()) return;

    if (!hasCargo()) {
        log('no prebuilt binary for this platform and no Rust toolchain found;');
        log('install Rust (https://rustup.rs) + cmake, or use a supported prebuilt platform.');
        return;
    }

    log('no prebuilt binary for this platform; compiling the native addon...');
    try {
        execFileSync(process.execPath, [path.join(root, 'scripts', 'build.js')], {
            cwd: root,
            stdio: 'inherit',
        });
    } catch {
        log('build failed during postinstall; will retry on first use.');
    }
}

try {
    main();
} catch (e) {
    log(`postinstall skipped: ${e && e.message ? e.message : e}`);
}
