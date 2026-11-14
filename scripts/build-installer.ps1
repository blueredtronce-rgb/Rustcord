# Builds dist\Rustcord-Setup-<version>.exe: the release app packed into the release installer
# (src/bin/rustcord-setup, `--pack`). Own code only: no installer tools, no downloads.
#   powershell -ExecutionPolicy Bypass -File scripts\build-installer.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
# A short target dir: the MSVC linker fails on long paths (README, Build).
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = 'C:\rc-target' }
$release = Join-Path $env:CARGO_TARGET_DIR 'release'
$app = Join-Path $release 'rustcord.exe'
$setup = Join-Path $release 'rustcord-setup.exe'

# A running build locks its exe: move it aside (it keeps running) instead of stopping it.
foreach ($exe in $app, $setup) {
    if (-not (Test-Path $exe)) { continue }
    try { [IO.File]::Open($exe, 'Open', 'ReadWrite', 'None').Close() }
    catch {
        $running = Get-Process | Where-Object { $_.Path -eq $exe } | Select-Object -First 1
        $tag = if ($running) { $running.Id } else { Get-Date -Format 'yyyyMMddHHmmss' }
        $name = [IO.Path]::GetFileNameWithoutExtension($exe)
        Rename-Item $exe "$name-running-$tag.exe.old"
    }
}

Push-Location $root
try {
    cargo build --release --bin rustcord --bin rustcord-setup
    if ($LASTEXITCODE) { throw "cargo build failed ($LASTEXITCODE)" }
    $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
    $dist = Join-Path $root 'dist'
    New-Item -ItemType Directory -Force $dist | Out-Null
    $out = Join-Path $dist "Rustcord-Setup-$version.exe"
    # The release setup is a GUI program: wait for it explicitly.
    $p = Start-Process $setup -ArgumentList '--pack', "`"$app`"", "`"$out`"" -Wait -PassThru -NoNewWindow
    if ($p.ExitCode) { throw "packing failed ($($p.ExitCode))" }
    $mb = [math]::Round((Get-Item $out).Length / 1MB, 1)
    Write-Host "Built $out ($mb MB)"
}
finally { Pop-Location }
