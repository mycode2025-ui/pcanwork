param(
    [ValidateRange(0.01, 168.0)]
    [double]$Hours = 8.0,
    [string]$Report = ""
)

$ErrorActionPreference = "Stop"
$workspace = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$executable = Join-Path $workspace "target\debug\pcanwork.exe"
$stamp = Get-Date -Format "yyyyMMdd-HHmmss"
if (-not $Report) {
    $Report = Join-Path $workspace "artifacts\hardware-soak\$stamp\report.json"
}
$reportDirectory = Split-Path -Parent $Report
New-Item -ItemType Directory -Force -Path $reportDirectory | Out-Null
$ipcInfo = Join-Path $reportDirectory "ipc.txt"
$env:PCANWORK_IPC_INFO_FILE = $ipcInfo

$application = Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
try {
    for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipcInfo); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipcInfo)) {
        throw "PcanWork IPC startup timed out"
    }
    $connection = Get-Content -LiteralPath $ipcInfo
    python (Join-Path $workspace "scripts\hardware_soak_gate.py") `
        --port ([int]$connection[0]) `
        --token $connection[1] `
        --hours $Hours `
        --report $Report
    exit $LASTEXITCODE
}
finally {
    if ($application -and -not $application.HasExited) {
        Stop-Process -Id $application.Id -Force
    }
}
