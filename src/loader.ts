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

/**
 * A CommonJS-style `require` synthesized from this ES module's URL. Native
 * addons (`.node` files) are loaded through `require`, which understands the
 * platform binary format, rather than the ESM `import` machinery.
 *
 * @internal
 */
const require = createRequire(import.meta.url);

/**
 * Absolute path of the directory containing this compiled module, resolved from
 * `import.meta.url`. Used as the anchor for locating package resources.
 *
 * @internal
 */
const moduleDir = dirname(fileURLToPath(import.meta.url));

/**
 * The package root (one level up from the bundled `dist/` directory). All
 * runtime asset lookups (prebuilt binaries, the build script) are resolved
 * relative to this directory.
 *
 * @internal
 */
const PACKAGE_ROOT = join(moduleDir, '..');

/**
 * Guard that aborts loading on an unsupported Node.js runtime. Only Node 24 and
 * Node 26 major versions are supported; anything else means the native ABI may
 * not match.
 *
 * @throws {Error} If the running Node major version is neither 24 nor 26.
 * @internal
 */
function assertSupportedRuntime(): void {
    const major = Number(process.versions.node.split('.')[0]);
    if (major !== 24 && major !== 26) {
        throw new Error(
            `rwebtransport supports Node 24 and Node 26 only; this is Node ${process.versions.node}.`,
        );
    }
}

/**
 * Compute the expected location of the prebuilt native addon for the current
 * platform and architecture, namely
 * `prebuilds/<process.platform>-<process.arch>/rwebtransport.node` under the
 * package root.
 *
 * @returns The absolute path to the platform-specific prebuilt `.node` file.
 *          The file is not guaranteed to exist; callers check separately.
 * @internal
 */
function prebuiltPath(): string {
    return join(
        PACKAGE_ROOT,
        'prebuilds',
        `${process.platform}-${process.arch}`,
        'rwebtransport.node',
    );
}

/**
 * Memoized addon instance. Populated on the first successful {@link loadNative}
 * call and returned directly by all subsequent calls; `undefined` until then.
 *
 * @internal
 */
let cached: NativeAddon | undefined;

/**
 * Locate, load, and cache the rwebtransport native addon.
 *
 * @remarks
 * On the first call the resolution proceeds as follows:
 *   1. Return early if a previous call already cached the addon.
 *   2. Assert the runtime is Node 24 or 26 (see {@link assertSupportedRuntime}).
 *   3. If a prebuilt binary exists for this platform and architecture, `require`
 *      it, cache it, and return it.
 *   4. Otherwise fall back to compiling from source: run `scripts/build.js` with
 *      the current Node executable (working directory set to the package root
 *      and stdio inherited, so the compile output is streamed to the caller's
 *      console), then `require` and cache the freshly produced binary. A notice
 *      is written to `process.stderr` before the (one-time) build begins.
 *
 * Every later call short-circuits on the cached instance without re-checking the
 * runtime or the filesystem.
 *
 * @returns The loaded {@link NativeAddon}: the raw function surface exported by
 *          `rwebtransport.node`.
 * @throws {Error} If the runtime is an unsupported Node.js version.
 * @throws {Error} If no prebuilt binary exists and `scripts/build.js` is absent,
 *                 so no binary can be compiled.
 * @throws {Error} If the on-the-fly build runs but does not produce the expected
 *                 binary. `execFileSync` additionally propagates if the build
 *                 process itself exits non-zero, and `require` propagates if the
 *                 resulting binary cannot be loaded.
 * @see {@link NativeAddon} for the shape of the returned object.
 * @internal
 */
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
