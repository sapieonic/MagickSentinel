<#
.SYNOPSIS
    The tier-C acceptance gate. Run this on a Windows 8.1 or pre-1903 Windows 10
    machine before every release. The install MUST fail and MUST roll back cleanly.

.DESCRIPTION
    client/installer/README.md requires this before every release, and the reason is
    not "the installer should validate its inputs". It is this:

      A tier C install that SUCCEEDS produces a service that runs, reports healthy in
      every 30-second heartbeat, and never captures a single call.

    That is strictly worse than no install, because the portal's fleet view then says
    the machine is covered. The BPO tells its bank client that 100% of calls are
    monitored. Neither of them finds out otherwise until a coverage reconciliation
    against the dialer CDR is running (OPEN-7, not built) or until a compliance
    incident on an uncaptured call. This gate is the only thing between that and a
    release.

    WHY IT IS MANUAL, AND WHY THAT IS NOT LAZINESS. The four tier C cases are x86,
    ARM64, Windows 8.1 and Windows 10 before build 18362. GitHub-hosted runners are
    x64 Windows Server 2022 or later, so a hosted runner is a tier A machine and
    cannot be made to look like any of the four. In particular:

      * You cannot fake the check. It is a WiX FileSearch with @MinVersion against
        kernel32.dll's version resource, chosen precisely because it cannot be
        spoofed by the things that can be spoofed -- see the README's explanation of
        why VersionNT and CurrentBuildNumber are both useless here. Setting a
        registry value or applying a compatibility shim does not change the answer,
        which is the point of the design and also why no hosted runner can test it.
      * A VM is required, and it must be a real pre-1903 image. An in-place-upgraded
        Windows 10 that reports 19045 is a tier B machine and will pass the install,
        which is a pass that proves nothing.

    So the gate is a script an engineer runs on a VM, producing a machine-readable
    result that the release workflow's publish approval requires. Automating the
    approval without automating the test would be worse than leaving it manual.

    WHAT IT CHECKS.

      1. msiexec exits non-zero. A silent install that exits 0 on a tier C machine is
         the failure this whole gate exists to catch.
      2. The verbose log names the tier launch condition. A non-zero exit for some
         other reason -- a corrupt MSI, a missing prerequisite, an ICE failure -- is
         not a pass. It looks like one, and next release the real condition might be
         broken while the install still fails for the other reason.
      3. Rollback is clean, checked against the four things the package creates:
           - the SentinelSvc service is absent
           - INSTALLFOLDER is absent
           - HKLM\SOFTWARE\MagickVoice\Sentinel is absent
           - the WER LocalDumps policy keys are absent
         "Failed and rolled back" and "failed and left a service registered" are very
         different outcomes and msiexec's exit code does not distinguish them.
      4. It refuses to run on a machine that is not tier C, because a pass on tier A
         hardware is a false pass and false passes on this gate are how the failure
         above reaches a floor.

.PARAMETER MsiPath
    The MSI to test. Use the actual release candidate, signed, not a local rebuild:
    part of what this verifies is that the package as published blocks the install.

.PARAMETER ResultPath
    Where to write the JSON result. Defaults to .\tier-c-acceptance.json. Attach it
    to the release approval.

.PARAMETER LogPath
    msiexec verbose log path. Defaults to .\tierc.log.

.PARAMETER Force
    Run even though this machine is not tier C. For rehearsing the script only; the
    result is stamped not_a_gate and MUST NOT be attached to a release approval.

.EXAMPLE
    # On a Windows 8.1 or pre-1903 Windows 10 VM, as administrator:
    .\tier-c-acceptance.ps1 -MsiPath .\Sentinel-0.1.0-x64.msi

.NOTES
    The support matrix authority is client/sentinel-capture/src/tier.rs. The
    thresholds are 22000 (Windows 11 client), 20348 (Server 2022) and 18362 (the tier
    B floor). 20348 is a SERVER build number: using it as a client threshold reports
    every Windows 10 desktop as tier A and process loopback then fails to activate on
    all of them. Do not "simplify" the numbers in this script either.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $MsiPath,

    [string] $ResultPath = ".\tier-c-acceptance.json",
    [string] $LogPath = ".\tierc.log",
    [switch] $Force
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Mirrors client/sentinel-capture/src/tier.rs. Kept as named constants so a reader
# can see which is which; a bare 18362 in a comparison is how 20348 got used as a
# client threshold in the first place.
$TIER_B_FLOOR_BUILD    = 18362   # Windows 10 1903. Below this is tier C.
$WINDOWS_11_BUILD      = 22000
$SERVER_2022_BUILD     = 20348

function Get-OsBuildNumber {
    # Read the build from the registry, not from GetVersionEx or [Environment]::OSVersion:
    # both lie to an unmanifested process, which is exactly the trap the WXS avoids by
    # using a FileSearch. CurrentBuildNumber is a REG_SZ, so cast it deliberately.
    $k = "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion"
    $v = (Get-ItemProperty -Path $k -Name CurrentBuildNumber).CurrentBuildNumber
    return [int] $v
}

function Get-NativeArchitecture {
    # PROCESSOR_ARCHITEW6432 is set when a 32-bit process runs on a 64-bit OS, so
    # prefer it. On ARM64 running x64 emulation, PROCESSOR_ARCHITECTURE reports the
    # emulated architecture, which is why ARM64 is a tier C case at all: the package
    # would otherwise install and never capture.
    if ($env:PROCESSOR_ARCHITEW6432) { return $env:PROCESSOR_ARCHITEW6432 }
    return $env:PROCESSOR_ARCHITECTURE
}

$build = Get-OsBuildNumber
$arch = Get-NativeArchitecture
$isTierC = ($build -lt $TIER_B_FLOOR_BUILD) -or ($arch -ne "AMD64")

Write-Host "tier-c: this machine reports build $build, native arch $arch" -ForegroundColor Cyan
if ($build -ge $WINDOWS_11_BUILD) {
    Write-Host "tier-c: that is tier A (Windows 11 client, >= $WINDOWS_11_BUILD)"
} elseif ($build -ge $SERVER_2022_BUILD) {
    Write-Host "tier-c: that is tier A (Server 2022 or later, >= $SERVER_2022_BUILD)"
} elseif ($build -ge $TIER_B_FLOOR_BUILD) {
    Write-Host "tier-c: that is tier B (Windows 10 1903..22H2)"
} else {
    Write-Host "tier-c: that is tier C (below $TIER_B_FLOOR_BUILD)"
}

if (-not $isTierC -and -not $Force) {
    throw @"
This machine is tier $(if ($build -ge $SERVER_2022_BUILD) { "A" } else { "B" }) (build $build, $arch), not tier C.

The install is SUPPOSED to succeed here, so running the gate would produce a
meaningless failure or, worse, a meaningless pass. Run it on:

  * a Windows 8.1 VM (kernel32 6.3.9600.x), or
  * a Windows 10 image earlier than 1903 / build 18362 -- a genuine pre-1903 image,
    not one that has been in-place upgraded, or
  * an ARM64 machine (the package installs under x64 emulation and never captures,
    which is the case the ARM64 launch condition exists for).

-Force rehearses the script and stamps the result not_a_gate. Do not attach a
not_a_gate result to a release approval.
"@
}

if (-not (Test-Path $MsiPath)) { throw "MSI not found at $MsiPath" }
$msi = Resolve-Path $MsiPath

# The signature is part of what ships, so record it. Not a gate here -- an unsigned
# smoke-test MSI can still be used to rehearse the tier check -- but an approval
# reviewer should be able to see whether the artefact tested was the artefact signed.
$sig = Get-AuthenticodeSignature $msi
Write-Host "tier-c: MSI Authenticode status = $($sig.Status)"

if (Test-Path $LogPath) { Remove-Item -Force $LogPath }

Write-Host "tier-c: msiexec /i $msi /qn /l*v $LogPath" -ForegroundColor Cyan
$proc = Start-Process -FilePath "msiexec.exe" `
    -ArgumentList @("/i", "`"$msi`"", "/qn", "/l*v", "`"$LogPath`"") `
    -Wait -PassThru
$exitCode = $proc.ExitCode
Write-Host "tier-c: msiexec exit code $exitCode"

$log = if (Test-Path $LogPath) { Get-Content -Raw $LogPath } else { "" }

# The tier launch condition's message text. Matched loosely on the distinctive parts
# rather than on the whole sentence, so a wording change in Sentinel.wxs does not
# silently turn this check into a no-op that always fails -- and a wording change
# that DOES break it fails visibly here, which is the correct place to notice.
$tierMessageMatched = ($log -match "18362") -or
                      ($log -match "(?i)requires Windows 10") -or
                      ($log -match "(?i)not supported on this version of Windows") -or
                      ($log -match "(?i)ARM64")

# Rollback evidence, one check per thing the package creates.
$serviceAbsent = $null -eq (Get-Service -Name "SentinelSvc" -ErrorAction SilentlyContinue)
$installFolder = Join-Path ${env:ProgramFiles} "MagickVoice\Sentinel"
$installFolderAbsent = -not (Test-Path $installFolder)
$productRegAbsent = -not (Test-Path "HKLM:\SOFTWARE\MagickVoice\Sentinel")
$werAbsent =
    (-not (Test-Path "HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\SentinelService.exe")) -and
    (-not (Test-Path "HKLM:\SOFTWARE\Microsoft\Windows\Windows Error Reporting\LocalDumps\SentinelAgent.exe"))

$checks = [ordered]@{
    install_failed        = ($exitCode -ne 0)
    tier_message_in_log   = $tierMessageMatched
    service_absent        = $serviceAbsent
    install_folder_absent = $installFolderAbsent
    product_registry_absent = $productRegAbsent
    wer_policy_absent     = $werAbsent
}

foreach ($k in $checks.Keys) {
    $ok = $checks[$k]
    $mark = if ($ok) { "PASS" } else { "FAIL" }
    $colour = if ($ok) { "Green" } else { "Red" }
    Write-Host ("  {0,-24} {1}" -f $k, $mark) -ForegroundColor $colour
}

$passed = -not ($checks.Values -contains $false)

$result = [ordered]@{
    gate           = "tier-c-install-must-fail-and-roll-back"
    passed         = $passed
    not_a_gate     = [bool] $Force -and -not $isTierC
    msi            = $msi.Path
    msi_sha256     = (Get-FileHash $msi -Algorithm SHA256).Hash.ToLowerInvariant()
    msi_signature  = $sig.Status.ToString()
    machine        = [ordered]@{
        os_build      = $build
        native_arch   = $arch
        classified_as = if ($build -lt $TIER_B_FLOOR_BUILD -or $arch -ne "AMD64") { "C" }
                        elseif ($build -ge $SERVER_2022_BUILD) { "A" } else { "B" }
        computer_name = $env:COMPUTERNAME
    }
    msiexec_exit   = $exitCode
    checks         = $checks
    log            = (Resolve-Path $LogPath -ErrorAction SilentlyContinue).Path
    run_by         = "$env:USERDOMAIN\$env:USERNAME"
    run_at_utc     = (Get-Date).ToUniversalTime().ToString("o")
}
$result | ConvertTo-Json -Depth 5 | Set-Content -Path $ResultPath -Encoding utf8
Write-Host "tier-c: wrote $ResultPath" -ForegroundColor Cyan

if (-not $passed) {
    throw "TIER C ACCEPTANCE GATE FAILED. Do not release. See $LogPath and $ResultPath."
}
if ($result.not_a_gate) {
    Write-Warning "Result is stamped not_a_gate (-Force on a non-tier-C machine). It does not satisfy the release gate."
    exit 0
}
Write-Host "TIER C ACCEPTANCE GATE PASSED: the install failed with the tier message and rolled back cleanly." -ForegroundColor Green
