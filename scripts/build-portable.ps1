# made by CasualArclamp
# Builds single-file DecSAT executables for 64-bit Windows 10/11 that need nothing
# installed: the C runtime is linked statically, so no Visual C++ redistributable
# (the same build as release.yml). By default every app is built; the results go
# to exe\ (git-ignored): decsat-gui.exe, decsat.exe (command line) and
# decsat-tx.exe (transmitter). The apps' default features bring every receiver
# (HackRF, Fobos, Airspy Mini / R2 / HydraSDR, UHD, Pluto, RTL-SDR).
# After DecLAB's script.
#
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\build-portable.ps1 gui cli   # just some
#
# Needs the submodules (git submodule update --init) and CMake for libopus; if
# cmake is not on PATH, Visual Studio's own is used. Builds in target\portable,
# so the usual target directory and its cache stay as they are.

# Note: with -File, PowerShell binds only the first bare word to a
# [string[]] parameter, so the names are taken from all remaining arguments.
param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Apps)
if (-not $Apps -or $Apps.Count -eq 0) { $Apps = @("gui", "cli", "tx") }

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$triple = "x86_64-pc-windows-msvc"

# app -> (cargo package, exe name)
$all = [ordered]@{
    gui = @("decsat-gui", "decsat-gui.exe")
    cli = @("decsat-cli", "decsat.exe")
    tx  = @("decsat-tx-app", "decsat-tx.exe")
    server = @("decsat-server", "decsat-server.exe")
    serverapp = @("decsat-server-app", "decsat-server-app.exe")
}
foreach ($a in $Apps) {
    if (-not $all.Contains($a)) { throw "unknown app '$a' (gui, cli, tx)" }
}

if (-not (Get-Command cmake -ErrorAction SilentlyContinue)) {
    $vsCmake = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin"
    if (Test-Path $vsCmake) { $env:PATH = "$vsCmake;$env:PATH" }
}

# With an explicit --target the flags reach only the executables, not build scripts
# and proc macros.
$env:RUSTFLAGS = "-C target-feature=+crt-static"
$env:CARGO_TARGET_DIR = Join-Path $root "target\portable"
if (-not $env:CARGO_BUILD_JOBS) { $env:CARGO_BUILD_JOBS = "6" }

Push-Location $root
try {
    $pkgs = foreach ($a in $Apps) { "-p"; $all[$a][0] }
    cargo build --release --target $triple @pkgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    $out = Join-Path $root "exe"
    New-Item -ItemType Directory -Force $out | Out-Null
    foreach ($a in $Apps) {
        $exe = $all[$a][1]
        Copy-Item (Join-Path $env:CARGO_TARGET_DIR "$triple\release\$exe") $out -Force
        $mb = (Get-Item (Join-Path $out $exe)).Length / 1MB
        Write-Host ("{0,-16} {1,6:N1} MB" -f $exe, $mb)
    }
    Write-Host "written to $out"
} finally {
    Pop-Location
}
