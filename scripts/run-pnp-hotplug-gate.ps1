[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [switch]$ConfirmPnpDeviceCycle,
    [string]$InstanceIdPattern = 'USB\VID_0471&PID_1260*',
    [string]$Dbc = 'C:\Users\XCHARGE-2026Q1-LT08\Desktop\EU_HVBOXCheck\HVBoxCheck_EU.dbc'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The PnP hotplug gate must run in an elevated PowerShell process.'
}
if (-not $ConfirmPnpDeviceCycle) {
    throw 'Pass -ConfirmPnpDeviceCycle; this gate temporarily disables a specific USB device.'
}
$matches = @(Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like $InstanceIdPattern })
if ($matches.Count -ne 1) {
    throw "Expected exactly one present PnP device matching $InstanceIdPattern; found $($matches.Count)."
}
$device = $matches[0]
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$executable = Join-Path $root 'target\debug\pcanwork.exe'
$evidence = Join-Path $root ('artifacts\pnp-hotplug\' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $evidence | Out-Null
$ipc = Join-Path $evidence 'ipc.txt'
$env:PCANWORK_IPC_INFO_FILE = $ipc
$process = Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
$disabled = $false
try {
    for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipc); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipc)) { throw 'PcanWork IPC startup timed out.' }
    $connection = @(Get-Content -LiteralPath $ipc)
    python (Join-Path $PSScriptRoot 'prepare_abnormal_exit.py') `
        --port ([int]$connection[0]) --token $connection[1] `
        --report (Join-Path $evidence 'before-disable.json')
    if ($LASTEXITCODE -ne 0) { throw 'Unable to prepare four-channel hotplug state.' }

    Disable-PnpDevice -InstanceId $device.InstanceId -Confirm:$false
    $disabled = $true
    python (Join-Path $PSScriptRoot 'verify_hotplug_disconnect.py') `
        --port ([int]$connection[0]) --token $connection[1] `
        --report (Join-Path $evidence 'disconnect.json')
    if ($LASTEXITCODE -ne 0) { throw 'PcanWork did not report the disabled USB adapter.' }

    Enable-PnpDevice -InstanceId $device.InstanceId -Confirm:$false
    $disabled = $false
    Start-Sleep -Seconds 3
    python (Join-Path $PSScriptRoot 'four_channel_gate.py') `
        --port ([int]$connection[0]) --token $connection[1] `
        --dbc $Dbc --report (Join-Path $evidence 'recovery.json')
    if ($LASTEXITCODE -ne 0) { throw 'Four-channel recovery after PnP hotplug failed.' }
}
finally {
    if ($disabled) {
        Enable-PnpDevice -InstanceId $device.InstanceId -Confirm:$false -ErrorAction Continue
    }
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    Remove-Item Env:PCANWORK_IPC_INFO_FILE -ErrorAction SilentlyContinue
}

[ordered]@{
    passed = $true
    completed_at = (Get-Date).ToString('o')
    device = $device.FriendlyName
    instance_id = $device.InstanceId
    evidence = $evidence
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidence 'summary.json') -Encoding utf8
Write-Host "PnP hotplug gate passed: $evidence"
