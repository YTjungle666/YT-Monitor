#Requires -RunAsAdministrator
# ytmon-agent uninstaller: stop+delete service, drop firewall rule, remove dirs.
# ASCII-only (see install.ps1 for why).

$ErrorActionPreference = 'Continue'
$ServiceName = 'ytmon-agent'
$Dest        = Join-Path $env:ProgramFiles 'ytmon'
$StateDir    = Join-Path $env:ProgramData 'ytmon'

Write-Host '== ytmon-agent uninstall ==' -ForegroundColor Cyan

$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc) {
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
    & sc.exe delete $ServiceName | Out-Null
    Write-Host 'service deleted'
} else {
    Write-Host 'service not present'
}

Get-NetFirewallRule -DisplayName 'ytmon-agent' -ErrorAction SilentlyContinue |
    Remove-NetFirewallRule -ErrorAction SilentlyContinue
Write-Host 'firewall rule removed'

if (Test-Path $Dest)     { Remove-Item $Dest -Recurse -Force; Write-Host "removed $Dest" }
if (Test-Path $StateDir) { Remove-Item $StateDir -Recurse -Force; Write-Host "removed $StateDir" }

Write-Host 'done' -ForegroundColor Green
