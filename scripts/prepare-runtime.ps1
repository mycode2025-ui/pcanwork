[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$outputPath = if ([System.IO.Path]::IsPathRooted($OutputDirectory)) {
    $OutputDirectory
} else {
    Join-Path $root $OutputDirectory
}
$output = [System.IO.Path]::GetFullPath($outputPath)
New-Item -ItemType Directory -Force -Path $output | Out-Null

function Copy-IfChanged([string]$Source, [string]$Destination) {
    $sourceItem = Get-Item -LiteralPath $Source
    $copy = $true
    if (Test-Path -LiteralPath $Destination -PathType Leaf) {
        $destinationItem = Get-Item -LiteralPath $Destination
        $copy = $sourceItem.Length -ne $destinationItem.Length -or
            $sourceItem.LastWriteTimeUtc -ne $destinationItem.LastWriteTimeUtc
    }
    if ($copy) {
        $parent = Split-Path -Parent $Destination
        New-Item -ItemType Directory -Force -Path $parent | Out-Null
        Copy-Item -LiteralPath $Source -Destination $Destination -Force
    }
}

function Copy-TreeIfChanged([string]$SourceDirectory, [string]$DestinationDirectory) {
    $sourceRoot = (Resolve-Path -LiteralPath $SourceDirectory).Path
    $sourcePrefix = $sourceRoot.TrimEnd('\') + '\'
    foreach ($sourceFile in Get-ChildItem -LiteralPath $sourceRoot -File -Recurse) {
        if (-not $sourceFile.FullName.StartsWith($sourcePrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Runtime source escaped its root: $($sourceFile.FullName)"
        }
        $relative = $sourceFile.FullName.Substring($sourcePrefix.Length)
        Copy-IfChanged $sourceFile.FullName (Join-Path $DestinationDirectory $relative)
    }
}

$files = @(
    @('pcanwork.py', 'pcanwork.py'),
    @('zlgcan_x64\zlgcan.dll', 'zlgcan.dll'),
    @('GCAN\x64\ECanVci64.dll', 'ECanVci64.dll'),
    @('GCAN\x64\CHUSBDLL64.dll', 'CHUSBDLL64.dll'),
    @('zhcxCAN\x64\ControlCAN.dll', 'ControlCAN.dll')
)
foreach ($file in $files) {
    Copy-IfChanged (Join-Path $root $file[0]) (Join-Path $output $file[1])
}

Copy-TreeIfChanged (Join-Path $root 'zlgcan_x64\kerneldlls') (Join-Path $output 'kerneldlls')
foreach ($template in Get-ChildItem -LiteralPath (Join-Path $root 'templates') -File -Filter '*.py') {
    Copy-IfChanged $template.FullName (Join-Path $output (Join-Path 'templates' $template.Name))
}

Write-Output $output
