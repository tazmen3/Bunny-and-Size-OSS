[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Directory,
    [Parameter(Mandatory)][string]$Version,
    [Parameter(Mandatory)][string]$PolicyFile,
    [switch]$RequireSignature,
    [switch]$RequireUnsigned
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ($RequireSignature -and $RequireUnsigned) { throw 'Conflicting signature modes.' }
$policy = Get-Content -LiteralPath $PolicyFile -Raw | ConvertFrom-Json
if ($RequireSignature -and (!$policy.publisher_subject -or !$policy.certificate_sha256)) {
    throw 'Public signing identity is not configured; release blocked.'
}
$tools = @(Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" -ErrorAction SilentlyContinue | Sort-Object FullName)
if ($RequireSignature -and !$tools.Count) { throw 'Windows SDK SignTool is required.' }
$rows = @(foreach ($name in @('bunny-and-size.exe','bunny-and-size-cli.exe','sortsize.exe')) {
    $path = Join-Path $Directory $name
    $before = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    $sig = Get-AuthenticodeSignature -LiteralPath $path
    $info = [Diagnostics.FileVersionInfo]::GetVersionInfo($path)
    $subject = $null
    $certHash = $null
    $timestamp = $null
    $toolExit = $null
    if ($sig.SignerCertificate) {
        $subject = $sig.SignerCertificate.Subject
        $hasher = [Security.Cryptography.SHA256]::Create()
        try { $certHash = ([BitConverter]::ToString($hasher.ComputeHash($sig.SignerCertificate.RawData))).Replace('-','').ToLowerInvariant() }
        finally { $hasher.Dispose() }
    }
    if ($sig.TimeStamperCertificate) { $timestamp = $sig.TimeStamperCertificate.Subject }
    if ($info.ProductName -cne $policy.product_name -or $info.ProductVersion -cne $Version) {
        throw "Unexpected product metadata: $name"
    }
    if ($RequireUnsigned -and ($sig.Status -ne 'NotSigned' -or $sig.SignerCertificate -or $sig.TimeStamperCertificate)) {
        throw "Expected unsigned executable: $name ($($sig.Status))"
    }
    if ($RequireSignature) {
        if ($sig.Status -ne 'Valid' -or [string]$sig.SignatureType -ne 'Authenticode') { throw "Invalid embedded signature: $name ($($sig.Status))" }
        if ($subject -cne $policy.publisher_subject -or $certHash -cne $policy.certificate_sha256) { throw "Unexpected publisher/certificate: $name" }
        if (!$timestamp) { throw "Missing timestamp: $name" }
        $eku = @($sig.SignerCertificate.Extensions | Where-Object { $_.Oid.Value -eq '2.5.29.37' } | ForEach-Object { $_.EnhancedKeyUsages } | ForEach-Object { $_.Value })
        if ('1.3.6.1.5.5.7.3.3' -notin $eku) { throw "Missing code signing EKU: $name" }
        # /all checks every embedded signature; /pa uses Authenticode policy.
        # A nonzero exit (including warnings) fails closed. Never import roots.
        $toolLog = @(& $tools[-1].FullName verify /pa /all /tw /v $path 2>&1)
        $toolExit = $LASTEXITCODE
        if ($toolExit -ne 0) { throw "SignTool verification failed: $name (exit $toolExit)" }
    }
    $after = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($before -cne $after) { throw "File changed during verification: $name" }
    [ordered]@{name=$name;sha256=$after;status=[string]$sig.Status;signature_type=[string]$sig.SignatureType;publisher_subject=$subject;certificate_sha256=$certHash;timestamp_subject=$timestamp;signtool_exit=$toolExit;product_name=$info.ProductName;product_version=$info.ProductVersion}
})
[ordered]@{schema=1;public_verified=[bool]$RequireSignature;files=$rows} | ConvertTo-Json -Depth 6 -Compress
