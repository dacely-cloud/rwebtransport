#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0
'use strict';

// Builds the rwebtransport native addon with cargo and copies the resulting
// cdylib into prebuilds/<platform>-<arch>/rwebtransport.node so the loader can
// find it. Used both for local development and as the compile step in CI and in
// the install-time fallback.

const { execFileSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const root = path.resolve(__dirname, '..');
const debug = process.argv.includes('--debug');
const profile = debug ? 'debug' : 'release';

const CRATE = 'rwebtransport-native';
const LIB_BASENAME = 'rwebtransport_native';

function cdylibName() {
  switch (process.platform) {
    case 'win32':
      return `${LIB_BASENAME}.dll`;
    case 'darwin':
      return `lib${LIB_BASENAME}.dylib`;
    default:
      return `lib${LIB_BASENAME}.so`;
  }
}

function run(cmd, args) {
  console.error(`> ${cmd} ${args.join(' ')}`);
  execFileSync(cmd, args, { cwd: root, stdio: 'inherit' });
}

function main() {
  const cargoArgs = ['build', '-p', CRATE];
  if (!debug) cargoArgs.push('--release');
  // Allow an explicit target (used by cross-compiling CI jobs).
  const target = process.env.CARGO_BUILD_TARGET;
  if (target) cargoArgs.push('--target', target);

  run('cargo', cargoArgs);

  const targetDir = process.env.CARGO_TARGET_DIR
    ? path.resolve(process.env.CARGO_TARGET_DIR)
    : path.join(root, 'target');
  const artifactDir = target
    ? path.join(targetDir, target, profile)
    : path.join(targetDir, profile);
  const artifact = path.join(artifactDir, cdylibName());

  if (!fs.existsSync(artifact)) {
    throw new Error(`built artifact not found: ${artifact}`);
  }

  const arch = process.env.PREBUILD_ARCH || process.arch;
  const plat = process.env.PREBUILD_PLATFORM || process.platform;
  const outDir = path.join(root, 'prebuilds', `${plat}-${arch}`);
  fs.mkdirSync(outDir, { recursive: true });
  const out = path.join(outDir, 'rwebtransport.node');
  fs.copyFileSync(artifact, out);
  console.error(`copied ${artifact} -> ${out}`);
}

main();
