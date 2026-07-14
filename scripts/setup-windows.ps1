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
# A native command's non-zero exit must NOT throw: winget returns benign
# non-zero codes (e.g. "already installed"), and we inspect $LASTEXITCODE
# ourselves. This automatic variable only exists on PowerShell 7.3+; assigning
# it on 5.1 is a harmless no-op.
$PSNativeCommandUseErrorActionPreference = $false

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Write-Ok($msg) { Write-Host "  ok  $msg" -ForegroundColor Green }
function Write-Note($msg) { Write-Host "  !!  $msg" -ForegroundColor Yellow }

if ($env:OS -ne 'Windows_NT') {
    Write-Error 'This script is for Windows only.'
}

# Merge the persisted (Machine + User) PATH into the live process PATH so tools
# installed during this run become visible to Get-Command, WITHOUT dropping
# entries the launching shell injected into the process only (a profile tweak,
# a Node version manager, a VS developer shell, etc.).
function Update-SessionPath {
    $reg = @(
        [Environment]::GetEnvironmentVariable('Path', 'Machine'),
        [Environment]::GetEnvironmentVariable('Path', 'User')
    ) -join ';'
    $have = $env:Path -split ';'
    foreach ($p in ($reg -split ';' | Where-Object { $_ })) {
        if ($have -notcontains $p) {
            $env:Path += ";$p"
            $have += $p
        }
    }
}

function Test-Tool($name) {
    return [bool](Get-Command $name -ErrorAction SilentlyContinue)
}

# winget is usually on PATH, but the App Installer execution alias
# (%LOCALAPPDATA%\Microsoft\WindowsApps\winget.exe) is sometimes missing from a
# given shell's PATH, so fall back to it before giving up.
function Resolve-Winget {
    $cmd = Get-Command winget -ErrorAction SilentlyContinue
    if ($cmd) { return $cmd.Source }
    $alias = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps\winget.exe'
    if (Test-Path $alias) { return $alias }
    return $null
}

Write-Step 'Locating winget (the Windows package manager)'
$script:winget = Resolve-Winget
if ($script:winget) {
    Write-Ok "winget found ($script:winget)"
} else {
    Write-Note @'
winget was not found. It ships with Windows 10 (1809+) and 11 as the "App
Installer" package. If it IS installed, open a normal (non-admin) Windows
Terminal where 'winget --version' works and run this script there, or install
App Installer from the Microsoft Store.
Continuing anyway: the script will still check your toolchain and configure
NASM, but it cannot install missing tools. Install any it reports from:
  Rust  https://rustup.rs    CMake https://cmake.org/download/
  Go    https://go.dev/dl/   NASM  https://www.nasm.us/   Node https://nodejs.org/
'@
}

# Install a winget package only if its command is missing.
function Install-Tool($command, $wingetId, $label, $url) {
    Write-Step "Checking for $label"
    if (Test-Tool $command) {
        Write-Ok "$label already installed ($((Get-Command $command).Source))"
        return
    }
    if (-not $script:winget) {
        Write-Note "$label is missing; install it from $url"
        return
    }
    Write-Host "  installing $label via winget ($wingetId) ..."
    & $script:winget install --id $wingetId --exact --silent `
        --accept-source-agreements --accept-package-agreements
    # Inspect the exit code directly (a non-zero native exit does not throw).
    # 0 = installed; -1978335189 (0x8A15002B) = no applicable installer/upgrade,
    # i.e. already present, which is fine.
    $code = $LASTEXITCODE
    if ($code -ne 0 -and $code -ne -1978335189) {
        Write-Note ("winget could not install {0} (exit 0x{1:X}); install it from {2}" -f $label, $code, $url)
    }
    Update-SessionPath
    if (Test-Tool $command) {
        Write-Ok "$label installed"
    } else {
        Write-Note "$label is still not on PATH. Open a new terminal to re-check, or install it from $url"
    }
}

Install-Tool 'cargo' 'Rustlang.Rustup' 'Rust (rustup)' 'https://rustup.rs'
Install-Tool 'cmake' 'Kitware.CMake' 'CMake' 'https://cmake.org/download/'
Install-Tool 'go' 'GoLang.Go' 'Go' 'https://go.dev/dl/'
Install-Tool 'nasm' 'NASM.NASM' 'NASM' 'https://www.nasm.us/'

if (Test-Tool 'rustup') {
    Write-Step 'Ensuring the MSVC Rust toolchain (boring-sys needs MSVC, not GNU)'
    rustup toolchain install stable-x86_64-pc-windows-msvc | Out-Null
    # If the active default toolchain is GNU, switch the default to MSVC so the
    # build actually uses it (cargo would otherwise keep using the GNU default).
    $default = (& rustup default 2>$null | Out-String)
    if ($default -match 'gnu') {
        Write-Note 'Default Rust toolchain is GNU; switching the default to MSVC for this build.'
        rustup default stable-x86_64-pc-windows-msvc | Out-Null
    }
    Write-Ok 'MSVC Rust toolchain ready'
}

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
    # scope) to the real nasm.exe so the boring-sys build stops failing. This
    # alone fixes the reported error; we deliberately do NOT rewrite the
    # persistent User PATH, because reading it expanded and writing it back would
    # bake %VAR% entries from REG_EXPAND_SZ to REG_SZ and lose live expansion.
    [Environment]::SetEnvironmentVariable('ASM_NASM', $nasm, 'User')
    $env:ASM_NASM = $nasm
    # Make nasm reachable in THIS session too (process scope only).
    $nasmDir = Split-Path $nasm
    if (($env:Path -split ';') -notcontains $nasmDir) { $env:Path = "$nasmDir;$env:Path" }
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

Write-Host ''
Write-Step 'Setup finished.'
Write-Host 'Open a NEW terminal so the updated environment is picked up, then run:' -ForegroundColor Cyan
Write-Host '    npm install'
Write-Host '    npm run build'
