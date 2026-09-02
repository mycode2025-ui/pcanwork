[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$ApplicationArguments
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
& (Join-Path $PSScriptRoot 'prepare-runtime.ps1') `
    -OutputDirectory (Join-Path $root 'target\debug') | Out-Null

Push-Location $root
try {
    & cargo run -p pcanwork -- @ApplicationArguments
    exit $LASTEXITCODE
} finally {
    Pop-Location
}
