[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [switch]$ConfirmBusOffFaultInjection,
    [ValidateRange(1, 4)]
    [int]$Channel = 2,
    [string]$FaultBaud = '250K',
    [string]$Dbc = 'C:\Users\XCHARGE-2026Q1-LT08\Desktop\EU_HVBOXCheck\HVBoxCheck_EU.dbc',
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if (-not $ConfirmBusOffFaultInjection) {
    throw 'Pass -ConfirmBusOffFaultInjection; this gate deliberately creates CAN bus errors.'
}
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$executable = Join-Path $root 'target\debug\pcanwork.exe'
$settingsPath = Join-Path $env:LOCALAPPDATA 'PcanWork\pcanwork_settings.json'
$evidence = Join-Path $root ('artifacts\bus-off\' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $evidence | Out-Null
$backup = Join-Path $evidence 'pcanwork_settings.original.json'
Copy-Item -LiteralPath $settingsPath -Destination $backup

if (-not $SkipBuild) {
    $env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
    $env:CARGO_TARGET_DIR = Join-Path $root 'target'
    $env:CARGO_BUILD_JOBS = '4'
    & cargo build --locked -p pcanwork --jobs 4
    if ($LASTEXITCODE -ne 0) { throw 'Latest Debug fault-gate build failed.' }
}

function Start-IpcApplication([string]$Name) {
    $ipc = Join-Path $evidence "$Name-ipc.txt"
    Remove-Item -LiteralPath $ipc -ErrorAction SilentlyContinue
    $env:PCANWORK_IPC_INFO_FILE = $ipc
    $process = Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
    for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipc); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipc)) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "PcanWork $Name IPC startup timed out."
    }
    [pscustomobject]@{ Process = $process; Connection = @(Get-Content -LiteralPath $ipc) }
}

$faultProcess = $null
$recoveryProcess = $null
try {
    $settings = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json
    $target = @($settings.channels | Where-Object { [int]$_.sw_channel -eq $Channel })
    if ($target.Count -ne 1) { throw "CAN$Channel is not uniquely configured." }
    if ([string]$target[0].device_type -notlike 'USBCAN*') {
        throw "CAN$Channel must be a ZLG USBCAN channel for this gate."
    }
    $normalBaud = [string]$target[0].baud
    if ($normalBaud -eq $FaultBaud) { throw 'Fault baud must differ from the normal baud.' }
    $target[0].baud = $FaultBaud
    $settings | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $settingsPath -Encoding utf8

    $faultProcess = Start-IpcApplication 'fault'
    python (Join-Path $PSScriptRoot 'bus_off_probe.py') `
        --port ([int]$faultProcess.Connection[0]) --token $faultProcess.Connection[1] `
        --channel $Channel --report (Join-Path $evidence 'fault.json')
    if ($LASTEXITCODE -ne 0) { throw 'Bus-Off reporting gate failed.' }
    Stop-Process -Id $faultProcess.Process.Id -Force
    $faultProcess.Process.WaitForExit()
    $faultProcess = $null

    Copy-Item -LiteralPath $backup -Destination $settingsPath -Force
    Start-Sleep -Seconds 2
    $recoveryProcess = Start-IpcApplication 'recovery'
    python (Join-Path $PSScriptRoot 'four_channel_gate.py') `
        --port ([int]$recoveryProcess.Connection[0]) --token $recoveryProcess.Connection[1] `
        --dbc $Dbc --report (Join-Path $evidence 'recovery.json')
    if ($LASTEXITCODE -ne 0) { throw 'Four-channel recovery after Bus-Off failed.' }
}
finally {
    Copy-Item -LiteralPath $backup -Destination $settingsPath -Force
    foreach ($candidate in @($faultProcess, $recoveryProcess)) {
        if ($candidate -and -not $candidate.Process.HasExited) {
            Stop-Process -Id $candidate.Process.Id -Force -ErrorAction SilentlyContinue
        }
    }
    Remove-Item Env:PCANWORK_IPC_INFO_FILE -ErrorAction SilentlyContinue
}

[ordered]@{
    passed = $true
    completed_at = (Get-Date).ToString('o')
    channel = $Channel
    fault_baud = $FaultBaud
    evidence = $evidence
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidence 'summary.json') -Encoding utf8
Write-Host "Bus-Off recovery gate passed: $evidence"
