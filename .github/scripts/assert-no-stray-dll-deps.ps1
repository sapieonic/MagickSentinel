<#
.SYNOPSIS
    Fails if a shipping binary imports a DLL the MSI does not package.

.DESCRIPTION
    THE FAILURE THIS CATCHES, because it is invisible everywhere else.

    `client/installer/Sentinel.wxs` packages exactly three files: SentinelService.exe,
    SentinelAgent.exe and widget.html. If either executable imports a DLL that is not
    part of Windows and not in the package, then on every installed machine:

      * the MSI installs successfully,
      * the service is registered and set to auto-start,
      * and the binary fails to load with a missing-DLL error.

    So the install succeeds, the deployment tool reports success, and the fleet has
    machines that will never capture a call. That is this product's characteristic
    failure mode — healthy-looking and silent — arriving through a linker flag.

    IT IS NOT HYPOTHETICAL. The `sqlcipher` feature is
    `libsqlite3-sys/bundled-sqlcipher`, whose build script emits
    `cargo:rustc-link-lib=dylib=libcrypto` on Windows. Whether that produces a DLL
    dependency depends entirely on which kind of `libcrypto.lib` was on the link line:
    a dynamic OpenSSL install (the hosted runner's preinstalled one, and every Win32
    OpenSSL installer) supplies an import library and the executables then need
    `libcrypto-3-x64.dll`; a static one (`x64-windows-static-md`, which
    .github/actions/sqlcipher-openssl installs) links it in and they do not. Both
    builds succeed identically. Only this check tells them apart.

    The same reasoning covers the MSVC runtime, mingw's runtime, and anything else a
    future dependency drags in.

.PARAMETER Path
    One or more .exe or .dll files to inspect.

.PARAMETER AllowExtra
    Additional DLL names to permit, for a payload that genuinely is packaged. Adding a
    name here is a claim that Sentinel.wxs ships it — check that it does.

.NOTES
    Uses `dumpbin /dependents`, which is part of the MSVC toolchain and therefore
    present wherever these binaries were built. The alternative, Dependencies.exe, is a
    third-party download; for a check that exists to keep third-party binaries out of
    the package, that would be an odd dependency to take on.

    The allow-list below is deliberately narrow and static rather than "anything in
    System32". A DLL that happens to exist on the build machine is not evidence that it
    exists on a locked-down Windows 10 desktop with the customer's baseline applied,
    and the whole point of this check is what happens on that machine rather than on
    this one.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, ValueFromRemainingArguments = $true)]
    [string[]] $Path,

    [string[]] $AllowExtra = @()
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# Core Windows DLLs that are present on every supported build (tier A and tier B, i.e.
# Windows 10 1903 / build 18362 and later). Anything not on this list has to be either
# packaged or removed.
#
# Note what is NOT here: vcruntime140.dll and msvcp140.dll. The MSVC runtime is
# redistributable and is NOT guaranteed present — a clean Windows 10 image may not have
# it, and "the build machine had Visual Studio" is exactly the assumption that produces
# a package that works everywhere except the customer's SOE. If a build starts importing
# them, the choice is to link the CRT statically (`-C target-feature=+crt-static`) or to
# add the redistributable to the MSI as a prerequisite; it is not to add them here.
$Allowed = @(
    # Base
    'kernel32.dll', 'kernelbase.dll', 'ntdll.dll', 'advapi32.dll', 'user32.dll',
    'gdi32.dll', 'shell32.dll', 'shlwapi.dll', 'ole32.dll', 'oleaut32.dll',
    'combase.dll', 'rpcrt4.dll', 'sechost.dll', 'msvcrt.dll',
    # Networking and crypto that ships with Windows
    'ws2_32.dll', 'crypt32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'ncrypt.dll',
    'secur32.dll', 'iphlpapi.dll', 'userenv.dll',
    # Audio, WASAPI and the COM surfaces the capture code uses
    'mmdevapi.dll', 'audioses.dll', 'avrt.dll', 'winmm.dll',
    # Service control, sessions, diagnostics
    'wtsapi32.dll', 'powrprof.dll', 'dbghelp.dll', 'version.dll', 'psapi.dll',
    'api-ms-win-core-synch-l1-2-0.dll'
) + $AllowExtra

# api-ms-win-* and ext-ms-win-* are API-set contract names. They resolve through the
# API-set schema on every supported build and are not real files, so matching them by
# prefix is correct rather than lazy.
$AllowedPrefixes = @('api-ms-win-', 'ext-ms-win-')

$dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
if (-not $dumpbin) {
    # Search the VS installation, since dumpbin is not on PATH outside a developer
    # command prompt.
    $candidates = Get-ChildItem -Path "${env:ProgramFiles}\Microsoft Visual Studio", "${env:ProgramFiles(x86)}\Microsoft Visual Studio" `
        -Recurse -Filter dumpbin.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\Hostx64\\x64\\' } |
        Sort-Object FullName -Descending
    if (-not $candidates) {
        throw "dumpbin.exe was not found. It ships with the MSVC toolchain, which built these binaries, so its absence means this is not the machine that built them."
    }
    $dumpbin = $candidates[0]
}
Write-Host "using $($dumpbin.Source ?? $dumpbin.FullName)"

$failures = @()

foreach ($p in $Path) {
    if (-not (Test-Path $p)) { throw "not found: $p" }
    $resolved = (Resolve-Path $p).Path
    Write-Host ""
    Write-Host "== $([IO.Path]::GetFileName($resolved))" -ForegroundColor Cyan

    $output = & ($dumpbin.Source ?? $dumpbin.FullName) /nologo /dependents $resolved
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed on $resolved" }

    # dumpbin prints the imports as an indented block between "Image has the following
    # dependencies:" and "Summary". Parsing by shape rather than by line offset,
    # because the header wording differs between toolchain versions.
    $imports = $output |
        Where-Object { $_ -match '^\s{4}\S+\.dll\s*$' } |
        ForEach-Object { $_.Trim().ToLowerInvariant() } |
        Sort-Object -Unique

    foreach ($dll in $imports) {
        $ok = ($Allowed -contains $dll) -or ($AllowedPrefixes | Where-Object { $dll.StartsWith($_) })
        if ($ok) {
            Write-Host ("   ok      {0}" -f $dll)
        } else {
            Write-Host ("   STRAY   {0}" -f $dll) -ForegroundColor Red
            $failures += [pscustomobject]@{ binary = [IO.Path]::GetFileName($resolved); dll = $dll }
        }
    }
}

if ($failures.Count -gt 0) {
    $list = ($failures | ForEach-Object { "  $($_.binary) imports $($_.dll)" }) -join "`n"
    throw @"
Shipping binaries import DLLs that client/installer/Sentinel.wxs does not package.

$list

The MSI would install successfully, register the service, and leave a machine on which
the binaries cannot start. The install succeeds, the deployment tool reports success,
and the fleet view says the machine is covered while it captures nothing.

If this is libcrypto-3-x64.dll: the build linked against a DYNAMIC OpenSSL. Use the
x64-windows-static-md triplet (.github/actions/sqlcipher-openssl does) so libcrypto is
linked into the executables. Packaging the DLL instead would put an unsigned
third-party binary beside two EV-signed ones, in a directory a SYSTEM service executes
from -- which is both an EDR conversation and a DLL-planting surface.

If this is vcruntime140.dll or msvcp140.dll: link the CRT statically, or add the
Visual C++ redistributable to the package as a prerequisite. Do not assume it is
present on a customer's standard image.

Otherwise: either package it deliberately, in Sentinel.wxs, with the signing and
provenance that implies -- or remove the dependency.
"@
}

Write-Host ""
Write-Host "No stray DLL dependencies: every import is a Windows-provided library." -ForegroundColor Green
