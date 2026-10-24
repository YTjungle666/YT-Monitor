#Requires -RunAsAdministrator
<#
    ytmon-agent installer for Windows
    ---------------------------------
    1. copy ytmon-agent.exe to C:\Program Files\ytmon
    2. write agent.toml (kept if already present)
    3. add an outbound firewall rule (the agent never listens on a port)
    4. register and start the ytmon-agent service

    This file is deliberately ASCII-only: PowerShell 5.1 reads .ps1 files
    using the system ANSI codepage, so non-ASCII text corrupts parsing.

    Uninstall: powershell -ExecutionPolicy Bypass -File uninstall.ps1
#>

param(
    # Ingest bearer token. Pass it to rotate or set it; omit to keep whatever
    # the existing agent.toml already has.
    [string]$Token = '',
    # Ingest URL. Required: the package cannot know your monitor's host.
    [string]$ServerUrl = $env:YT_MONITOR_INGEST
)

$ErrorActionPreference = 'Stop'

$ServiceName = 'ytmon-agent'
$Dest        = Join-Path $env:ProgramFiles 'ytmon'
$Exe         = Join-Path $Dest 'ytmon-agent.exe'
$Conf        = Join-Path $Dest 'agent.toml'
$StateDir    = Join-Path $env:ProgramData 'ytmon'

if ([string]::IsNullOrWhiteSpace($ServerUrl)) {
    throw 'ServerUrl is empty. Pass -ServerUrl http://<monitor>:9180/ingest (or set YT_MONITOR_INGEST).'
}

Write-Host '== ytmon-agent install ==' -ForegroundColor Cyan

if (-not (Test-Path (Join-Path $PSScriptRoot 'ytmon-agent.exe'))) {
    throw 'ytmon-agent.exe not found next to this script. Extract the whole zip first.'
}

New-Item -ItemType Directory -Force -Path $Dest     | Out-Null
New-Item -ItemType Directory -Force -Path $StateDir | Out-Null

# A running service holds an exclusive lock on its exe, so the copy fails with
# "file is in use" unless the service is stopped first. This has to happen
# before Copy-Item, not in the service-recreation block further down.
$running = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($running -and $running.Status -ne 'Stopped') {
    Write-Host 'stopping service to release the binary'
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
}
# Stopping the service is not always enough: an install that was interrupted
# can leave a process the SCM no longer tracks, and that process still holds an
# exclusive lock on the exe.
Get-Process -Name 'ytmon-agent' -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "killing leftover process pid $($_.Id)"
    Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
}
# Wait for the handle to actually close before copying; a fixed sleep races a
# slow teardown, so probe until the file can be opened for writing.
for ($i = 0; $i -lt 10; $i++) {
    try {
        $probe = [IO.File]::Open($Exe, 'Open', 'Write', 'None')
        $probe.Close()
        break
    } catch {
        Start-Sleep -Milliseconds 500
    }
}

Copy-Item (Join-Path $PSScriptRoot 'ytmon-agent.exe') $Exe -Force
Write-Host "copied to $Exe"

if (-not (Test-Path $Conf)) {
    $stateEscaped = $StateDir -replace '\\', '\\'
    $content = @"
host = "$env:COMPUTERNAME"
ctid = 100
server = "$ServerUrl"
interval_secs = 60
state_dir = "$stateEscaped"
"@
    Set-Content -Path $Conf -Value $content -Encoding ASCII
    Write-Host "wrote $Conf (host=$env:COMPUTERNAME)"
} else {
    Write-Host "kept existing $Conf"
}

# --- ingest token ---
# Kept outside the "create once" block: rotating the token must not require
# deleting the config, and an empty -Token leaves the current value in place.
if ($Token) {
    $lines = @()
    if (Test-Path $Conf) {
        $lines = Get-Content $Conf | Where-Object { $_ -notmatch '^server_token' }
    }
    $lines += ('server_token = "' + $Token + '"')
    Set-Content -Path $Conf -Value $lines -Encoding ASCII
    Write-Host 'wrote ingest token'
}

# --- firewall: outbound only ---
if (-not (Get-NetFirewallRule -DisplayName 'ytmon-agent' -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -DisplayName 'ytmon-agent' `
        -Description 'ytmon agent outbound metrics reporting' `
        -Direction Outbound -Action Allow -Program $Exe -Profile Any | Out-Null
    Write-Host 'added outbound firewall rule: ytmon-agent'
} else {
    Write-Host 'firewall rule already present'
}

# --- service ---
# NOTE: do not use `& sc.exe create ... binPath= $path` here. PowerShell 5.1
# re-quotes arguments for native executables and does not escape inner double
# quotes, so a binPath containing '"C:\Program Files\..."' arrives malformed
# and sc.exe fails silently. New-Service goes through the .NET API and writes
# the string verbatim.
$existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($existing) {
    Write-Host 'removing existing service'
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1
    & sc.exe delete $ServiceName 2>&1 | Out-Null
    Start-Sleep -Seconds 2
}

$binPath = '"' + $Exe + '" --service --config "' + $Conf + '"'
Write-Host "binPath: $binPath"

try {
    New-Service -Name $ServiceName `
        -BinaryPathName $binPath `
        -DisplayName 'ytmon host metrics agent' `
        -Description 'Sends host health metrics to ytmon-server' `
        -StartupType Automatic -ErrorAction Stop | Out-Null
    Write-Host 'service created'
} catch {
    Write-Host "New-Service failed: $($_.Exception.Message)" -ForegroundColor Red
    throw
}

# restart-on-failure policy, best effort (values contain no spaces or quotes)
cmd /c "sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/5000/restart/5000" 2>&1 | Out-Null
Write-Host 'restart policy set'

try {
    Start-Service -Name $ServiceName -ErrorAction Stop
    Write-Host 'service started'
} catch {
    Write-Host "Start-Service failed: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host '---- last 20 log lines ----' -ForegroundColor Yellow
    $logFile = Join-Path $StateDir 'agent.log'
    if (Test-Path $logFile) { Get-Content $logFile -Tail 20 }
    exit 1
}

Start-Sleep -Seconds 3
$svc = Get-Service -Name $ServiceName
Write-Host ''
Write-Host ("service {0}: {1} / {2}" -f $svc.Name, $svc.Status, $svc.StartType) -ForegroundColor Green
Write-Host "log: $StateDir\agent.log"

if ($svc.Status -ne 'Running') {
    Write-Host 'service is not Running - check the log above and report back' -ForegroundColor Yellow
    exit 1
}
