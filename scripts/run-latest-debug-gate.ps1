param(
    [string]$Dbc = "C:\Users\XCHARGE-2026Q1-LT08\Desktop\EU_HVBOXCheck\HVBoxCheck_EU.dbc",
    [string]$Report = "D:\_Xcharge\Pcanwork\artifacts\four-channel-after-sim-worker.json"
)

$ErrorActionPreference = "Stop"
$workspace = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$target = Join-Path $workspace "target"
$executable = Join-Path $target "debug\pcanwork.exe"
$ipcInfo = Join-Path $workspace "artifacts\latest-debug-ipc.txt"

$env:CARGO_TARGET_DIR = $target
$env:CARGO_BUILD_JOBS = "4"
cargo build --bin pcanwork
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

if (Test-Path -LiteralPath $ipcInfo) {
    Remove-Item -LiteralPath $ipcInfo -Force
}
$env:PCANWORK_IPC_INFO_FILE = $ipcInfo
$process = Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
try {
    for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipcInfo); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipcInfo)) {
        throw "Debug IPC startup timed out"
    }
    $connection = Get-Content -LiteralPath $ipcInfo
    python (Join-Path $workspace "scripts\four_channel_gate.py") `
        --port ([int]$connection[0]) `
        --token $connection[1] `
        --dbc $Dbc `
        --report $Report
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
}
finally {
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force
    }
}
