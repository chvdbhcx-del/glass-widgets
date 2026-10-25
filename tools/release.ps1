<#
  Builds the Glass Widgets installer and (unless -NoPublish) publishes a GitHub release.

    pwsh tools/release.ps1 -NoPublish            build exe + installer into dist\
    pwsh tools/release.ps1 -Notes "What is new"   build, tag v<version> (from Cargo.toml) and publish the installer

  Needs: Rust (MSVC), NSIS `makensis.exe` (on PATH, or the copy Tauri downloads to %LOCALAPPDATA%\tauri\NSIS), `gh` logged in (publishing).
  To ship a new version, bump `version` in Cargo.toml and add a CHANGELOG entry, commit and push first.
#>
param(
    [string]$Notes = '',
    [string]$Repo = 'chvdbhcx-del/glass-widgets',
    [switch]$NoPublish
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$ver = (Select-String -Path 'Cargo.toml' -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$nsis = (Get-Command makensis.exe -ErrorAction SilentlyContinue).Source
if (-not $nsis) { $nsis = Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe' }
if (-not (Test-Path $nsis)) { throw 'makensis.exe not found (install NSIS or put it on PATH)' }

Write-Host "Building Glass Widgets $ver ..."
cargo build --release
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

New-Item -ItemType Directory -Force dist | Out-Null
$out = Join-Path $root "dist\GlassWidgets_${ver}_x64-setup.exe"
& $nsis "/DVERSION=$ver" "/DEXEPATH=$root\target\release\mswidgets.exe" "/DOUTFILE=$out" (Join-Path $root 'installer\glass-widgets.nsi')
if ($LASTEXITCODE -ne 0 -or -not (Test-Path $out)) { throw 'makensis failed' }
Write-Host "Installer: $out"

if ($NoPublish) { Write-Host 'Not published (-NoPublish).'; return }

$tag = "v$ver"
gh release view $tag --repo $Repo *> $null
if ($LASTEXITCODE -eq 0) {
    Write-Host "Release $tag exists: replacing its file"
    gh release upload $tag $out --repo $Repo --clobber
} else {
    gh release create $tag $out --repo $Repo --title "Glass Widgets $ver" --notes $Notes
}
if ($LASTEXITCODE -ne 0) { throw 'gh release failed' }
Write-Host "Published: https://github.com/$Repo/releases/tag/$tag"
