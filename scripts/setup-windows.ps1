#Requires -Version 5.1
# SPDX-License-Identifier: Apache-2.0
#
# scripts/setup-windows.ps1
#
# Prepares a Windows machine to build rwebtransport's native addon from source
# (Cloudflare quiche + BoringSSL via the boring-sys crate). On top of Rust and
# the MSVC C++ build tools, BoringSSL on Windows needs:
#
#   * CMake - configures the BoringSSL build
#   * NASM  - assembles BoringSSL's crypto (this is the usual missing piece)
#   * Go    - a BoringSSL build-time code generator
#   * Node  - version 24 or 26 (the only supported lines)
#
# The script installs whatever is missing with winget, then points cmake at
# NASM by setting the ASM_NASM environment variable to the real nasm.exe. That
# is the exact variable cmake complains about with:
#
#   CMake Error: Could not find the compiler specified in the environment
#   variable ASM_NASM: C:\...\nasm.exe
#
# Run it from the repository root (an elevated shell is not required):
#
#   powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
#
# or the equivalent npm alias:
#
#   npm run setup:windows
#
# When it finishes, open a NEW terminal so the updated environment is picked up,
# then build:
#
#   npm install
#   npm run build

$ErrorActionPreference = 'Stop'

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "  ok  $msg" -ForegroundColor Green }
function Write-Note($msg) { Write-Host "  !!  $msg" -ForegroundColor Yellow }

if ($env:OS -ne 'Windows_NT') {
    Write-Error 'This script is for Windows only.'
}

# Reload PATH from the registry so tools installed during this run become
# visible to Get-Command without opening a new shell.
function Update-SessionPath {
    $machine = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $user = [Environment]::GetEnvironmentVariable('Path', 'User')
    $env:Path = @($machine, $user | Where-Object { $_ }) -join ';'
}

function Test-Tool($name) {
    return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

# ---- winget ----------------------------------------------------------------
Write-Step 'Checking for winget (the Windows package manager)'
if (-not (Test-Tool 'winget')) {
    Write-Error @'
winget was not found. It ships with Windows 10 (1809+) and Windows 11 as the
"App Installer" package; install it from the Microsoft Store, then re-run this.
Or install the tools by hand:
  Rust:  https://rustup.rs
  CMake: https://cmake.org/download/
  NASM:  https://www.nasm.us/
  Go:    https://go.dev/dl/
  Node:  https://nodejs.org/  (version 24 or 26)
'@
}
Write-Ok 'winget is available'

# Install a winget package only if its command is missing.
function Install-Tool($command, $wingetId, $label) {
    Write-Step "Checking for $label"
    if (Test-Tool $command) {
        Write-Ok "$label already installed ($((Get-Command $command).Source))"
        return
    }
    Write-Host "  installing $label via winget ($wingetId) ..."
    try {
        winget install --id $wingetId --exact --silent `
            --accept-source-agreements --accept-package-agreements
    } catch {
        Write-Note "winget reported: $($_.Exception.Message)"
    }
    Update-SessionPath
    if (Test-Tool $command) {
        Write-Ok "$label installed"
    } else {
        Write-Note "$label was installed but '$command' is not on PATH yet; a new terminal may be needed."
    }
}

Install-Tool 'cargo' 'Rustlang.Rustup' 'Rust (rustup)'
Install-Tool 'cmake' 'Kitware.CMake' 'CMake'
Install-Tool 'go' 'GoLang.Go' 'Go'
Install-Tool 'nasm' 'NASM.NASM' 'NASM'

# ---- Rust MSVC toolchain ----------------------------------------------------
if (Test-Tool 'rustup') {
    Write-Step 'Ensuring the MSVC Rust toolchain (boring-sys needs MSVC, not GNU)'
    try {
        rustup toolchain install stable-x86_64-pc-windows-msvc | Out-Null
        Write-Ok 'stable-x86_64-pc-windows-msvc is installed'
    } catch {
        Write-Note "Could not install the MSVC toolchain automatically: $($_.Exception.Message)"
    }
}

# ---- NASM discovery + the ASM_NASM fix -------------------------------------
Write-Step 'Pointing cmake at NASM (the ASM_NASM environment variable)'

function Find-Nasm {
    $cmd = Get-Command nasm -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $roots = @(
        "$env:ProgramFiles\NASM",
        "${env:ProgramFiles(x86)}\NASM",
        "$env:LOCALAPPDATA\bin\NASM",
        "$env:LOCALAPPDATA\Microsoft\WinGet\Packages"
    ) | Where-Object { $_ -and (Test-Path $_) }
    foreach ($root in $roots) {
        $hit = Get-ChildItem -Path $root -Filter nasm.exe -Recurse -ErrorAction SilentlyContinue |
            Select-Object -First 1
        if ($hit) { return $hit.FullName }
    }
    return $null
}

$nasm = Find-Nasm
if ($nasm) {
    # cmake reads ASM_NASM to locate the assembler. Set it (persistently, user
    # scope) to the real nasm.exe so the boring-sys build stops failing.
    [Environment]::SetEnvironmentVariable('ASM_NASM', $nasm, 'User')
    $env:ASM_NASM = $nasm

    # Belt and suspenders: make sure NASM's folder is on the user PATH too.
    $nasmDir = Split-Path $nasm
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $nasmDir) {
        $newPath = if ($userPath) { "$userPath;$nasmDir" } else { $nasmDir }
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
    }
    Update-SessionPath
    Write-Ok "ASM_NASM = $nasm"
} else {
    # Clear a stale ASM_NASM that points to a missing file (this is the exact
    # state that produced the original error) so cmake can search PATH instead.
    $stale = [Environment]::GetEnvironmentVariable('ASM_NASM', 'User')
    if ($stale -and -not (Test-Path $stale)) {
        [Environment]::SetEnvironmentVariable('ASM_NASM', $null, 'User')
        Remove-Item Env:\ASM_NASM -ErrorAction SilentlyContinue
        Write-Note "Removed a stale ASM_NASM that pointed to a missing file: $stale"
    }
    Write-Note 'NASM was not found. Install it from https://www.nasm.us/ and re-run this script.'
}

# ---- MSVC C++ build tools ---------------------------------------------------
Write-Step 'Checking for the MSVC C++ build tools'
$hasVc = Test-Tool 'cl'
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not $hasVc -and (Test-Path $vswhere)) {
    $vc = & $vswhere -latest -products * `
        -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
        -property installationPath
    if ($vc) { $hasVc = $true }
}
if ($hasVc) {
    Write-Ok 'MSVC C++ build tools found'
} else {
    Write-Note @'
The MSVC C++ build tools were not detected. BoringSSL needs a C/C++ compiler.
Install the "Desktop development with C++" workload, for example:
  winget install --id Microsoft.VisualStudio.2022.BuildTools --exact --override "--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --quiet --wait"
or add that workload from the Visual Studio Installer.
'@
}

# ---- Node.js ----------------------------------------------------------------
Write-Step 'Checking Node.js (rwebtransport supports 24 and 26 only)'
if (Test-Tool 'node') {
    $nodeVer = (& node -v).TrimStart('v')
    $major = [int]($nodeVer.Split('.')[0])
    if ($major -eq 24 -or $major -eq 26) {
        Write-Ok "Node $nodeVer"
    } else {
        Write-Note "Node $nodeVer is not supported. Install Node 24 or 26 (https://nodejs.org or nvm-windows)."
    }
} else {
    Write-Note 'Node.js was not found. Install Node 24 or 26 from https://nodejs.org.'
}

# ---- Done -------------------------------------------------------------------
Write-Host ''
Write-Step 'Setup finished.'
Write-Host 'Open a NEW terminal so the updated environment is picked up, then run:' -ForegroundColor Cyan
Write-Host '    npm install'
Write-Host '    npm run build'
