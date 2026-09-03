<#
.SYNOPSIS
    Turns web/widget/dist into the WebDir layout Sentinel.wxs packages, and refuses
    to produce a package whose widget would render blank on every desktop.

.DESCRIPTION
    Two facts that do not currently line up, both owned by other work streams:

      * `web/widget/vite.config.ts` builds a normal vite application: dist/index.html
        plus dist/assets/*.js and *.css, with `base: './'` so the asset URLs are
        relative. That is the correct vite configuration for a bundle WebView2 loads
        from disk.
      * `client/installer/Sentinel.wxs` packages exactly ONE file from that
        directory -- `<File Name="widget.html" Source="$(var.WebDir)\widget.html">`
        -- and no assets directory. `build.ps1` checks for `widget.html` and nothing
        else.

    So the MSI expects a single self-contained HTML file under a different name from
    the one vite emits. This script bridges the name, and it refuses to bridge the
    difference in shape.

    WHY THE REFUSAL IS A HARD FAILURE. If the bundle references external assets and
    the package ships only widget.html, then on every installed machine WebView2
    loads an HTML file whose scripts 404. The result is not an install failure and
    not a crash: the service installs, starts, detects its tier, and reports healthy
    in every heartbeat -- while the agent's widget is a blank rectangle and the
    non-dismissible recording indicator the compliance requirement is built around is
    not on screen. That is the product's dangerous failure mode (looks fine, is not
    fine) reached by way of a packaging detail, and it would be found by an agent on a
    collections floor rather than by us.

    So: assets present and not inlined => throw. The fix belongs upstream and there
    are two of them, in preference order:

      (a) Make the widget build emit one self-contained file. In vite that is
          `build.assetsInlineLimit` raised past the bundle size, or a single-file
          plugin, with the output named widget.html. Best: it matches what the WXS
          already says, and a single file has no relative-path ambiguity under the
          WebView2 virtual host mapping.
      (b) Add the assets directory to Sentinel.wxs as its own component, with the
          component-rule bookkeeping that implies.

    -AllowIncompleteBundle exists for a local or CI smoke test of the packaging
    pipeline itself, and stamps the fact into the staging manifest so it cannot be
    mistaken for a shippable build later.

.PARAMETER DistDir
    The vite output directory. Defaults to <repo>/web/widget/dist.

.PARAMETER StageDir
    Where to write the WebDir passed to build.ps1. Defaults to
    <repo>/client/installer/obj/webdir, which is already gitignored by
    client/installer/.gitignore.

.PARAMETER AllowIncompleteBundle
    Stage anyway when the bundle is not self-contained. Smoke tests only.
#>
[CmdletBinding()]
param(
    [string] $DistDir,
    [string] $StageDir,
    [switch] $AllowIncompleteBundle
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = Resolve-Path (Join-Path $PSScriptRoot "..\..")
if (-not $DistDir)  { $DistDir  = Join-Path $repoRoot "web\widget\dist" }
if (-not $StageDir) { $StageDir = Join-Path $repoRoot "client\installer\obj\webdir" }

if (-not (Test-Path $DistDir)) {
    throw "The widget bundle was not found at $DistDir. Run 'npm ci && npm run build' in web/ first."
}

$index = Join-Path $DistDir "index.html"
if (-not (Test-Path $index)) {
    throw "No index.html in $DistDir. vite did not produce a bundle; check the build log rather than staging an empty WebDir."
}

# Anything vite emitted that is not the entry document. `-Recurse` because the
# assets directory is the usual case but not the only one -- a public/ directory
# copied verbatim lands at the top level.
# @() around the pipeline so a single result is still an array with a .Count. A
# `Where-Object` that matches exactly one file otherwise yields a scalar, and the
# self-containment test below would then be reading .Count off a FileInfo.
$extras = @(Get-ChildItem -Path $DistDir -Recurse -File |
    Where-Object { $_.FullName -ne (Resolve-Path $index).Path })

# Source maps are not a packaging problem: nothing loads them at runtime, and they
# are deliberately on (`sourcemap: true`) so a widget stack trace from a floor is
# readable. They are excluded from the self-containment test and from the staged
# output, because shipping them would put the widget's source on every desktop.
$loadBearing = @($extras | Where-Object { $_.Extension -ne ".map" })

if ($loadBearing.Count -gt 0 -and -not $AllowIncompleteBundle) {
    $list = ($loadBearing | ForEach-Object { "  " + $_.FullName.Substring($DistDir.Length + 1) }) -join "`n"
    throw @"
The widget bundle is not self-contained, and Sentinel.wxs packages only widget.html.

Files vite emitted that the MSI would leave behind:

$list

Packaging this would install a widget.html whose scripts 404 on every machine. The
service would still install, start, and report healthy in every heartbeat -- with no
widget and therefore no recording indicator on screen. That is worse than a failed
install, because the fleet view would say the machine is covered.

Fix it upstream, not here:
  (a) make web/widget emit one self-contained file (raise vite's
      build.assetsInlineLimit past the bundle size, or use a single-file plugin), or
  (b) add the assets directory to client/installer/Sentinel.wxs as a component.

Pass -AllowIncompleteBundle only for a smoke test of the packaging pipeline. The
staging manifest records that you did.
"@
}

if (Test-Path $StageDir) { Remove-Item -Recurse -Force $StageDir }
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

# The rename is the whole bridge: vite writes index.html, the WXS wants widget.html.
Copy-Item -Path $index -Destination (Join-Path $StageDir "widget.html")

if ($AllowIncompleteBundle -and $loadBearing.Count -gt 0) {
    # Copy the extras so a smoke-test install has some chance of rendering, even
    # though the MSI will not carry them. Keeps the failure mode visible: the staged
    # directory works, the installed one does not.
    foreach ($f in $loadBearing) {
        $rel = $f.FullName.Substring($DistDir.Length + 1)
        $dest = Join-Path $StageDir $rel
        New-Item -ItemType Directory -Force -Path (Split-Path -Parent $dest) | Out-Null
        Copy-Item -Path $f.FullName -Destination $dest
    }
}

$staged = @(Get-ChildItem -Path $StageDir -Recurse -File)
$manifest = [ordered]@{
    stage_dir                = $StageDir
    dist_dir                 = $DistDir
    self_contained           = ($loadBearing.Count -eq 0)
    allow_incomplete_bundle  = [bool] $AllowIncompleteBundle
    shippable                = (($loadBearing.Count -eq 0) -and -not $AllowIncompleteBundle)
    files                    = @($staged | ForEach-Object {
        [ordered]@{
            name   = $_.FullName.Substring($StageDir.Length + 1)
            bytes  = $_.Length
            sha256 = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    })
}
$manifestPath = Join-Path $StageDir "widget-payload.json"
$manifest | ConvertTo-Json -Depth 5 | Set-Content -Path $manifestPath -Encoding utf8

Write-Host "widget: staged $($staged.Count) file(s) into $StageDir" -ForegroundColor Cyan
if (-not $manifest.shippable) {
    Write-Warning "This WebDir is NOT shippable: see $manifestPath."
}
Write-Output $StageDir
