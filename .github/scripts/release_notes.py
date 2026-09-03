#!/usr/bin/env python3
"""Compose the release notes from the payload manifest the Windows build produced.

Why this is a script and not a heredoc in the workflow: the notes have to carry
specific facts about the artefact, and every one of them is something a person will
later be asked to prove. The three that matter most, and why:

* **The MSI's SHA-256.** The identity of the download. A customer's IT lead who is
  handed a file over email needs to be able to tell whether it is the one we
  published.

* **The WebView2 bootstrapper's SHA-256.** `client/installer/redist/README.md` asks
  for exactly this: "Record the SHA-256 in the release notes so the exact payload
  that shipped can be identified later." It is the one payload in the package that we
  did not build, and the only way to answer "which bootstrapper did that machine
  run?" after the fact.

* **The build features.** `--features sentinel-core/sqlcipher` is the difference
  between an encrypted and an unencrypted spool on the endpoint, and
  `docs/security.md` calls it "the most checkable gap in this document": *"Anyone
  reviewing the encryption-at-rest claim should confirm that the shipped binary was
  built with the feature enabled, because nothing in the source tree guarantees it."*
  Printing the feature list in the release notes is how that becomes checkable
  without a reviewer having to trust us.

The notes also restate the tier-C acceptance requirement and the open decisions this
artefact carries, because a release record that says which questions were still open
when it shipped is how a pilot finding gets traced back to an assumption rather than
to a bug.
"""

from __future__ import annotations

import argparse
import json
import pathlib


def _sig_line(p: dict) -> str:
    sig = p.get("signature") or "unknown"
    stamp = " (timestamped)" if p.get("timestamped") else " (NOT timestamped)"
    return f"`{p['sha256']}`  {p['name']} — {sig}{stamp}"


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--manifest", required=True, type=pathlib.Path)
    ap.add_argument("--out", required=True, type=pathlib.Path)
    args = ap.parse_args()

    m = json.loads(args.manifest.read_text(encoding="utf-8"))
    pay = m["payloads"]
    tc = m["toolchain"]

    lines: list[str] = []
    add = lines.append

    add(f"# MagickVoice Sentinel {m['version']}")
    add("")
    add(f"Built from `{m['commit']}` at {m['built_at_utc']} by "
        f"[this workflow run]({m['workflow_run']}).")
    add("")

    if not m.get("shippable", False):
        # Loud, first, and above the download. An artefact that is not shippable must
        # say so before anyone scrolls to the asset list.
        add("> **THIS BUILD IS NOT SHIPPABLE.** "
            "See the payload manifest for which check failed — an unsigned package, or a "
            "widget bundle that is not self-contained. Do not install it on a customer, "
            "pilot or demo machine.")
        add("")

    # ---------------------------------------------------------------- the download
    add("## Package")
    add("")
    add("| | |")
    add("|---|---|")
    add(f"| File | `{pay['msi']['name']}` |")
    add(f"| SHA-256 | `{pay['msi']['sha256']}` |")
    add(f"| Size | {pay['msi']['bytes']:,} bytes |")
    add(f"| Authenticode | {pay['msi']['signature']}"
        f"{' , RFC 3161 timestamped' if pay['msi']['timestamped'] else ' , **no timestamp**'} |")
    if pay["msi"].get("signer"):
        add(f"| Signer | `{pay['msi']['signer']}` |")
    add(f"| Signing provider | {m['signing_provider']} |")
    add("")
    add("Verify before deploying:")
    add("")
    add("```powershell")
    add(f"Get-FileHash .\\{pay['msi']['name']} -Algorithm SHA256")
    add(f"Get-AuthenticodeSignature .\\{pay['msi']['name']} | Format-List")
    add("```")
    add("")

    # -------------------------------------------------------- what is in the package
    add("## Payload record")
    add("")
    add("Every file the package installs, with the hash it was installed from. "
        "`payload-manifest.json` is attached to this release and carries the same data "
        "in machine-readable form.")
    add("")
    add("**Executables** (signed separately from the MSI: an MSI's signature covers the "
        "package file, not the files inside it, so signing only the package would leave "
        "these unsigned on disk)")
    add("")
    for b in pay["binaries"]:
        add(f"- {_sig_line(b)}")
    add("")

    add("**Widget bundle**")
    add("")
    for f in pay["widget"]["files"]:
        add(f"- `{f['sha256']}`  {f['name']}")
    if not pay["widget"].get("self_contained", True):
        add("")
        add("> The bundle is **not self-contained** and the package carries only "
            "`widget.html`. On an installed machine the widget's scripts will 404: the "
            "service installs, starts and reports healthy, and the agent sees a blank "
            "widget with no recording indicator. See `.github/scripts/stage-widget.ps1`.")
    add("")

    wv = pay["webview2"]
    add("**WebView2 Evergreen bootstrapper** — the one payload we did not build. "
        "`client/installer/redist/README.md` asks for this hash to be recorded here so "
        "the exact payload that shipped can be identified later.")
    add("")
    add(f"- `{wv['sha256']}`  {wv['payload']} ({wv['size_bytes']:,} bytes)")
    add(f"- Fetched from {wv['source_url']} at {wv['fetched_at_utc']}")
    add(f"- Authenticode: signed by `{wv['signer_subject']}` "
        f"(thumbprint `{wv['signer_thumbprint']}`), verified Valid before packaging")
    add("")

    # ---------------------------------------------------------------- how it was built
    add("## Build configuration")
    add("")
    add("| | |")
    add("|---|---|")
    add(f"| Target | `{tc['target']}` |")
    add(f"| Profile | `{tc['profile']}` |")
    add(f"| Cargo features | `{', '.join(tc['features'])}` |")
    add(f"| rustc | {tc['rustc']} |")
    add(f"| WiX | {tc['wix']} |")
    add(f"| Node | {tc['node']} |")
    add(f"| Runner image | {m['runner_image']} |")
    add("")
    add("`sentinel-core/sqlcipher` is the feature that matters to a reviewer. Without it "
        "the endpoint spool is plain SQLite and call audio sits unencrypted on the agent's "
        "desktop (spec 6.5 and 12.3). `docs/security.md` notes that nothing in the source "
        "tree guarantees the shipped binary had it enabled — this line, and the CI job "
        "that runs the test suite in the same configuration, are that guarantee.")
    add("")
    add("CycloneDX SBOMs for the Rust and npm dependency graphs are attached. Build "
        "provenance is attested against this repository and is verifiable with "
        "`gh attestation verify`.")
    add("")

    # -------------------------------------------------------------------- the gates
    add("## Before this is given to anyone")
    add("")
    add("**Tier-C acceptance.** Run `.github/scripts/tier-c-acceptance.ps1` against this "
        "exact MSI on a Windows 8.1 or a genuine pre-1903 Windows 10 VM. The install must "
        "fail with the tier message and roll back cleanly. This is not input validation: a "
        "tier C install that succeeds produces a service that runs, reports healthy in "
        "every heartbeat, and never captures a call — while the fleet view says the machine "
        "is covered. Attach the result JSON to this release.")
    add("")
    add("**EDR allowlisting.** The agent's behaviour is, feature for feature, the signature "
        "of a keylogger with audio capture. Signing helps with reputation heuristics and "
        "does not prevent a behavioural detection. Two to six weeks of calendar time, per "
        "`docs/deployment.md`; it cannot start when the pilot does.")
    add("")
    add("**Deployment.** SCCM/Intune, silent, per-machine x64:")
    add("")
    add("```")
    add(f"msiexec /i {pay['msi']['name']} /qn /l*v %TEMP%\\sentinel-install.log ^")
    add("        ENROLLMENTTOKEN=<single-use token from the portal> ^")
    add("        APIBASEURL=https://api.sentinel.magickvoice.com ^")
    add("        TENANTHINT=<identity platform tenant id>")
    add("```")
    add("")
    add("`ENROLLMENTTOKEN` is single-use and expires after 24 hours. Mint one per "
        "deployment wave, not one per fleet.")
    add("")

    # ------------------------------------------------------------- open decisions
    add("## Open decisions this build carries")
    add("")
    add("Recorded, not resolved. See `docs/open-decisions.md`.")
    add("")
    for k, v in m.get("open_decisions", {}).items():
        add(f"- **{k}** — {v}")
    add("")

    args.out.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
