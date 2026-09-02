[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$Installer,
    [Parameter(Mandatory)]
    [string]$ExpectedVersion,
    [Parameter(Mandatory)]
    [switch]$ConfirmDestructiveInstallCycle,
    [string]$InstallDirectory = (Join-Path $env:ProgramFiles 'PcanWork'),
    [string]$ReleaseReport
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'The install-cycle gate must run in an elevated PowerShell process.'
}
if (-not $ConfirmDestructiveInstallCycle) {
    throw 'Pass -ConfirmDestructiveInstallCycle explicitly; this gate uninstalls and reinstalls PcanWork.'
}

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$installerPath = (Resolve-Path -LiteralPath $Installer).Path
$install = [System.IO.Path]::GetFullPath($InstallDirectory)
$expectedDefault = [System.IO.Path]::GetFullPath((Join-Path $env:ProgramFiles 'PcanWork'))
if ($install -ne $expectedDefault) {
    throw "Refusing an unexpected install target: $install"
}
$artifactDirectory = Join-Path $root (
    'artifacts\install-cycle\' + (Get-Date -Format 'yyyyMMdd-HHmmss')
)
New-Item -ItemType Directory -Force -Path $artifactDirectory | Out-Null

$userDirectory = Join-Path $env:LOCALAPPDATA 'PcanWork'
$settingsPath = Join-Path $userDirectory 'pcanwork_settings.json'
$licensePath = Join-Path $userDirectory 'license.pcanlic'
$settingsBackup = Join-Path $artifactDirectory 'pcanwork_settings.original.json'
$licenseBackup = Join-Path $artifactDirectory 'license.original.pcanlic'
$hadSettings = Test-Path -LiteralPath $settingsPath -PathType Leaf
$hadLicense = Test-Path -LiteralPath $licensePath -PathType Leaf
if ($hadSettings) { Copy-Item -LiteralPath $settingsPath -Destination $settingsBackup }
if ($hadLicense) { Copy-Item -LiteralPath $licensePath -Destination $licenseBackup }

$sentinelProject = Join-Path $artifactDirectory 'install-cycle-recent.pcprj'
'{"name":"InstallCycleGate"}' | Set-Content -LiteralPath $sentinelProject -Encoding utf8
New-Item -ItemType Directory -Force -Path $userDirectory | Out-Null
$settings = if ($hadSettings) {
    Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json
} else {
    [pscustomobject]@{}
}
if (-not ($settings.PSObject.Properties.Name -contains 'recent_project_paths')) {
    $settings | Add-Member -NotePropertyName recent_project_paths -NotePropertyValue @()
}
$settings.recent_project_paths = @($sentinelProject) + @(
    $settings.recent_project_paths | Where-Object { $_ -ne $sentinelProject }
)
$settings | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $settingsPath -Encoding utf8

function Invoke-Installer([string[]]$Arguments) {
    $process = Start-Process -FilePath $installerPath -ArgumentList $Arguments -Wait -PassThru `
        -WindowStyle Hidden
    if ($process.ExitCode -ne 0) { throw "Installer failed with exit code $($process.ExitCode)." }
}

function Invoke-Uninstaller {
    $uninstaller = Get-ChildItem -LiteralPath $install -Filter 'unins*.exe' -File `
        -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $uninstaller) { return }
    $process = Start-Process -FilePath $uninstaller.FullName -ArgumentList @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART'
    ) -Wait -PassThru -WindowStyle Hidden
    if ($process.ExitCode -ne 0) { throw "Uninstaller failed with exit code $($process.ExitCode)." }
    if (Test-Path -LiteralPath (Join-Path $install 'pcanwork.exe')) {
        throw 'Uninstall left pcanwork.exe behind.'
    }
    $extension = [Microsoft.Win32.Registry]::ClassesRoot.OpenSubKey('.pcprj')
    $className = if ($extension) { [string]$extension.GetValue('') } else { '' }
    if ($extension) { $extension.Dispose() }
    if ($className -eq 'PcanWork.Project') {
        throw 'Uninstall left the PcanWork .pcprj association behind.'
    }
}

$verify = Join-Path $PSScriptRoot 'verify-installed-product.ps1'
$completed = $false
try {
    Invoke-Uninstaller
    Invoke-Installer @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOICONS',
        "/DIR=`"$install`"", "/LOG=`"$(Join-Path $artifactDirectory 'install-first.log')`""
    )
    & $verify -InstallDirectory $install -ExpectedVersion $ExpectedVersion `
        -ReleaseReport $ReleaseReport -ExpectedRecentProject $sentinelProject `
        -OutputReport (Join-Path $artifactDirectory 'first-install.json')

    Invoke-Uninstaller
    if (-not (Test-Path -LiteralPath $settingsPath -PathType Leaf)) {
        throw 'Uninstall removed user settings; recent projects must survive reinstall.'
    }
    if ($hadLicense -and -not (Test-Path -LiteralPath $licensePath -PathType Leaf)) {
        throw 'Uninstall removed the installed offline license.'
    }

    Invoke-Installer @(
        '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOICONS',
        "/DIR=`"$install`"", "/LOG=`"$(Join-Path $artifactDirectory 'install-second.log')`""
    )
    & $verify -InstallDirectory $install -ExpectedVersion $ExpectedVersion `
        -ReleaseReport $ReleaseReport -ExpectedRecentProject $sentinelProject `
        -OutputReport (Join-Path $artifactDirectory 'second-install.json')
    $completed = $true
}
finally {
    if (-not (Test-Path -LiteralPath (Join-Path $install 'pcanwork.exe') -PathType Leaf)) {
        Write-Warning 'Restoring the installed application after an interrupted gate.'
        Invoke-Installer @(
            '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/NOICONS',
            "/DIR=`"$install`"", "/LOG=`"$(Join-Path $artifactDirectory 'install-recovery.log')`""
        )
    }
    if ($hadSettings) {
        Copy-Item -LiteralPath $settingsBackup -Destination $settingsPath -Force
    } else {
        Remove-Item -LiteralPath $settingsPath -ErrorAction SilentlyContinue
    }
    if ($hadLicense) {
        Copy-Item -LiteralPath $licenseBackup -Destination $licensePath -Force
    } else {
        Remove-Item -LiteralPath $licensePath -ErrorAction SilentlyContinue
    }
}

if (-not $completed) { throw 'Install-cycle gate did not complete.' }
[ordered]@{
    passed = $true
    completed_at = (Get-Date).ToString('o')
    installer = $installerPath
    version = $ExpectedVersion
    install_directory = $install
    settings_restored = $true
    license_restored = $true
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $artifactDirectory 'summary.json') -Encoding utf8
Write-Host "Install-cycle gate passed: $artifactDirectory"
