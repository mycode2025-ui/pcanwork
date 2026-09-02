[CmdletBinding()]
param(
    [string]$InstallDirectory = (Join-Path $env:ProgramFiles 'PcanWork'),
    [string]$ExpectedVersion,
    [string]$ReleaseReport,
    [string]$ReleaseDirectory,
    [string]$ExpectedRecentProject,
    [string]$OutputReport
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if (-not $ExpectedVersion) {
    $root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    $ExpectedVersion = (Get-Content -LiteralPath (Join-Path $root 'product-version.txt') -Raw).Trim()
    if ($ExpectedVersion -notmatch '^\d+\.\d+\.\d+$') {
        throw 'Unable to determine the expected product version.'
    }
}

$install = [System.IO.Path]::GetFullPath($InstallDirectory)
$checks = [System.Collections.Generic.List[object]]::new()
function Assert-ProductCheck([string]$Name, [bool]$Condition, [string]$Detail) {
    $checks.Add([ordered]@{ name = $Name; passed = $Condition; detail = $Detail })
    if (-not $Condition) { throw "$Name failed: $Detail" }
}

$requiredFiles = @(
    'pcanwork.exe', 'pcanwork.exe.integrity', 'serial-tool.exe',
    'modbus-tools.exe', 'modbus-tools.exe.integrity', 'zlgcan.dll',
    'kerneldlls\CANDevCore.dll', 'kerneldlls\CANDevice.dll',
    'kerneldlls\USBCAN_E_64.dll', 'kerneldlls\USBCANFD.dll',
    'kerneldlls\devices_property\usbcan-e-u.xml',
    'kerneldlls\devices_property\usbcanfd-200u.xml',
    'drivers\zlg-usbcan-e-u\usbcan_e_u_x64.inf',
    'ECanVci64.dll', 'CHUSBDLL64.dll', 'ControlCAN.dll',
    'app.ico', 'project.ico', 'pcanwork.py'
)
foreach ($relative in $requiredFiles) {
    $path = Join-Path $install $relative
    Assert-ProductCheck "Installed file $relative" (Test-Path -LiteralPath $path -PathType Leaf) $path
}

foreach ($name in @('pcanwork.exe', 'serial-tool.exe', 'modbus-tools.exe')) {
    $path = Join-Path $install $name
    $actual = (Get-Item -LiteralPath $path).VersionInfo.FileVersion
    Assert-ProductCheck "Version $name" ($actual -eq $ExpectedVersion) "expected=$ExpectedVersion actual=$actual"
}

if ($ReleaseDirectory) {
    $releaseDirectoryPath = [System.IO.Path]::GetFullPath($ReleaseDirectory)
    foreach ($name in @(
        'pcanwork.exe', 'pcanwork.exe.integrity', 'serial-tool.exe',
        'modbus-tools.exe', 'modbus-tools.exe.integrity'
    )) {
        $installedPath = Join-Path $install $name
        $releasePath = Join-Path $releaseDirectoryPath $name
        Assert-ProductCheck "Release reference $name" (
            Test-Path -LiteralPath $releasePath -PathType Leaf
        ) $releasePath
        $installedHash = (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash
        $releaseHash = (Get-FileHash -LiteralPath $releasePath -Algorithm SHA256).Hash
        Assert-ProductCheck "Installed/release SHA256 $name" (
            $installedHash -eq $releaseHash
        ) "installed=$installedHash release=$releaseHash"
    }
}

if ($ReleaseReport) {
    $release = Get-Content -LiteralPath $ReleaseReport -Raw | ConvertFrom-Json
    $releaseEntries = if ($release.PSObject.Properties.Name -contains 'files') {
        @($release.files)
    } else {
        @($release.artifacts)
    }
    foreach ($entry in $releaseEntries) {
        $name = [System.IO.Path]::GetFileName([string]$entry.File)
        if ($name -eq ('PcanWork-Setup-' + $ExpectedVersion + '.exe')) { continue }
        $installedPath = Join-Path $install $name
        if (Test-Path -LiteralPath $installedPath -PathType Leaf) {
            $expectedHash = if ($entry.PSObject.Properties.Name -contains 'SHA256') {
                [string]$entry.SHA256
            } else { [string]$entry.sha256 }
            $actualHash = (Get-FileHash -LiteralPath $installedPath -Algorithm SHA256).Hash
            Assert-ProductCheck "SHA256 $name" ($actualHash -eq $expectedHash) "expected=$expectedHash actual=$actualHash"
        }
    }
}

$extension = [Microsoft.Win32.Registry]::ClassesRoot.OpenSubKey('.pcprj')
$className = if ($extension) { [string]$extension.GetValue('') } else { '' }
if ($extension) { $extension.Dispose() }
Assert-ProductCheck 'Project extension class' ($className -eq 'PcanWork.Project') "actual=$className"

$commandKey = [Microsoft.Win32.Registry]::ClassesRoot.OpenSubKey(
    'PcanWork.Project\shell\open\command'
)
$openCommand = if ($commandKey) { [string]$commandKey.GetValue('') } else { '' }
if ($commandKey) { $commandKey.Dispose() }
$expectedExecutable = Join-Path $install 'pcanwork.exe'
Assert-ProductCheck 'Project open command executable' (
    $openCommand.Contains('"' + $expectedExecutable + '"') -and $openCommand.Contains('"%1"')
) $openCommand

$uninstallEntries = @(
    'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
    'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
) | ForEach-Object { Get-ItemProperty $_ -ErrorAction SilentlyContinue } |
    Where-Object {
        $_.PSObject.Properties.Name -contains 'DisplayName' -and
        $_.DisplayName -like 'PcanWork*'
    }
$matchingUninstall = @($uninstallEntries) | Where-Object {
    $_.PSObject.Properties.Name -contains 'DisplayVersion' -and
    $_.PSObject.Properties.Name -contains 'InstallLocation' -and
    -not [string]::IsNullOrWhiteSpace([string]$_.InstallLocation) -and
    $_.DisplayVersion -eq $ExpectedVersion -and
    [System.IO.Path]::GetFullPath($_.InstallLocation.TrimEnd('\')) -eq $install.TrimEnd('\')
}
Assert-ProductCheck 'Uninstall registration' (@($matchingUninstall).Count -ge 1) (
    "PcanWork version=$ExpectedVersion install=$install"
)

$settingsPath = Join-Path $env:LOCALAPPDATA 'PcanWork\pcanwork_settings.json'
if ($ExpectedRecentProject) {
    $settings = Get-Content -LiteralPath $settingsPath -Raw | ConvertFrom-Json
    $recent = @($settings.recent_project_paths)
    Assert-ProductCheck 'Recent project persistence' ($recent -contains $ExpectedRecentProject) (
        "expected=$ExpectedRecentProject actual=$($recent -join ';')"
    )
}

$report = [ordered]@{
    passed = $true
    generated_at = (Get-Date).ToString('o')
    install_directory = $install
    expected_version = $ExpectedVersion
    settings_path = $settingsPath
    checks = $checks
}
if (-not $OutputReport) {
    $OutputReport = Join-Path (
        (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
    ) 'artifacts\installed-product\latest.json'
}
$reportParent = Split-Path -Parent $OutputReport
New-Item -ItemType Directory -Force -Path $reportParent | Out-Null
$report | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $OutputReport -Encoding utf8
Write-Host "Installed product verification passed: $OutputReport"
