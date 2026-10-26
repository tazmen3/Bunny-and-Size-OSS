[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$AuditRoot,
    [Parameter(Mandatory)][string]$OutputDirectory,
    [string]$Python = 'python',
    [string]$TargetDirectory = $env:CARGO_TARGET_DIR
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    $checkoutState = & git status --porcelain --untracked-files=all
    if ($LASTEXITCODE -ne 0 -or $checkoutState) { throw 'Le checkout doit être propre avant compilation.' }
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Packaging Windows natif requis (PowerShell 5.1 ou 7).' }
    if (-not $TargetDirectory) { throw 'Définir CARGO_TARGET_DIR ou -TargetDirectory.' }
    $targetRoot = [IO.Path]::GetFullPath($TargetDirectory)
    if (Test-Path -LiteralPath $OutputDirectory) { throw 'Le dossier de sortie doit être neuf ; aucun écrasement.' }
    $rustVersion = (& rustc -Vv) -join "`n"
    if ($LASTEXITCODE -ne 0 -or $rustVersion -notmatch 'release: 1\.99\.0' -or $rustVersion -notmatch 'host: x86_64-pc-windows-msvc') {
        throw 'Rust 1.99.0 MSVC x64 requis.'
    }
    foreach ($name in @('RUSTFLAGS','CARGO_ENCODED_RUSTFLAGS','SLINT_STYLE','SLINT_DEFAULT_FONT','SLINT_FONT_PATH','SLINT_EMBED_TEXTURES','SLINT_EMBED_RESOURCES','CARGO_BUILD_TARGET')) {
        if ([Environment]::GetEnvironmentVariable($name)) { throw "Variable de build non prévue : $name" }
    }
    $env:RUSTFLAGS = "--remap-path-prefix=$targetRoot=/build --remap-path-prefix=$repoRoot=/sortsize --remap-path-prefix=$env:CARGO_HOME=/cargo"
    $env:CARGO_BUILD_JOBS = '2'
    $locks = @('Cargo.lock','spikes/slint/Cargo.lock') | ForEach-Object { (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash }
    & cargo build --locked --release --target x86_64-pc-windows-msvc --target-dir $targetRoot -p sortsize-cli --bins
    if ($LASTEXITCODE -ne 0) { throw 'Échec du build CLI.' }
    & cargo build --locked --release --target x86_64-pc-windows-msvc --target-dir $targetRoot --manifest-path spikes/slint/Cargo.toml --bin bunny-and-size
    if ($LASTEXITCODE -ne 0) { throw 'Échec du build GUI.' }
    $after = @('Cargo.lock','spikes/slint/Cargo.lock') | ForEach-Object { (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash }
    if (Compare-Object $locks $after) { throw 'Un lockfile a changé.' }
    & $Python "$PSScriptRoot/package_windows.py" --audit-root $AuditRoot --output $OutputDirectory --target-dir $targetRoot --msvc-version $env:VCToolsVersion
    if ($LASTEXITCODE -ne 0) { throw 'Échec de la préparation des archives.' }
} finally {
    Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
    Pop-Location
}
