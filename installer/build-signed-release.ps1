[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$CertificateThumbprint,

    [ValidateNotNullOrEmpty()]
    [string]$TimestampUrl = "http://timestamp.digicert.com",

    [switch]$SkipCargoBuild,

    [switch]$KeepVersion,

    [switch]$SkipVerification,

    [switch]$RunInstallCycleGate,

    [switch]$AllowVersionOverwrite,

    [string]$IntegrityPrivateKey = 'D:\_LicenseSecrets\PcanWork\pcanwork-ed25519-private.pem'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$verificationReport = Join-Path $projectRoot 'artifacts\release-verification\latest.json'
if (-not $SkipVerification -and -not $SkipCargoBuild) {
    & (Join-Path $projectRoot 'scripts\run-release-verification.ps1') -Jobs 4
    if (-not $?) {
        throw 'Release verification failed.'
    }
}
if (-not $SkipCargoBuild -and -not $KeepVersion) {
    $bumpedVersion = & (Join-Path $projectRoot 'tools\bump-workspace-version.ps1') `
        -ProjectRoot $projectRoot
    if (-not $?) {
        throw 'Automatic release version increment failed.'
    }
    Write-Host "Release version incremented to $bumpedVersion"
}
$appVersion = (Get-Content -LiteralPath (Join-Path $projectRoot 'product-version.txt') -Raw).Trim()
if ($appVersion -notmatch '^\d+\.\d+\.\d+$') {
    throw 'product-version.txt must contain one three-part version.'
}
$rootManifest = Get-Content -LiteralPath (Join-Path $projectRoot "Cargo.toml") -Raw
$versionMatch = [regex]::Match($rootManifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw "Unable to read the Cargo package version from Cargo.toml"
}
$cargoVersion = $versionMatch.Groups[1].Value
$memberVersions = @(
    "serial\Cargo.toml",
    "modbus\Cargo.toml"
) | ForEach-Object {
    $manifestPath = Join-Path $projectRoot $_
    $manifest = Get-Content -LiteralPath $manifestPath -Raw
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success) {
        throw "Unable to read the product version from $manifestPath"
    }
    $match.Groups[1].Value
}
if ($memberVersions | Where-Object { $_ -ne $cargoVersion }) {
    throw "Workspace Cargo package versions must all match $cargoVersion."
}
$issPath = Join-Path $PSScriptRoot "pcanwork.iss"
$iss = Get-Content -LiteralPath $issPath -Raw
$issVersionMatch = [regex]::Match($iss, '(?m)^#define\s+AppVer\s+"([^"]+)"')
if (-not $issVersionMatch.Success -or $issVersionMatch.Groups[1].Value -ne $appVersion) {
    throw "installer\pcanwork.iss AppVer must match product version $appVersion."
}
$certificateThumbprint = $CertificateThumbprint.Replace(" ", "").ToUpperInvariant()
$distDir = Join-Path $PSScriptRoot "dist"
$installerPath = Join-Path $distDir "PcanWork-Setup-$appVersion.exe"
if ((Test-Path -LiteralPath $installerPath) -and -not $AllowVersionOverwrite) {
    throw "Release $appVersion already exists. Increase the workspace version before creating another release."
}
$executables = @(
    (Join-Path $projectRoot "target\release\pcanwork.exe"),
    (Join-Path $projectRoot "target\release\serial-tool.exe"),
    (Join-Path $projectRoot "target\release\modbus-tools.exe")
)

function Find-SignTool {
    $command = Get-Command "signtool.exe" -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }
    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    $candidate = Get-ChildItem -LiteralPath $kitsRoot -Filter "signtool.exe" -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "\\x64\\signtool\.exe$" } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if (-not $candidate) {
        throw "signtool.exe not found. Install the Windows SDK signing tools."
    }
    return $candidate.FullName
}

function Find-Iscc {
    $command = Get-Command "ISCC.exe" -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }
    $candidate = Get-ChildItem -LiteralPath $env:LOCALAPPDATA -Filter "ISCC.exe" -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match "Inno Setup" } |
        Select-Object -First 1
    if (-not $candidate) {
        throw "ISCC.exe not found. Install Inno Setup 6."
    }
    return $candidate.FullName
}

function Assert-ValidSignature([string]$Path) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne "Valid") {
        throw "Authenticode verification failed for '$Path': $($signature.Status) $($signature.StatusMessage)"
    }
}

$certificate = Get-ChildItem "Cert:\CurrentUser\My\$certificateThumbprint", "Cert:\LocalMachine\My\$certificateThumbprint" -ErrorAction SilentlyContinue |
    Select-Object -First 1
if (-not $certificate) {
    throw "Code-signing certificate '$certificateThumbprint' was not found in CurrentUser/My or LocalMachine/My."
}
if (-not $certificate.HasPrivateKey) {
    throw "Certificate '$certificateThumbprint' does not expose a private key."
}
if ($certificate.NotBefore -gt (Get-Date) -or $certificate.NotAfter -le (Get-Date)) {
    throw "Certificate '$certificateThumbprint' is outside its validity period."
}
$codeSigningOid = "1.3.6.1.5.5.7.3.3"
if ($certificate.EnhancedKeyUsageList.ObjectId.Value -notcontains $codeSigningOid) {
    throw "Certificate '$certificateThumbprint' is not valid for code signing."
}

$signTool = Find-SignTool
$iscc = Find-Iscc

if (-not $SkipCargoBuild) {
    $env:CARGO_BUILD_JOBS = "2"
    & cargo build --workspace --release
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo release build failed with exit code $LASTEXITCODE."
    }
}

$versionStamp = Join-Path $projectRoot 'tools\stamp-pe-version.ps1'
$stampDefinitions = @(
    @{ Path = $executables[0]; Product = 'PcanWork'; Description = 'PcanWork CAN/CAN FD Engineering Workbench' },
    @{ Path = $executables[1]; Product = 'Serial Tool'; Description = 'Serial, Network and SSH Debugging Tool' },
    @{ Path = $executables[2]; Product = 'Modbus Tools'; Description = 'Modbus Engineering Tools' }
)
foreach ($definition in $stampDefinitions) {
    & $versionStamp -Executable $definition.Path -Version $appVersion `
        -ProductName $definition.Product -Description $definition.Description | Out-Null
}

foreach ($executable in $executables) {
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw "Release executable is missing: $executable"
    }
    & $signTool sign /sha1 $certificateThumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 /v $executable
    if ($LASTEXITCODE -ne 0) {
        throw "Signing failed for '$executable' with exit code $LASTEXITCODE."
    }
    Assert-ValidSignature $executable
}

$integrityScript = Join-Path $projectRoot "tools\sign-integrity.ps1"
& $integrityScript -Executable $executables[0] -Product pcanwork -AppVersion $appVersion -PrivateKey $IntegrityPrivateKey
if ($LASTEXITCODE -ne 0) {
    throw "PcanWork integrity signing failed with exit code $LASTEXITCODE."
}
& $integrityScript -Executable $executables[2] -Product modbus -AppVersion $appVersion -PrivateKey $IntegrityPrivateKey
if ($LASTEXITCODE -ne 0) {
    throw "Modbus integrity signing failed with exit code $LASTEXITCODE."
}

$signCommand = "`"$signTool`" sign /sha1 $certificateThumbprint /fd SHA256 /tr $TimestampUrl /td SHA256 /v `$f"
& $iscc "/Srelease=$signCommand" $issPath
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup compilation failed with exit code $LASTEXITCODE."
}
if (-not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
    throw "Expected installer was not produced: $installerPath"
}
Assert-ValidSignature $installerPath

$artifacts = @($executables + $installerPath) | ForEach-Object {
    $item = Get-Item -LiteralPath $_
    $signature = Get-AuthenticodeSignature -LiteralPath $_
    [ordered]@{
        file = $item.Name
        bytes = $item.Length
        sha256 = (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash
        file_version = $item.VersionInfo.FileVersion
        signature = $signature.Status.ToString()
        signer_thumbprint = $signature.SignerCertificate.Thumbprint
        timestamp_subject = $signature.TimeStamperCertificate.Subject
    }
}

$manifest = [ordered]@{
    product = "PcanWork"
    version = $appVersion
    generated_utc = (Get-Date).ToUniversalTime().ToString("o")
    artifacts = $artifacts
}
$manifestPath = Join-Path $distDir "PcanWork-$appVersion-release-manifest.json"
$manifest | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath $manifestPath -Encoding UTF8
$reportDirectory = Join-Path $distDir "reports\$appVersion"
New-Item -ItemType Directory -Force -Path $reportDirectory | Out-Null
Copy-Item -LiteralPath $manifestPath -Destination (
    Join-Path $reportDirectory 'release-report.json'
) -Force
$artifacts | ForEach-Object {
    "$($_.sha256)  $($_.file)"
} | Set-Content -LiteralPath (Join-Path $reportDirectory 'SHA256SUMS.txt') -Encoding ascii
$commit = (& git -C $projectRoot rev-parse HEAD 2>$null)
$dirty = [bool](& git -C $projectRoot status --porcelain 2>$null)
@(
    "# PcanWork $appVersion"
    ""
    "Generated: $((Get-Date).ToString('o'))"
    "Commit: $commit"
    "Dirty worktree: $dirty"
    "Authenticode: Valid"
    ""
    "## Changed files"
    ""
    (& git -C $projectRoot diff --stat 2>$null)
) | Set-Content -LiteralPath (Join-Path $reportDirectory 'CHANGELOG.md') -Encoding utf8
if (Test-Path -LiteralPath $verificationReport) {
    Copy-Item -LiteralPath $verificationReport -Destination (
        Join-Path $reportDirectory 'verification.json'
    ) -Force
}
if ($RunInstallCycleGate) {
    & (Join-Path $projectRoot 'scripts\run-install-cycle-gate.ps1') `
        -Installer $installerPath -ExpectedVersion $appVersion `
        -ReleaseReport (Join-Path $reportDirectory 'release-report.json') `
        -ConfirmDestructiveInstallCycle
}
Write-Host "Signed release completed: $installerPath"
Write-Host "Release manifest: $manifestPath"
Write-Host "Release evidence: $reportDirectory"
