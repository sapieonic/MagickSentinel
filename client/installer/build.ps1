<#
.SYNOPSIS
    Builds and (optionally) signs the MagickVoice Sentinel MSI.

.DESCRIPTION
    Runs on Windows with the Rust MSVC toolchain and the WiX v4 .NET tool installed.
    See README.md for prerequisites and for the signing procedure.

    The order below is not arbitrary. The binaries are signed *before* the MSI is
    built, and the MSI is signed after: an MSI's own signature covers the package
    file, not the files inside it, so signing only the MSI leaves two unsigned
    executables on disk for EDR to find. Spec 12.1 requires an EV certificate on all
    binaries and the MSI, and spec 18 warns that the EDR conversation takes longer
    than the code — unsigned binaries are the fastest way to make that worse.

.PARAMETER Version
    Three-part MSI version, e.g. 0.1.0. Defaults to the version in Cargo.toml.
    MSI ignores the fourth field when deciding whether an upgrade applies, so the
    build metadata does not belong here.

.PARAMETER SignToolPath
    Path to signtool.exe. Signing is skipped when this is not supplied, which is fine
    for a local build and is not fine for anything a customer receives.

.PARAMETER CertificateThumbprint
    SHA-1 thumbprint of the EV code-signing certificate in the current user's store,
    or of the certificate the HSM/KSP surfaces.

.PARAMETER TimestampUrl
    RFC 3161 timestamp authority. Without a timestamp every signature expires with the
    certificate, and an MSI that was valid at release stops validating a year later on
    machines that have not been updated since.
#>
[CmdletBinding()]
param(
    [string] $Version,
    [string] $Configuration = "release",
    [string] $SignToolPath,
    [string] $CertificateThumbprint,
    [string] $TimestampUrl = "http://timestamp.digicert.com",
    [string] $WebDir,
    [string] $RedistDir,
    [string] $OutputDir
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$RepoRoot   = Resolve-Path (Join-Path $PSScriptRoot "..")
$InstallerDir = $PSScriptRoot
if (-not $OutputDir) { $OutputDir = Join-Path $InstallerDir "out" }
if (-not $WebDir)    { $WebDir    = Join-Path $RepoRoot "..\web\widget\dist" }
if (-not $RedistDir) { $RedistDir = Join-Path $InstallerDir "redist" }

function Assert-Tool($name, $hint) {
    if (-not (Get-Command $name -ErrorAction SilentlyContinue)) {
        throw "$name was not found on PATH. $hint"
    }
}

Assert-Tool "cargo" "Install the Rust toolchain from https://rustup.rs and add the x86_64-pc-windows-msvc target."
Assert-Tool "wix"   "Install the WiX v4 tool: dotnet tool install --global wix --version 4.0.5"

if (-not $Version) {
    # The workspace version is the single source of truth; keeping a second copy in
    # this script is a guarantee that the two will disagree at the worst moment.
    $cargoToml = Get-Content (Join-Path $RepoRoot "Cargo.toml") -Raw
    if ($cargoToml -match '(?m)^\s*version\s*=\s*"([0-9]+\.[0-9]+\.[0-9]+)"') {
        $Version = $Matches[1]
    } else {
        throw "Could not read the workspace version from Cargo.toml; pass -Version explicitly."
    }
}
Write-Host "Building MagickVoice Sentinel $Version ($Configuration)" -ForegroundColor Cyan

# ------------------------------------------------------------------ 1. binaries
# MSVC, not GNU: the MSI ships to managed Windows desktops, EV signing and WER
# symbolication both expect PDBs from the MSVC toolchain, and the mingw runtime would
# be one more unsigned DLL for EDR to object to.
Push-Location $RepoRoot
try {
    $cargoArgs = @("build", "--target", "x86_64-pc-windows-msvc",
                   "-p", "sentinel-agent", "-p", "sentinel-service")
    if ($Configuration -eq "release") { $cargoArgs += "--release" }
    # SQLCipher is a production requirement (spec 6.5 and 12.3): without this feature
    # the spool is plain SQLite and call audio sits unencrypted on the endpoint.
    $cargoArgs += @("--features", "sentinel-core/sqlcipher")
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
} finally {
    Pop-Location
}

$BinDir = Join-Path $RepoRoot "target\x86_64-pc-windows-msvc\$Configuration"
foreach ($exe in @("SentinelAgent.exe", "SentinelService.exe")) {
    $p = Join-Path $BinDir $exe
    if (-not (Test-Path $p)) { throw "Expected $p to exist after the build." }
}

# ------------------------------------------------------------------ 2. payloads
if (-not (Test-Path (Join-Path $WebDir "widget.html"))) {
    throw "The widget bundle was not found at $WebDir. Build web/widget first, or pass -WebDir."
}

$bootstrapper = Join-Path $RedistDir "MicrosoftEdgeWebview2Setup.exe"
if (-not (Test-Path $bootstrapper)) {
    # Deliberately not downloaded by this script: a build that fetches an executable
    # from the internet and packages it unverified is the supply-chain problem the
    # bank's security review exists to find. Fetch it once, check its signature, and
    # commit the hash to the release record.
    throw @"
The WebView2 Evergreen bootstrapper is missing from $RedistDir.

Download it from https://developer.microsoft.com/microsoft-edge/webview2/ ,
verify that it is signed by Microsoft Corporation:

    Get-AuthenticodeSignature $bootstrapper | Format-List

and record its SHA-256 in the release notes before building a customer package.
"@
}

# ------------------------------------------------- 3. sign the binaries (first)
function Invoke-Sign([string[]] $Files) {
    if (-not $SignToolPath -or -not $CertificateThumbprint) {
        Write-Warning "Signing skipped: pass -SignToolPath and -CertificateThumbprint. Do not ship an unsigned build."
        return
    }
    & $SignToolPath sign `
        /sha1 $CertificateThumbprint `
        /fd SHA256 `
        /tr $TimestampUrl `
        /td SHA256 `
        /d "MagickVoice Sentinel" `
        @Files
    if ($LASTEXITCODE -ne 0) { throw "signtool failed" }
}

Invoke-Sign @(
    (Join-Path $BinDir "SentinelAgent.exe"),
    (Join-Path $BinDir "SentinelService.exe")
)

# ---------------------------------------------------------------- 4. build MSI
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$msi = Join-Path $OutputDir "Sentinel-$Version-x64.msi"

& wix build `
    (Join-Path $InstallerDir "Sentinel.wxs") `
    -arch x64 `
    -ext WixToolset.Util.wixext `
    -ext WixToolset.UI.wixext `
    -d "ProductVersion=$Version" `
    -d "BinDir=$BinDir" `
    -d "WebDir=$WebDir" `
    -d "RedistDir=$RedistDir" `
    -out $msi
if ($LASTEXITCODE -ne 0) { throw "wix build failed" }

# --------------------------------------------------------- 5. sign the package
Invoke-Sign @($msi)

Write-Host "Built $msi" -ForegroundColor Green
Write-Host ""
Write-Host "Verify before shipping:" -ForegroundColor Cyan
Write-Host "  Get-AuthenticodeSignature '$msi' | Format-List"
Write-Host "  msiexec /i '$msi' /qn /l*v install.log ENROLLMENTTOKEN=<token> APIBASEURL=<url>"
Write-Host ""
Write-Host "On a tier C machine the install must fail with the tier message and roll back"
Write-Host "cleanly. That is the acceptance test for this package; run it on a Windows 8.1"
Write-Host "or pre-1903 Windows 10 VM before every release."
