[CmdletBinding()]
param(
    [switch]$SkipQuickGate,
    [switch]$SkipBuild,
    [switch]$SkipFaultGates,
    [switch]$SkipBusOffGate,
    [switch]$RunPnpHotplugGate,
    [string]$Dbc = 'C:\Users\XCHARGE-2026Q1-LT08\Desktop\EU_HVBOXCheck\HVBoxCheck_EU.dbc',
    [ValidateRange(0.01, 168.0)]
    [double]$QuickHours = 8.0,
    [ValidateRange(0.01, 168.0)]
    [double]$FormalHours = 24.0
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$evidence = Join-Path $root "artifacts\hardware-certification\$stamp"
New-Item -ItemType Directory -Force -Path $evidence | Out-Null
$runner = Join-Path $PSScriptRoot 'run-hardware-soak.ps1'
$stages = [System.Collections.Generic.List[object]]::new()

if (-not $SkipBuild) {
    $env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
    $env:CARGO_TARGET_DIR = Join-Path $root 'target'
    $env:CARGO_BUILD_JOBS = '4'
    & cargo build --locked -p pcanwork --jobs 4
    if ($LASTEXITCODE -ne 0) { throw 'Latest Debug hardware-gate build failed.' }
}

function Invoke-HardwareStage([string]$Name, [double]$Hours, [string]$ReportName) {
    $report = Join-Path $evidence $ReportName
    $started = Get-Date
    & $runner -Hours $Hours -Report $report
    if ($LASTEXITCODE -ne 0) { throw "$Name failed with exit code $LASTEXITCODE" }
    $result = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
    if (-not $result.passed) { throw "$Name did not produce a passing report." }
    $stages.Add([ordered]@{
        name = $Name
        hours = $Hours
        seconds = [math]::Round(((Get-Date) - $started).TotalSeconds, 3)
        report = $report
        reconnects = $result.reconnects
        tx = $result.tx
        rx = $result.rx
        passed = $true
    })
}

function Invoke-AuxiliaryGate([string]$Name, [scriptblock]$Action) {
    $started = Get-Date
    & $Action
    if ($LASTEXITCODE -ne 0) { throw "$Name failed with exit code $LASTEXITCODE" }
    $stages.Add([ordered]@{
        name = $Name
        seconds = [math]::Round(((Get-Date) - $started).TotalSeconds, 3)
        passed = $true
    })
}

if (-not $SkipFaultGates) {
    Invoke-AuxiliaryGate 'device-occupancy' {
        & (Join-Path $PSScriptRoot 'run-device-occupancy-gate.ps1')
    }
    Invoke-AuxiliaryGate 'abnormal-exit-recovery' {
        & (Join-Path $PSScriptRoot 'run-abnormal-exit-gate.ps1') -Dbc $Dbc
    }
    if (-not $SkipBusOffGate) {
        Invoke-AuxiliaryGate 'bus-off-recovery' {
            & (Join-Path $PSScriptRoot 'run-bus-off-recovery-gate.ps1') `
                -ConfirmBusOffFaultInjection -Dbc $Dbc -SkipBuild
        }
    }
    if ($RunPnpHotplugGate) {
        Invoke-AuxiliaryGate 'pnp-hotplug-recovery' {
            & (Join-Path $PSScriptRoot 'run-pnp-hotplug-gate.ps1') `
                -ConfirmPnpDeviceCycle -Dbc $Dbc
        }
    }
}

if (-not $SkipQuickGate) {
    Invoke-HardwareStage 'four-channel-quick' $QuickHours 'quick-8h.json'
}
Invoke-HardwareStage 'four-channel-formal' $FormalHours 'formal-24h.json'

[ordered]@{
    passed = $true
    completed_at = (Get-Date).ToString('o')
    stages = $stages
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (
    Join-Path $evidence 'summary.json'
) -Encoding utf8
Write-Host "Hardware certification passed: $evidence"
