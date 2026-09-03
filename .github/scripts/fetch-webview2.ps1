<#
.SYNOPSIS
    Fetches the WebView2 Evergreen bootstrapper into client/installer/redist/ and
    refuses to hand it on unless it is exactly the file we expected.

.DESCRIPTION
    `client/installer/redist/MicrosoftEdgeWebview2Setup.exe` is deliberately NOT
    committed, and `build.ps1` deliberately does NOT download it. Both README files
    give the same reason and it is the right one: a build that pulls an executable
    off the internet and packages it unverified is precisely the supply-chain problem
    a bank's security review exists to find.

    A release workflow, however, has to get that file from somewhere. So this script
    exists to do the fetch the way the READMEs describe doing it by hand, with the
    verification made mandatory rather than advisory:

      1. Download over TLS from Microsoft's documented go.microsoft.com link.
      2. Verify the SHA-256 against a value pinned OUTSIDE this script, passed in as
         -ExpectedSha256. A hash pinned in the same file that does the download is
         not a pin, it is a comment: whoever can change the download can change the
         hash in the same commit. The workflow reads it from a repository variable so
         changing it is a settings change with an audit trail, made by someone with
         admin rights, rather than a line in a diff.
      3. Verify the Authenticode signature: the file must be signed, the signature
         must be Valid, and the subject must name Microsoft Corporation. A hash match
         alone would be satisfied by any file the pinned hash was updated to.
      4. FAIL CLOSED on any of those. No fallback, no warning-and-continue, no
         "the hash changed, Microsoft must have shipped an update". A bootstrapper
         update is a deliberate act: verify the new file by hand, record its hash in
         the release record, and update the pinned variable.

    The output is a small JSON manifest alongside the payload, which the release
    workflow folds into the release notes. redist/README.md asks for the SHA-256 to
    be recorded "so the exact payload that shipped can be identified later"; this is
    that record, produced automatically instead of from someone's memory.

.PARAMETER ExpectedSha256
    The pinned SHA-256, lowercase or uppercase hex, with or without separators.
    REQUIRED. There is no default and there must not be one.

.PARAMETER Mode
    Evergreen (the only implemented value) or FixedVersion.

    This is the OPEN-5 seam and it is NOT a decision. OPEN-5 asks whether the
    Evergreen bootstrapper — which downloads the runtime from Microsoft at install
    time, and therefore fails on an air-gapped floor — is acceptable, or whether the
    ~150 MB fixed-version runtime has to ship inside the MSI. The answer depends on
    the customer's egress answer from Phase 0 and has not been given.

    So: Evergreen is what the package does today, this script keeps doing exactly
    that, and -Mode FixedVersion throws with a pointer to the decision rather than
    quietly picking an answer. Wiring the fixed-version path here would resolve
    OPEN-5 by implementation, which docs/open-decisions.md says explicitly not to do:
    "an OPEN item must not be invented away."

.PARAMETER OutDir
    Where to place MicrosoftEdgeWebview2Setup.exe. Defaults to the repository's
    client/installer/redist, which is where build.ps1 looks.

.PARAMETER ManifestPath
    Where to write the JSON payload record. Defaults to <OutDir>\webview2-payload.json.

.EXAMPLE
    .\fetch-webview2.ps1 -ExpectedSha256 $env:WEBVIEW2_BOOTSTRAPPER_SHA256

.NOTES
    To establish or rotate the pinned hash, do what redist/README.md says, on a
    machine you trust, and then set the repository variable:

        Invoke-WebRequest https://go.microsoft.com/fwlink/p/?LinkId=2124703 `
            -OutFile MicrosoftEdgeWebview2Setup.exe
        Get-AuthenticodeSignature .\MicrosoftEdgeWebview2Setup.exe | Format-List
        Get-FileHash .\MicrosoftEdgeWebview2Setup.exe -Algorithm SHA256
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ExpectedSha256,

    [ValidateSet("Evergreen", "FixedVersion")]
    [string] $Mode = "Evergreen",

    [string] $OutDir,

    [string] $ManifestPath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Microsoft's documented permalink for the Evergreen Standalone/Bootstrapper
# download. It is a redirector, which is why the hash pin below is load-bearing
# rather than belt-and-braces: what the link resolves to is not under our control.
$EvergreenUrl = "https://go.microsoft.com/fwlink/p/?LinkId=2124703"

# The expected Authenticode subject. A substring match on the CN, not an exact DN
# match: Microsoft rotates the certificate and the full DN changes with it, while the
# organisation name does not.
$ExpectedPublisher = "Microsoft Corporation"

if ($Mode -eq "FixedVersion") {
    throw @"
-Mode FixedVersion is not implemented, on purpose.

Bundling the fixed-version WebView2 runtime instead of the Evergreen bootstrapper is
OPEN-5 (docs/open-decisions.md, and the OPEN-5 section of client/installer/README.md).
It is undecided, and it is not CI's decision to make. Implementing it here would
resolve an open question by writing code that assumes an answer, which is the
specific failure docs/open-decisions.md warns about.

What the package does today: installs the Evergreen bootstrapper with
Return="ignore", so an air-gapped machine gets capture without a widget rather than
no capture at all. That is interim behaviour, documented as interim.

When OPEN-5 is decided in favour of fixed-version, the work is: add the runtime CAB
as a payload, extract it under [INSTALLFOLDER]\WebView2, set
WEBVIEW2_BROWSER_EXECUTABLE_FOLDER for the agent process, and take on the runtime
update obligation Evergreen does not have. Then this branch gets an implementation
and the Evergreen branch gets deleted -- not both kept behind a flag, which would
mean two install paths and one of them untested.
"@
}

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $OutDir) { $OutDir = Join-Path $repoRoot "client\installer\redist" }
if (-not $ManifestPath) { $ManifestPath = Join-Path $OutDir "webview2-payload.json" }

# Normalise the pin so a value pasted from Get-FileHash (uppercase) or from sha256sum
# (lowercase, sometimes with a trailing filename) compares equal to what we compute.
$expected = ($ExpectedSha256 -replace '[^0-9A-Fa-f]', '').ToLowerInvariant()
if ($expected.Length -ne 64) {
    throw "-ExpectedSha256 must be 64 hex characters after separators are stripped; got $($expected.Length)."
}

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$target = Join-Path $OutDir "MicrosoftEdgeWebview2Setup.exe"

Write-Host "webview2: downloading $EvergreenUrl" -ForegroundColor Cyan
# Download to a temporary name and only move it into place after every check passes.
# Writing straight to $target would leave a rejected payload sitting exactly where
# build.ps1 looks for one, and build.ps1 checks only that the file exists.
$staging = Join-Path ([System.IO.Path]::GetTempPath()) ("webview2-" + [guid]::NewGuid().ToString("n") + ".exe")
try {
    # -UseBasicParsing for PowerShell 5.1 compatibility; harmless on 7.x. TLS 1.2 is
    # forced because the default on an older host can still be SSL3/TLS1.0, and a
    # download of an executable is not the place to negotiate downwards.
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $EvergreenUrl -OutFile $staging -UseBasicParsing -MaximumRedirection 10

    $size = (Get-Item $staging).Length
    # The bootstrapper is about 2 MB. A few hundred bytes means a captive portal or an
    # error page saved with a .exe name, and it is worth saying so plainly before the
    # hash mismatch says it obscurely.
    if ($size -lt 500KB) {
        throw "The download is only $size bytes. That is not the bootstrapper -- check for a proxy or captive portal intercepting go.microsoft.com."
    }

    $actual = (Get-FileHash $staging -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw @"
WebView2 bootstrapper SHA-256 mismatch. Refusing to package it.

  expected  $expected
  actual    $actual
  size      $size bytes

This is a fail-closed check and there is no override. Either the download was
tampered with, or Microsoft has shipped a new bootstrapper.

If it is a legitimate update: fetch the file on a machine you trust, verify its
Authenticode signature by hand, record the new SHA-256 in the release record, and
update the WEBVIEW2_BOOTSTRAPPER_SHA256 repository variable. Do not update the pin
from the value this job just printed -- that would make the pin describe whatever
was served, which is the same as having no pin.
"@
    }
    Write-Host "webview2: sha256 matches the pin ($actual)" -ForegroundColor Green

    # Authenticode, second and not instead. The hash proves the file is the one the
    # pin names; the signature proves the pin names a Microsoft file. Both, because
    # each covers the other's failure mode.
    $sig = Get-AuthenticodeSignature $staging
    if ($sig.Status -ne "Valid") {
        throw "Authenticode status is '$($sig.Status)' (StatusMessage: $($sig.StatusMessage)). Expected Valid. Refusing to package an executable whose signature does not verify."
    }
    $subject = $sig.SignerCertificate.Subject
    if ($subject -notmatch [regex]::Escape($ExpectedPublisher)) {
        throw "Authenticode signer is '$subject', which does not name '$ExpectedPublisher'. Refusing to package it."
    }
    Write-Host "webview2: Authenticode Valid, signed by $subject" -ForegroundColor Green

    Move-Item -Force -Path $staging -Destination $target
} finally {
    if (Test-Path $staging) { Remove-Item -Force $staging }
}

# The release record. redist/README.md asks for the SHA-256 to be recorded so the
# exact payload that shipped can be identified later; the workflow reads this file
# and puts it in the release notes.
$manifest = [ordered]@{
    payload            = "MicrosoftEdgeWebview2Setup.exe"
    mode               = $Mode
    open5_status       = "undecided; Evergreen bootstrapper installed with Return=ignore (see client/installer/README.md)"
    source_url         = $EvergreenUrl
    sha256             = $expected
    size_bytes         = (Get-Item $target).Length
    authenticode_valid = $true
    signer_subject     = $subject
    signer_thumbprint  = $sig.SignerCertificate.Thumbprint
    signer_not_after   = $sig.SignerCertificate.NotAfter.ToString("o")
    fetched_at_utc     = (Get-Date).ToUniversalTime().ToString("o")
}
$manifest | ConvertTo-Json -Depth 4 | Set-Content -Path $ManifestPath -Encoding utf8

Write-Host "webview2: wrote $ManifestPath" -ForegroundColor Cyan
Write-Host ($manifest | ConvertTo-Json -Depth 4)
