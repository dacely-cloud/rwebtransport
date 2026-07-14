#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
#
# scripts/setup-unix.sh
#
# Prepares a Linux or macOS machine to build rwebtransport's native addon from
# source (Cloudflare quiche + BoringSSL via the boring-sys crate). BoringSSL is
# compiled from source, so you need:
#
#   * a C/C++ compiler (clang or gcc)
#   * CMake  - configures the BoringSSL build
#   * Ninja  - the recommended CMake generator (optional; CMake can fall back)
#   * Go     - a BoringSSL build-time code generator
#   * Rust   - the toolchain that compiles the addon
#   * Node   - version 24 or 26 (the only supported lines)
#
# NASM is NOT needed off Windows; the system assembler handles BoringSSL here.
#
# Usage (from the repository root):
#   bash scripts/setup-unix.sh          # install what is missing
#   bash scripts/setup-unix.sh --check  # report only, install nothing
#   npm run setup:unix                  # the same, via npm
#
# It uses the native package manager (apt, dnf, pacman, zypper, or Homebrew) and
# installs Rust via rustup if it is missing. When it finishes, run:
#   npm install
#   npm run build

set -euo pipefail

info() { printf '\033[36m==>\033[0m %s\n' "$*"; }
ok() { printf '  \033[32mok\033[0m  %s\n' "$*"; }
warn() { printf '  \033[33m!!\033[0m  %s\n' "$*"; }

have() { command -v "$1" >/dev/null 2>&1; }

CHECK=0
case "${1:-}" in
  --check | -n) CHECK=1 ;;
  "") ;;
  *) warn "Ignoring unknown argument: $1" ;;
esac

SUDO=""
if [ "$(id -u)" -ne 0 ] && have sudo; then SUDO="sudo"; fi

os="$(uname -s)"

install_linux() {
  if have apt-get; then
    $SUDO apt-get update
    $SUDO apt-get install -y cmake ninja-build golang-go build-essential pkg-config curl
  elif have dnf; then
    $SUDO dnf install -y cmake ninja-build golang gcc gcc-c++ pkgconf-pkg-config curl
  elif have pacman; then
    $SUDO pacman -Sy --needed --noconfirm cmake ninja go base-devel pkgconf curl
  elif have zypper; then
    $SUDO zypper install -y cmake ninja go gcc gcc-c++ pkg-config curl
  else
    warn "No supported package manager (apt, dnf, pacman, zypper) was found."
    warn "Install these manually: a C/C++ compiler, cmake, ninja, go."
    return 1
  fi
}

install_mac() {
  if ! xcode-select -p >/dev/null 2>&1 && ! have clang; then
    info "Installing the Xcode Command Line Tools (C/C++ compiler)..."
    xcode-select --install || true
    warn "Finish the Command Line Tools install if a dialog appeared, then re-run."
  fi
  if ! have brew; then
    warn "Homebrew was not found. Install it from https://brew.sh and re-run,"
    warn "or install cmake, ninja, and go by hand."
    return 1
  fi
  brew install cmake ninja go
}

if [ "$CHECK" -eq 1 ]; then
  info "Detected $os (check mode: nothing will be installed)"
else
  info "Detected $os"
  case "$os" in
    Linux) install_linux || warn "Package install step did not fully complete." ;;
    Darwin) install_mac || warn "Package install step did not fully complete." ;;
    *) warn "Unsupported OS '$os'. This script handles Linux and macOS." ;;
  esac

  # Rust via rustup, if cargo is not already available.
  if have cargo; then
    ok "Rust already installed ($(command -v cargo))"
  else
    info "Installing Rust via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck source=/dev/null
    [ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"
  fi
fi

# Report what the build needs. A missing C compiler, cmake, or go is fatal for
# the build; ninja is optional (cmake falls back to another generator).
info "Toolchain status"
if have cc || have clang || have gcc; then ok "C/C++ compiler present"; else warn "C/C++ compiler MISSING (required)"; fi
for tool in cmake go cargo; do
  if have "$tool"; then ok "$tool -> $(command -v "$tool")"; else warn "$tool MISSING (required)"; fi
done
if have ninja; then ok "ninja -> $(command -v ninja)"; else warn "ninja missing (optional)"; fi

info "Node.js (rwebtransport supports 24 and 26 only)"
if have node; then
  major="$(node -p 'process.versions.node.split(".")[0]' 2>/dev/null || echo '?')"
  if [ "$major" = "24" ] || [ "$major" = "26" ]; then
    ok "Node $(node -v)"
  else
    warn "Node $(node -v) is not supported. Install Node 24 or 26 (nvm, or https://nodejs.org)."
  fi
else
  warn "Node.js not found. Install Node 24 or 26 (nvm, or https://nodejs.org)."
fi

echo ""
if [ "$CHECK" -eq 1 ]; then
  info "Check complete. Re-run without --check to install what is missing."
else
  info "Setup finished. If Rust was just installed, open a new shell (or run '. \"\$HOME/.cargo/env\"'), then:"
  echo "    npm install"
  echo "    npm run build"
fi
