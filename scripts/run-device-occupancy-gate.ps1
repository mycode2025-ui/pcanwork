param(
    [string]$Report = "D:\_Xcharge\Pcanwork\artifacts\hardware-occupancy\report.json"
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$directory = Split-Path -Parent $Report
New-Item -ItemType Directory -Force -Path $directory | Out-Null
$ipcInfo = Join-Path $directory 'ipc.txt'
if (Test-Path -LiteralPath $ipcInfo) { Remove-Item -LiteralPath $ipcInfo -Force }
$env:PCANWORK_IPC_INFO_FILE = $ipcInfo
$application = Start-Process -FilePath (Join-Path $root 'target\debug\pcanwork.exe') `
    -PassThru -WindowStyle Hidden
try {
    for ($attempt = 0; $attempt -lt 150 -and -not (Test-Path -LiteralPath $ipcInfo); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipcInfo)) { throw 'Second-process IPC startup timed out' }
    $connection = Get-Content -LiteralPath $ipcInfo
    python (Join-Path $root 'scripts\device_occupancy_gate.py') `
        --port ([int]$connection[0]) --token $connection[1] --report $Report
    exit $LASTEXITCODE
}
finally {
    if ($application -and -not $application.HasExited) {
        Stop-Process -Id $application.Id -Force
    }
}
