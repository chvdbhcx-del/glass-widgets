# Deletes every target-* build dir next to Cargo.toml except target-agent.   tools\clean-targets.ps1 [-WhatIf]
[CmdletBinding(SupportsShouldProcess)]
param()
$root = Split-Path $PSScriptRoot
Get-ChildItem -Path $root -Directory -Filter 'target-*' | Where-Object Name -ne 'target-agent' | ForEach-Object {
    if ($PSCmdlet.ShouldProcess($_.FullName, 'Remove')) { Remove-Item -Recurse -Force $_.FullName }
}
