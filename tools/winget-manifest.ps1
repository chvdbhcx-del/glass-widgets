<#
  Writes the three winget manifests (version, installer, defaultLocale; schema 1.6.0) for a released Glass Widgets.

    pwsh tools/winget-manifest.ps1                  # latest GitHub release
    pwsh tools/winget-manifest.ps1 -Version 0.2.1

  Reads the release asset GlassWidgets_<Version>_x64-setup.exe with `gh release view` (URL and SHA256 as GitHub reports them,
  nothing is downloaded or modified) and writes the files to dist\winget\<Version>\. Nothing is submitted anywhere: that is a manual
  pull request to https://github.com/microsoft/winget-pkgs (see README, "Install with winget").
  Needs the GitHub CLI (`gh`), logged in or not (public repository).
#>
param(
    [string]$Version,
    [string]$Repo = 'chvdbhcx-del/glass-widgets'
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

$tag = if ($Version) { "v$Version" } else { '' }
$rel = (gh release view $tag --repo $Repo --json tagName,publishedAt,assets) | ConvertFrom-Json
if (-not $Version) { $Version = $rel.tagName.TrimStart('v') }
if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "Version must look like 1.2.3, got '$Version'" }

$name = "GlassWidgets_${Version}_x64-setup.exe"
$asset = $rel.assets | Where-Object name -eq $name
if (-not $asset) { throw "Release $($rel.tagName) has no asset $name" }
$allowed = "https://github.com/$Repo/releases/"
if (-not $asset.url.StartsWith($allowed)) { throw "Unexpected asset URL $($asset.url)" }
if ($asset.digest -notmatch '^sha256:([0-9a-fA-F]{64})$') { throw "Asset has no sha256 digest (got '$($asset.digest)')" }
$sha = $Matches[1].ToUpperInvariant()
$date = ([datetime]$rel.publishedAt).ToUniversalTime().ToString('yyyy-MM-dd')

$id = 'chvdbhcx-del.GlassWidgets'
$schema = 'https://aka.ms/winget-manifest.{0}.1.6.0.schema.json'
$files = [ordered]@{
    "$id.yaml" = @"
# yaml-language-server: `$schema=$($schema -f 'version')
PackageIdentifier: $id
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
"@
    "$id.installer.yaml" = @"
# yaml-language-server: `$schema=$($schema -f 'installer')
PackageIdentifier: $id
PackageVersion: $Version
InstallerLocale: en-US
InstallerType: nullsoft
Scope: user
InstallModes:
- interactive
- silent
- silentWithProgress
InstallerSwitches:
  Silent: /S
  SilentWithProgress: /S
UpgradeBehavior: install
ReleaseDate: $date
Installers:
- Architecture: x64
  InstallerUrl: $($asset.url)
  InstallerSha256: $sha
ManifestType: installer
ManifestVersion: 1.6.0
"@
    "$id.locale.en-US.yaml" = @"
# yaml-language-server: `$schema=$($schema -f 'defaultLocale')
PackageIdentifier: $id
PackageVersion: $Version
PackageLocale: en-US
Publisher: chvdbhcx-del
PublisherUrl: https://github.com/chvdbhcx-del
PublisherSupportUrl: https://github.com/$Repo/issues
PackageName: Glass Widgets
PackageUrl: https://github.com/$Repo
License: MIT
LicenseUrl: https://github.com/$Repo/blob/main/LICENSE
ShortDescription: Native Liquid-Glass desktop widgets for Windows 10/11.
Description: Translucent, refracting widget cards that float over the wallpaper (an animated one too), drawn with Direct2D on the GPU. No WebView, no Electron, no telemetry; the network is used only by the widgets that need it.
Moniker: glasswidgets
Tags:
- widgets
- desktop
- glass
- clock
- weather
ReleaseNotesUrl: https://github.com/$Repo/releases/tag/v$Version
ManifestType: defaultLocale
ManifestVersion: 1.6.0
"@
}

$out = Join-Path $root "dist\winget\$Version"
New-Item -ItemType Directory -Force -Path $out | Out-Null
$utf8 = New-Object Text.UTF8Encoding($false)
foreach ($f in $files.GetEnumerator()) {
    [IO.File]::WriteAllText((Join-Path $out $f.Key), $f.Value.Replace("`r`n", "`n") + "`n", $utf8)
}
Write-Host "SHA256 $sha"
Write-Host "Manifests written to $out"
Write-Host "Check them with: winget validate --manifest `"$out`""
