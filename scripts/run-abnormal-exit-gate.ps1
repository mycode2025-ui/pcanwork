[CmdletBinding()]
param(
    [string]$Dbc = 'C:\Users\XCHARGE-2026Q1-LT08\Desktop\EU_HVBOXCheck\HVBoxCheck_EU.dbc'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$executable = Join-Path $root 'target\debug\pcanwork.exe'
$evidence = Join-Path $root (
    'artifacts\abnormal-exit\' + (Get-Date -Format 'yyyyMMdd-HHmmss')
)
New-Item -ItemType Directory -Force -Path $evidence | Out-Null

function Start-IpcApplication([string]$InfoName) {
    $ipc = Join-Path $evidence $InfoName
    Remove-Item -LiteralPath $ipc -ErrorAction SilentlyContinue
    $env:PCANWORK_IPC_INFO_FILE = $ipc
    $process = Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
    for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipc); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipc)) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw 'PcanWork IPC startup timed out.'
    }
    [pscustomobject]@{ Process = $process; Connection = @(Get-Content -LiteralPath $ipc) }
}

$first = Start-IpcApplication 'first-ipc.txt'
try {
    python (Join-Path $PSScriptRoot 'prepare_abnormal_exit.py') `
        --port ([int]$first.Connection[0]) --token $first.Connection[1] `
        --report (Join-Path $evidence 'before-kill.json')
    if ($LASTEXITCODE -ne 0) { throw 'Unable to prepare the abnormal-exit state.' }
}
finally {
    if (-not $first.Process.HasExited) {
        Stop-Process -Id $first.Process.Id -Force
        $first.Process.WaitForExit()
    }
}

Start-Sleep -Seconds 2
$second = Start-IpcApplication 'second-ipc.txt'
try {
    python (Join-Path $PSScriptRoot 'four_channel_gate.py') `
        --port ([int]$second.Connection[0]) --token $second.Connection[1] `
        --dbc $Dbc --report (Join-Path $evidence 'recovery.json')
    if ($LASTEXITCODE -ne 0) { throw 'Four-channel recovery after abnormal exit failed.' }
}
finally {
    if (-not $second.Process.HasExited) {
        Stop-Process -Id $second.Process.Id -Force
    }
    Remove-Item Env:PCANWORK_IPC_INFO_FILE -ErrorAction SilentlyContinue
}

[ordered]@{
    passed = $true
    completed_at = (Get-Date).ToString('o')
    forced_process_id = $first.Process.Id
    recovery_process_id = $second.Process.Id
    evidence = $evidence
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $evidence 'summary.json') -Encoding utf8
Write-Host "Abnormal-exit recovery gate passed: $evidence"
