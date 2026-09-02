param(
    [string]$ZlgType = "USBCAN-E-U",
    [string[]]$Rates = @("125K", "250K", "500K", "1M"),
    [string]$PcanRate = "",
    [switch]$Termination,
    [switch]$PcanFdApi,
    [string]$Executable = "",
    [ValidateRange(1, 1000)]
    [int]$Frames = 10,
    [string]$Report = ""
)

$ErrorActionPreference = "Stop"
$workspace = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
if (-not $Executable) {
    $Executable = Join-Path $workspace "target\debug\pcanwork.exe"
}
$executable = (Resolve-Path -LiteralPath $Executable).Path
if (-not $Report) {
    $stamp = Get-Date -Format "yyyyMMdd-HHmmss"
    $Report = Join-Path $workspace "artifacts\hardware-bitrate\$stamp\report.json"
}
$reportDirectory = Split-Path -Parent $Report
New-Item -ItemType Directory -Force -Path $reportDirectory | Out-Null
$ipcInfo = Join-Path $reportDirectory "ipc.txt"
Remove-Item -LiteralPath $ipcInfo -Force -ErrorAction SilentlyContinue
$env:PCANWORK_IPC_INFO_FILE = $ipcInfo

$application = Start-Process -FilePath $executable -PassThru -WindowStyle Hidden
try {
    for ($attempt = 0; $attempt -lt 200 -and -not (Test-Path -LiteralPath $ipcInfo); $attempt++) {
        Start-Sleep -Milliseconds 100
    }
    if (-not (Test-Path -LiteralPath $ipcInfo)) {
        throw "PcanWork IPC startup timed out"
    }
    $connection = Get-Content -LiteralPath $ipcInfo
    # The main window performs its initial hardware identity scan immediately
    # after IPC startup. Let the vendor Open/ReadBoardInfo/Close cycle finish
    # before opening the same ZLG device for traffic testing.
    Start-Sleep -Seconds 4
    $arguments = @(
        (Join-Path $workspace "scripts\two_channel_bitrate_gate.py"),
        "--port", ([int]$connection[0]),
        "--token", $connection[1],
        "--zlg-type", $ZlgType,
        "--rates"
    ) + $Rates + @("--frames", $Frames, "--report", $Report)
    if ($PcanRate) {
        $arguments += @("--pcan-rate", $PcanRate)
    }
    if ($Termination) {
        $arguments += "--termination"
    }
    if ($PcanFdApi) {
        $arguments += "--pcan-fd-api"
    }
    & python @arguments
    exit $LASTEXITCODE
}
finally {
    if ($application -and -not $application.HasExited) {
        Stop-Process -Id $application.Id -Force
    }
}
