[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$ProjectRoot,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path -LiteralPath $ProjectRoot).Path
$productVersionPath = Join-Path $root 'product-version.txt'
$utf8 = [System.Text.UTF8Encoding]::new($false, $true)
$oldVersion = [System.IO.File]::ReadAllText($productVersionPath, $utf8).Trim()
$match = [regex]::Match($oldVersion, '^(\d+)\.(\d+)\.(\d+)$')
if (-not $match.Success) {
    throw 'product-version.txt must contain one three-part version.'
}
$newVersion = $match.Groups[1].Value + '.' + $match.Groups[2].Value + '.' + ([int]$match.Groups[3].Value + 1)
if ($DryRun) {
    Write-Output $newVersion
    return
}

$installerPath = Join-Path $root 'installer\pcanwork.iss'
$installer = [System.IO.File]::ReadAllText($installerPath, $utf8)
$installerMatch = [regex]::Match($installer, '(?m)^#define\s+AppVer\s+"([^"]+)"')
if (-not $installerMatch.Success -or $installerMatch.Groups[1].Value -ne $oldVersion) {
    throw "Installer version is not $oldVersion."
}

$updatedInstaller = $installer.Remove($installerMatch.Index, $installerMatch.Length).Insert(
    $installerMatch.Index,
    $installerMatch.Value.Replace($oldVersion, $newVersion)
)

$iconsSection = [regex]::Match(
    $updatedInstaller,
    '(?ms)^\[Icons\]\s*(.*?)(?=^\[|\z)'
).Groups[1].Value
foreach ($shortcut in [regex]::Matches($iconsSection, '(?m)^Name:\s*"([^"]+)"')) {
    $shortcutLeaf = [System.IO.Path]::GetFileName($shortcut.Groups[1].Value)
    if ($shortcutLeaf.IndexOfAny([System.IO.Path]::GetInvalidFileNameChars()) -ge 0) {
        throw "Installer shortcut contains an invalid file name: $shortcutLeaf"
    }
}

[System.IO.File]::WriteAllText($productVersionPath, "$newVersion`n", $utf8)
[System.IO.File]::WriteAllText($installerPath, $updatedInstaller, $utf8)

# Cargo package versions intentionally remain stable. Product versioning is
# stamped into the PE resources after compilation so a version-only package
# does not invalidate the large generated Slint crates.
Write-Output $newVersion
