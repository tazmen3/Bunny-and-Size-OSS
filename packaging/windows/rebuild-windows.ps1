[CmdletBinding()]
param([Parameter(Mandatory)][string]$BuildDirectory)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Windows requis (PowerShell 5.1 ou 7).' }
if (Test-Path -LiteralPath $BuildDirectory) { throw 'Choisir un dossier neuf ; aucun écrasement.' }
$buildRoot = [IO.Path]::GetFullPath($BuildDirectory)
New-Item -ItemType Directory -Path $buildRoot | Out-Null
$env:CARGO_HOME = Join-Path $buildRoot 'cargo-home'
$env:CARGO_TARGET_DIR = Join-Path $buildRoot 'target'
$env:CARGO_NET_OFFLINE = 'true'
$env:RUSTUP_TOOLCHAIN = '1.99.0-x86_64-pc-windows-msvc'
foreach ($name in @('RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','CARGO_BUILD_TARGET','SLINT_STYLE','SLINT_DEFAULT_FONT','SLINT_FONT_PATH','SLINT_EMBED_TEXTURES','SLINT_EMBED_RESOURCES')) {
    Remove-Item "Env:$name" -ErrorAction SilentlyContinue
}
Push-Location $PSScriptRoot
try {
    $env:RUSTFLAGS = "--remap-path-prefix=$env:CARGO_TARGET_DIR=/build --remap-path-prefix=$PSScriptRoot=/sortsize --remap-path-prefix=$env:CARGO_HOME=/cargo"
    $env:CARGO_BUILD_JOBS = '2'
    & cargo build --frozen --release --target x86_64-pc-windows-msvc -p sortsize-cli --bins
    if ($LASTEXITCODE -ne 0) { throw 'Reconstruction CLI échouée.' }
    & cargo build --frozen --release --target x86_64-pc-windows-msvc --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
    if ($LASTEXITCODE -ne 0) { throw 'Reconstruction GUI échouée.' }
    Write-Output "Reconstruit hors ligne : $env:CARGO_TARGET_DIR\x86_64-pc-windows-msvc\release"
} finally { Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue; Pop-Location }
