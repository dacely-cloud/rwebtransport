# Building rwebtransport

`rwebtransport` is a native addon: a Rust core (Cloudflare **quiche** + **BoringSSL**, bound to Node through **neon**) plus a thin TypeScript layer. Most users never build anything, because a prebuilt binary ships for the common platforms. This page explains exactly how the binary is selected, when and how the Rust core is compiled instead, what the build scripts do, where the compiled addon lands, and how the release pipeline produces the per-platform prebuilds.

For the top-level overview see [`../README.md`](../README.md). The loader that ties this together is [`../src/loader.ts`](../src/loader.ts).

---

## Supported platforms and Node versions

**Node.js 24.x or 26.x only.** No other major version is supported. The loader ([`../src/loader.ts`](../src/loader.ts)) throws on any other version, and `package.json` pins `engines.node` to `>=24 <25 || >=26 <27`. The native ABI is built and tested against exactly these two lines.

Prebuilt binaries are published for these targets:

| Target (`process.platform`-`process.arch`) | OS      | Arch  | Prebuilt? |
| ------------------------------------------ | ------- | ----- | --------- |
| `linux-x64`                                | Linux   | x64   | yes       |
| `linux-arm64`                              | Linux   | arm64 | yes       |
| `darwin-x64`                               | macOS   | x64   | yes       |
| `darwin-arm64`                             | macOS   | arm64 | yes       |
| `win32-x64`                                | Windows | x64   | yes       |

Any other combination (for example `win32-arm64`) has no prebuilt binary and is compiled from source at install time, which requires the toolchain described below. The directory name is always `${process.platform}-${process.arch}`, so it matches the values Node reports at runtime.

> One binary serves both Node lines. The addon targets **N-API version 8** (`neon` feature `napi-8` in [`../crates/native/Cargo.toml`](../crates/native/Cargo.toml)), a stable ABI, so a prebuild compiled on Node 26 loads unchanged on Node 24.

---

## How the loader picks a binary

`loadNative()` in [`../src/loader.ts`](../src/loader.ts) resolves the addon the first time you construct a `WebTransport` or `WebTransportServer`, then caches it. The order is:

1. **Assert the runtime.** If `process.versions.node` is not major 24 or 26, it throws immediately.
2. **Prefer a prebuilt binary.** It looks for `prebuilds/<platform>-<arch>/rwebtransport.node` under the package root. If that file exists, it is `require()`d and returned.
3. **Compile on the fly.** If no prebuilt binary is present, it runs [`../scripts/build.js`](../scripts/build.js) with the current Node executable (`execFileSync`, output streamed to your terminal), then loads the addon that the build produced. This needs a Rust toolchain plus the BoringSSL build prerequisites (see below). If the build script is missing, or if it finishes without producing the expected file, the loader throws with a clear message telling you to reinstall or run `npm run build:rust`.

So a fresh install on a supported platform loads a prebuilt binary with zero compilation, while an unsupported platform transparently compiles once and caches the result on disk for every later run.

### Install-time compile

`npm install` also runs [`../scripts/postinstall.js`](../scripts/postinstall.js), which front-loads the same work so the first import is fast:

- If `RWEBTRANSPORT_SKIP_POSTINSTALL=1` is set, it does nothing. This is used when developing the package itself and in CI, where the addon is built explicitly.
- If the Node major version is not 24 or 26, it prints a warning and stops.
- If a prebuilt binary already exists for this platform, it stops (nothing to do).
- If there is no `cargo` on `PATH`, it prints guidance (install Rust from https://rustup.rs plus cmake) and stops.
- Otherwise it compiles by invoking [`../scripts/build.js`](../scripts/build.js).

Every failure here is non-fatal: postinstall never breaks your `npm install`. If the compile does not happen at install time, the loader retries it lazily on first use as described above.

---

## Compiling from source

### Prerequisites

The Rust core links quiche against **BoringSSL**, which is built from source by the `boring-sys` crate during `cargo build`. You need:

- **Rust** (stable), the easiest way is [rustup](https://rustup.rs).
- **cmake**, to configure the BoringSSL build.
- A **C/C++ compiler** (clang or gcc on Linux/macOS, MSVC on Windows).
- **Go**, used by BoringSSL's build for code generation. CI installs Go 1.22.
- **NASM** on Windows only, for BoringSSL's assembly.
- **Ninja** is recommended as the cmake generator. CI installs it on Linux and macOS; cmake can fall back to another generator if it is absent.

For reference, the toolchain each CI runner installs is spelled out in [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml) and [`../.github/workflows/release.yml`](../.github/workflows/release.yml).

### Build it

```bash
git clone https://github.com/dacely-cloud/rwebtransport
cd rwebtransport
npm install
npm run build          # cargo build of the native addon, copy the .node, then bundle the TS layer
```

The Rust workspace ([`../Cargo.toml`](../Cargo.toml)) has three crates under `crates/`: `rwebtransport-native` (the neon addon, a `cdylib`), `rwebtransport-wtcore` (the HTTP/3 + WebTransport state machine), and `rwebtransport-echo-server` (a test fixture). The QUIC engine is vendored under `vendor/quiche` and is excluded from the workspace so it builds as a plain path dependency with the `boringssl-boring-crate` feature.

---

## Build scripts

All scripts are defined in [`../package.json`](../package.json):

| Command                    | What it does                                                                               |
| -------------------------- | ------------------------------------------------------------------------------------------ |
| `npm run build`            | `build:rust` then `build:ts` (the full build).                                             |
| `npm run build:rust`       | Release build of the native addon via [`../scripts/build.js`](../scripts/build.js).        |
| `npm run build:rust:debug` | Faster debug build (`node scripts/build.js --debug`), no optimization or LTO.              |
| `npm run build:ts`         | Bundles the TypeScript layer to `dist/` (tsup for CJS + ESM, then `tsc` for declarations). |

### What `scripts/build.js` does

[`../scripts/build.js`](../scripts/build.js) is the single compile step used by local development, by CI, and by the install-time fallback. It:

1. Runs `cargo build -p rwebtransport-native` (adding `--release` unless `--debug` was passed, and `--target <triple>` if `CARGO_BUILD_TARGET` is set).
2. Locates the resulting shared library in the cargo target directory. The `cdylib` filename is platform-specific:
    - Linux: `librwebtransport_native.so`
    - macOS: `librwebtransport_native.dylib`
    - Windows: `rwebtransport_native.dll`
3. Copies that library to `prebuilds/<platform>-<arch>/rwebtransport.node`, creating the directory if needed. Renaming the `cdylib` to `.node` is what makes it loadable with `require()`.

If the expected artifact is missing after the cargo build, it throws so the failure is obvious.

The release profile in [`../Cargo.toml`](../Cargo.toml) is tuned for a shippable binary: `opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, and `strip = true`. Panic strategy stays `unwind` (the default) on purpose, so neon can catch Rust panics at the N-API boundary and turn them into JS exceptions instead of aborting the Node process.

### Environment variables

`scripts/build.js` reads a few variables, mainly for cross-compiling in CI:

| Variable                         | Effect                                                                                |
| -------------------------------- | ------------------------------------------------------------------------------------- |
| `CARGO_BUILD_TARGET`             | Passes `--target <triple>` to cargo and looks for the artifact under that target.     |
| `CARGO_TARGET_DIR`               | Overrides where cargo writes build output (default: `./target`).                      |
| `PREBUILD_PLATFORM`              | Overrides the platform segment of the output directory (default: `process.platform`). |
| `PREBUILD_ARCH`                  | Overrides the arch segment of the output directory (default: `process.arch`).         |
| `RWEBTRANSPORT_SKIP_POSTINSTALL` | When `1`, [`../scripts/postinstall.js`](../scripts/postinstall.js) does nothing.      |

### Where the addon lands

Always here, relative to the package root:

```
prebuilds/<platform>-<arch>/rwebtransport.node
```

for example `prebuilds/linux-x64/rwebtransport.node` or `prebuilds/darwin-arm64/rwebtransport.node`. This is exactly where the loader looks. The `prebuilds/` tree is git-ignored (it is a build output), but it is listed in `package.json`'s `files`, so it is included in the published npm tarball along with `scripts/`, `crates/`, `vendor/`, `Cargo.toml`, and `Cargo.lock`. That is why the published package can both ship prebuilts and still compile from source when no prebuilt matches.

### Verifying a local build

After `npm run build`, a quick import confirms the addon loads (plain ESM):

```js
// check.mjs
import { WebTransport, WebTransportServer } from 'rwebtransport';

// Constructing either type triggers the loader, which resolves the .node addon.
// A bogus URL fails at connect time, not at load time, so reaching `.ready`
// (even to reject) proves the native binary loaded.
const wt = new WebTransport('https://127.0.0.1:1/does-not-exist');
try {
    await wt.ready;
} catch {
    // expected: nothing is listening. The point is that the addon loaded.
}
console.log('native addon loaded');
```

```bash
node check.mjs
```

---

## The release pipeline

Prebuilt binaries and the npm publish are produced by [`../.github/workflows/release.yml`](../.github/workflows/release.yml), which runs on a pushed `v*` tag (or manual `workflow_dispatch`). It has two jobs.

### 1. `prebuild` (one job per target)

A build matrix compiles the native addon on a native runner for each shipped target, so no cross-compilation is needed:

| Target         | Runner             |
| -------------- | ------------------ |
| `linux-x64`    | `ubuntu-24.04`     |
| `linux-arm64`  | `ubuntu-24.04-arm` |
| `darwin-x64`   | `macos-13`         |
| `darwin-arm64` | `macos-14`         |
| `win32-x64`    | `windows-2022`     |

Each job installs the stable Rust toolchain, Go 1.22, NASM (Windows only), and cmake + ninja (via apt on Linux, brew on macOS), then sets up Node 26 and runs `node scripts/build.js`. The resulting `prebuilds/<target>/rwebtransport.node` is uploaded as an artifact named `prebuild-<target>`. All targets build with Node 26, and the napi-8 ABI keeps them compatible with Node 24 as well.

### 2. `publish`

After all prebuild jobs succeed, the publish job on `ubuntu-24.04`:

1. Checks out the repo and sets up Node 26 pointed at the npm registry.
2. Runs `npm ci` with `RWEBTRANSPORT_SKIP_POSTINSTALL=1` so the install does not try to compile anything.
3. Downloads every `prebuild-*` artifact and assembles them back into `prebuilds/<target>/rwebtransport.node`.
4. Builds the TypeScript layer with `npm run build:ts`.
5. Packs `prebuilds/` into `rwebtransport-prebuilds.tar.gz` and attaches it to the GitHub Release.
6. Runs `npm publish --access public`, gated on an `NPM_TOKEN` secret being present.

The published package therefore contains the five prebuilds plus the full Rust source, giving both the fast path (a matching prebuilt binary) and the fallback path (compile from source) on any install.

---

## Continuous integration

Separately, [`../.github/workflows/ci.yml`](../.github/workflows/ci.yml) builds from source and runs the test suite on every push and pull request to `main`, across `ubuntu-24.04`, `macos-14`, and `windows-2022`, each on Node 24 and Node 26. It installs the same BoringSSL prerequisites, builds the addon with `npm run build:rust`, builds the `rwebtransport-echo-server` fixture, then runs `typecheck`, `lint`, and `test`. This is the matrix that keeps both Node lines and all three operating systems green.

---

## Troubleshooting

- **"no prebuilt binary for this platform; compiling the native addon".** Normal on an unsupported target or when installing on a platform without a published prebuild. It means the source compile is running; make sure Rust, cmake, a C/C++ compiler, and Go are installed (plus NASM on Windows).
- **"no prebuilt binary ... and no Rust toolchain found".** Postinstall could not find `cargo`. Install Rust from https://rustup.rs and cmake, then reinstall, or run `npm run build:rust` manually.
- **`build completed but ... was not produced`.** The cargo build ran but the `.node` was not copied into place. Rerun `npm run build:rust` and read the cargo output for the underlying compiler error (commonly a missing BoringSSL dependency such as cmake, Go, or NASM).
- **Unsupported Node version error at import.** You are on a Node line other than 24 or 26. Switch to a supported version; there is no build workaround.
