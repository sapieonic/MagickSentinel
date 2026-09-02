# MagickVoice Sentinel installer

WiX v4 project producing a per-machine x64 MSI for SCCM / Intune deployment
(spec section 5).

| File | What it is |
|---|---|
| `Sentinel.wxs` | The package authoring. Tier gate, service install, data-directory ACL, WebView2. |
| `Sentinel.wixproj` | MSBuild project, for building from Visual Studio or `dotnet build`. |
| `build.ps1` | The command-line build: cargo → sign → wix → sign. |
| `redist/` | Third-party payloads. **Not committed**; see [WebView2](#webview2) below. |

---

## Prerequisites

Build on Windows. The MSI cannot be produced on Linux — WiX v4 targets .NET and the
binaries must come from the MSVC toolchain, not mingw.

```powershell
# Rust, MSVC target
rustup target add x86_64-pc-windows-msvc

# WiX v4
dotnet tool install --global wix --version 4.0.5
wix extension add -g WixToolset.Util.wixext/4.0.5
wix extension add -g WixToolset.UI.wixext/4.0.5
```

You also need the widget bundle built (`web/widget`), and the WebView2 Evergreen
bootstrapper in `redist/`.

## Building

```powershell
.\build.ps1 -Version 0.1.0 `
            -WebDir ..\..\web\widget\dist `
            -SignToolPath "C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\signtool.exe" `
            -CertificateThumbprint <sha1 of the EV cert>
```

Omitting the signing parameters produces an unsigned MSI. That is fine for a local
smoke test and is not fine for anything a customer receives — see
[Signing](#signing).

The build enables the `sqlcipher` feature. Without it the spool is plain SQLite and
call audio sits unencrypted on the endpoint, which fails spec 6.5 and 12.3.

## Signing

Spec 12.1 requires an EV certificate on **all binaries and the MSI**. `build.ps1`
signs in two passes, and the order matters: an MSI's signature covers the package
file, not the files inside it, so signing only the MSI leaves two unsigned
executables on disk. Both passes use SHA-256 file digests and an RFC 3161 timestamp;
without the timestamp every signature expires with the certificate.

An EV certificate lives on a hardware token or in a cloud HSM. On a build agent that
means either an attended build with the token present, or a KSP-backed signing
service (Azure Trusted Signing, DigiCert KeyLocker). `-CertificateThumbprint` works
for both — the thumbprint selects the certificate; where the private key lives is the
KSP's problem.

Verify before shipping:

```powershell
Get-AuthenticodeSignature .\out\Sentinel-0.1.0-x64.msi | Format-List
```

## Deployment

### Intune (Win32 app or LOB app)

```
msiexec /i Sentinel-0.1.0-x64.msi /qn /l*v %TEMP%\sentinel-install.log ^
        ENROLLMENTTOKEN=<single-use token from the portal> ^
        APIBASEURL=https://api.sentinel.magickvoice.com ^
        TENANTHINT=<identity platform tenant id>
```

`ENROLLMENTTOKEN` is single-use and expires after 24 hours (spec 7.2). It is marked
`Secure` so it survives elevation, and listed in `MsiHiddenProperties` so it does not
appear in the verbose log an administrator will attach to a support ticket. Mint one
token per deployment wave, not one per fleet.

### Detection rule

`HKLM\SOFTWARE\MagickVoice\Sentinel\InstalledVersion` — a string equal to the
deployed version.

### Uninstall

```
msiexec /x {product code} /qn
```

The uninstall stops and removes the service and removes
`%PROGRAMDATA%\MagickVoice\Sentinel` **only if it is empty**. A machine that still
holds unacked audio keeps its spool: an uninstall destroying evidence nobody asked it
to destroy is the wrong default for a compliance product.

So: flush the spool before decommissioning a machine, confirm the directory is empty,
and only then uninstall. If the machine is being wiped anyway, the encrypted spool
going with it is fine — but the calls in it were never delivered, and the coverage
report will show the gap.

---

## The tier gate

Spec section 3: tier C is Windows 8.1, Windows 10 before 1903 (build 18362), x86, and
ARM64, and the installer MUST block all of them. The package does this four ways:

| Tier C case | How it is blocked |
|---|---|
| x86 | The package is x64. Windows refuses it with its own message before any condition runs, and `VersionNT64` is asserted as a backstop. |
| ARM64 | `PROCESSOR_ARCHITECTURE` registry search plus a launch condition. ARM64 runs x64 binaries under emulation, so the package would otherwise install and never capture. |
| Windows 8.1 | `kernel32.dll` version is 6.3.9600.x, below the 10.0.18362 minimum. |
| Windows 10 < 1903 | `kernel32.dll` version is below 10.0.18362. |

**Why a file-version search and not a registry read.** This is the part of the package
most likely to be "simplified" by someone who has not hit the failure modes:

- `VersionNT` is useless. Windows 10 and 11 both report `603` to an unmanifested MSI,
  so any condition written against it passes on Windows 8.1 too.
- `CurrentBuildNumber` is a `REG_SZ`. MSI compares a string property against an
  unquoted integer as *always false*, and against a quoted one *lexically* — under
  which `"9600" >= "18362"` is TRUE. A registry-based build check is either dead or
  backwards, and both failure modes are silent.
- `FileSearch/@MinVersion` performs a real four-part numeric comparison against the
  file's version resource. On Windows 10 and 11, `kernel32.dll` is versioned
  `10.0.<build>.<ubr>`, so this is a direct and correct build-number test.

`client/sentinel-capture/src/tier.rs` is the authority for the support matrix. The
thresholds here mirror it: **22000** for a Windows 11 client, **20348** for Server
2022, **18362** for the tier B floor. 20348 is a *server* build number — using it as a
client threshold reports every Windows 10 desktop on the floor as tier A, and process
loopback then fails to activate on all of them.

### Acceptance test

Run before every release, on a Windows 8.1 or pre-1903 Windows 10 VM:

```
msiexec /i Sentinel-x.y.z-x64.msi /qn /l*v tierc.log
```

The install must fail with the tier message and roll back cleanly. A tier C install
that succeeds produces a service that runs, reports healthy, and never captures a
call — which is worse than no install at all, because the fleet view says the machine
is covered.

---

## WebView2

Windows 11 ships the runtime; Windows 10 does not (spec section 3). The package
carries the Evergreen bootstrapper and runs it during install when the runtime is
absent.

Fetch it once and commit its hash to the release record — deliberately **not**
downloaded by `build.ps1`, because a build that pulls an executable off the internet
and packages it unverified is the supply-chain problem the bank's security review
exists to find:

```powershell
mkdir redist
# from https://developer.microsoft.com/microsoft-edge/webview2/
Get-AuthenticodeSignature .\redist\MicrosoftEdgeWebview2Setup.exe | Format-List
Get-FileHash .\redist\MicrosoftEdgeWebview2Setup.exe -Algorithm SHA256
```

### OPEN-5 — air-gapped install path is NOT DECIDED

> **Placeholder. Do not resolve this by choosing one.**

The Evergreen bootstrapper downloads the runtime, so it fails on a floor with no
egress. The open decision (spec 17, OPEN-5) is whether to bundle the ~150 MB
fixed-version runtime instead, or to require egress to Microsoft's CDN.

What the package does *today*: installs the Evergreen bootstrapper with
`Return="ignore"`. On an air-gapped machine the bootstrapper fails, the install
continues, capture works, and the widget does not render. That is the least-bad
interim behaviour — failing the whole install would leave the floor with no capture at
all, which for a compliance product is much worse than capture with no widget — but it
is interim, not an answer.

When OPEN-5 is decided:

- **(a) Evergreen accepted** — delete this section, and document the egress
  requirement (`*.msedge.net`, `msedge.api.cdp.microsoft.com`) in the customer
  deployment guide.
- **(b) Fixed-version required** — add the runtime CAB as a payload, extract it under
  `[INSTALLFOLDER]\WebView2`, and set `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` for the
  agent process. Note that fixed-version runtimes are not serviced automatically, so
  this also creates an update obligation the Evergreen path does not have.

---

## The data directory ACL

`%PROGRAMDATA%\MagickVoice\Sentinel` holds the SQLCipher spool, the device
certificate, staged updates and crash dumps. The ACL is set explicitly rather than
inherited:

| Principal | Rights | Why |
|---|---|---|
| `SYSTEM` | Full control | The service manages the spool, stages updates, ships dumps. |
| `Administrators` | Full control | Support and uninstall. |
| `Users` | Modify (inheritable) | `SentinelAgent.exe` runs as the signed-in user and **writes** the spool. |
| `Users` on `device\` | Read only | Machine identity, renewed by the service. The private key is not on disk at all — it is generated non-exportably in CNG. |
| `Users` on `staging\` | None | A directory a user can write and LocalSystem later executes from is a local privilege escalation. |

Two principals share this directory — the user-session agent writes it, the SYSTEM
service reads and manages it — which is exactly why the spool key is DPAPI **machine**
scope (spec 6.5) rather than user scope.

Modify for `Users` does mean an agent can delete their own spooled audio. That is a
deliberate trade, and it is spec 6.8's: *detect tampering and report it; do not fight
EDR or the user.* Deleted audio shows up as a coverage gap against the dialer CDR,
which turns an arms race into a management conversation. The alternative — having the
SYSTEM service own the spool — would mean piping every audio segment through the named
pipe into session 0, which is a great deal of machinery to make one deletion slightly
harder.

The permissions are marked inheritable so the spool database, its WAL file and staged
files inherit them. Without that they get the default `%PROGRAMDATA%` ACL, under which
a user may create a file but not modify one another user created — which, across a
shift change, is precisely the spool.

---

## What the package does not do

- **No `Run` key for the agent.** The service launches it on
  `WTS_SESSION_LOGON` (spec 6.1). A `Run` key is removed by the first agent who looks
  for it.
- **No UI sequence.** This package is deployed silently by SCCM or Intune. A UI would
  be dead weight and one more thing to localise.
- **No reboot.** Nothing here requires one. If `msiexec` schedules a reboot it is
  because a file was in use — investigate rather than accepting it; on a collections
  floor a reboot means an agent loses a call.
- **No EDR exclusions.** Those are configured in the customer's AV console, not by us.
  Start that conversation in Phase 0: the agent's behaviour — audio capture, network
  upload, a hidden window — looks exactly like a keylogger to any competent endpoint
  protection product (spec 18).
