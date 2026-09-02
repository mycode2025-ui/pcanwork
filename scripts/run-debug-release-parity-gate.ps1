[CmdletBinding()]
param(
    [string]$DebugExecutable = 'D:\_Xcharge\Pcanwork\target\debug\pcanwork.exe',
    [string]$ReleaseExecutable = 'C:\Program Files\PcanWork\pcanwork.exe',
    [string]$Dbc = 'C:\Users\XCHARGE-2026Q1-LT08\Desktop\EU_HVBOXCheck\HVBoxCheck_EU.dbc'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$debugPath = (Resolve-Path -LiteralPath $DebugExecutable).Path
$releasePath = (Resolve-Path -LiteralPath $ReleaseExecutable).Path
$evidence = Join-Path $root ('artifacts\debug-release-parity\' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
New-Item -ItemType Directory -Force -Path $evidence | Out-Null

function Invoke-FourChannelExecutable([string]$Name, [string]$Executable) {
    $ipc = Join-Path $evidence "$Name-ipc.txt"
    $report = Join-Path $evidence "$Name.json"
    $env:PCANWORK_IPC_INFO_FILE = $ipc
    $process = Start-Process -FilePath $Executable -PassThru -WindowStyle Hidden
    try {
        for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipc); $attempt++) {
            Start-Sleep -Milliseconds 100
        }
        if (-not (Test-Path -LiteralPath $ipc)) { throw "$Name IPC startup timed out." }
        $connection = @(Get-Content -LiteralPath $ipc)
        python (Join-Path $PSScriptRoot 'four_channel_gate.py') `
            --port ([int]$connection[0]) --token $connection[1] `
            --dbc $Dbc --report $report
        if ($LASTEXITCODE -ne 0) { throw "$Name four-channel gate failed." }
    }
    finally {
        if ($process -and -not $process.HasExited) {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        }
    }
    Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
}

try {
    $debug = Invoke-FourChannelExecutable 'debug' $debugPath
    Start-Sleep -Seconds 2
    $release = Invoke-FourChannelExecutable 'release' $releasePath
}
finally {
    Remove-Item Env:PCANWORK_IPC_INFO_FILE -ErrorAction SilentlyContinue
}

function Canonical([object]$Value) {
    $Value | ConvertTo-Json -Depth 10 -Compress
}
foreach ($comparison in @(
    @('matrix', $debug.matrix, $release.matrix),
    @('per-channel counters', $debug.pressure.per_channel, $release.pressure.per_channel),
    @('DBC diagnostics', $debug.dbc_diagnostics, $release.dbc_diagnostics)
)) {
    if ((Canonical $comparison[1]) -ne (Canonical $comparison[2])) {
        throw "Debug/Release mismatch in $($comparison[0])."
    }
}
foreach ($name in @('queued', 'tx', 'rx')) {
    if ($debug.pressure.$name -ne $release.pressure.$name) {
        throw "Debug/Release pressure counter mismatch: $name"
    }
}
foreach ($result in @($debug, $release)) {
    if (-not $result.passed) { throw 'A parity-side gate did not pass.' }
    foreach ($counter in @(
        'command_rejected', 'dropped_events', 'dropped_frames',
        'hardware_errors', 'hardware_overruns'
    )) {
        if ([int64]$result.pressure.health.$counter -ne 0) {
            throw "Parity-side health counter is non-zero: $counter"
        }
    }
}

[ordered]@{
    passed = $true
    completed_at = (Get-Date).ToString('o')
    debug_executable = $debugPath
    debug_sha256 = (Get-FileHash -LiteralPath $debugPath -Algorithm SHA256).Hash
    release_executable = $releasePath
    release_sha256 = (Get-FileHash -LiteralPath $releasePath -Algorithm SHA256).Hash
    matrix_links = @($debug.matrix).Count
    tx = $debug.pressure.tx
    rx = $debug.pressure.rx
    dbc_diagnostics = $debug.dbc_diagnostics
} | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $evidence 'summary.json') -Encoding utf8
Write-Host "Debug/Release parity gate passed: $evidence"
