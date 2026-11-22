#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
case "$(uname -m)" in
  x86_64) machine=x86_64; arch=x64 ;;
  aarch64) machine=aarch64; arch=arm64 ;;
  *) echo "Unsupported Linux prebuild architecture" >&2; exit 1 ;;
esac

build_dir=$(mktemp -d)
trap 'rm -rf -- "$build_dir"' EXIT
rust_sysroot=$(rustc --print sysroot)
go_sysroot=$(go env GOROOT)
runtime=${RWT_CONTAINER_RUNTIME:-docker}
image="quay.io/pypa/manylinux_2_28_${machine}:latest"

"$runtime" run --rm \
  --mount "type=bind,source=$root,target=/repo,readonly" \
  --mount "type=bind,source=$rust_sysroot,target=/rust,readonly" \
  --mount "type=bind,source=$go_sysroot,target=/go,readonly" \
  --mount "type=bind,source=$build_dir,target=/output" \
  --env "CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-8}" \
  --workdir /repo "$image" bash -euc '
    dnf -y -q --setopt=install_weak_deps=False install clang-devel ninja-build
    export PATH="/rust/bin:/go/bin:$PATH"
    export CARGO_TARGET_DIR=/tmp/rwt-target
    cargo build --locked --release -p rwebtransport-native
    install -m 755 "$CARGO_TARGET_DIR/release/librwebtransport_native.so" /output/rwebtransport.node
  '

check_versions() {
  local family=$1 maximum=$2 actual
  actual=$(readelf --version-info "$build_dir/rwebtransport.node" |
    awk -v family="$family" '{while (match($0, family "_[0-9]+(\\.[0-9]+)+")) { print substr($0, RSTART, RLENGTH); $0 = substr($0, RSTART + RLENGTH) }}' |
    sort -Vu | tail -1)
  if [[ -n "$actual" && "$(printf '%s\n' "$actual" "${family}_${maximum}" | sort -V | tail -1)" != "${family}_${maximum}" ]]; then
    echo "Linux prebuild requires $actual, above the supported ${family}_${maximum} baseline" >&2
    exit 1
  fi
}

check_versions GLIBC 2.28
check_versions GLIBCXX 3.4.25
install -D -m 755 "$build_dir/rwebtransport.node" "$root/prebuilds/linux-$arch/rwebtransport.node"
