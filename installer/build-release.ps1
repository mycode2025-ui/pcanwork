[CmdletBinding()]
param(
    [switch]$SkipCargoBuild,
    [switch]$KeepVersion,
    [switch]$SkipVerification,
    [switch]$RunInstallCycleGate,
    [switch]$RunNativeDpi,
    [switch]$FastBuild,
    [switch]$FullVerification,
    [switch]$AllowVersionOverwrite,
    [ValidateRange(1, 16)]
    [int]$FastJobs = 8,
    [string]$IntegrityPrivateKey = 'D:\_LicenseSecrets\PcanWork\pcanwork-ed25519-private.pem'
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$verificationReport = Join-Path $projectRoot 'artifacts\release-verification\latest.json'
$bumpScript = Join-Path $projectRoot 'tools\bump-workspace-version.ps1'
$utf8Strict = [System.Text.UTF8Encoding]::new($false, $true)

function Get-FastSourceFingerprint {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Directories,
        [Parameter(Mandatory = $true)]
        [string[]]$Files,
        [Parameter(Mandatory = $true)]
        [string]$Toolchain
    )

    $sourceFiles = [System.Collections.Generic.List[System.IO.FileInfo]]::new()
    foreach ($directory in $Directories) {
        $path = Join-Path $projectRoot $directory
        if (Test-Path -LiteralPath $path -PathType Container) {
            Get-ChildItem -LiteralPath $path -Recurse -File | Where-Object {
                $_.FullName -notmatch '[\\/]target[\\/]'
            } | ForEach-Object { $sourceFiles.Add($_) }
        }
    }
    foreach ($file in $Files) {
        $path = Join-Path $projectRoot $file
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            $sourceFiles.Add((Get-Item -LiteralPath $path))
        }
    }

    $rootPrefix = $projectRoot.TrimEnd('\') + '\'
    $entries = $sourceFiles | Sort-Object FullName -Unique | ForEach-Object {
        $relative = if ($_.FullName.StartsWith($rootPrefix, [StringComparison]::OrdinalIgnoreCase)) {
            $_.FullName.Substring($rootPrefix.Length)
        } else {
            $_.FullName
        }
        "$relative`t$((Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash)"
    }
    $payload = "pcanwork-fast-build-state-v2`n$Toolchain`n" + ($entries -join "`n")
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($payload)
        return -join ($sha.ComputeHash($bytes) | ForEach-Object { $_.ToString('x2') })
    } finally {
        $sha.Dispose()
    }
}
if (-not $SkipCargoBuild -and -not $KeepVersion) {
    $bumpedVersion = & $bumpScript -ProjectRoot $projectRoot
    if (-not $?) {
        throw 'Automatic release version increment failed.'
    }
    Write-Host "Release version incremented to $bumpedVersion"
}

# Full product gates are retained for archival releases. FastBuild performs
# the version, binary and installer checks below without compiling the whole
# workspace twice. Pass -FullVerification when a fast-profile package must
# also run every release gate.
if (-not $SkipVerification -and -not $SkipCargoBuild -and (-not $FastBuild -or $FullVerification)) {
    $verificationArguments = @{ Jobs = 4 }
    if (-not $RunNativeDpi) {
        $verificationArguments.SkipNativeDpi = $true
    }
    & (Join-Path $projectRoot 'scripts\run-release-verification.ps1') @verificationArguments
    if (-not $?) {
        throw 'Release verification failed.'
    }
} elseif ($FastBuild -and -not $SkipVerification) {
    Write-Host 'FastBuild: skipped full product gates; use -FullVerification for archival verification.'
}
$productVersionPath = Join-Path $projectRoot 'product-version.txt'
$appVersion = (Get-Content -LiteralPath $productVersionPath -Raw).Trim()
if ($appVersion -notmatch '^\d+\.\d+\.\d+$') {
    throw 'product-version.txt must contain one three-part version.'
}
$rootManifest = Get-Content -LiteralPath (Join-Path $projectRoot 'Cargo.toml') -Raw
$versionMatch = [regex]::Match($rootManifest, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw 'Unable to read the Cargo package version from Cargo.toml.'
}
$cargoVersion = $versionMatch.Groups[1].Value

foreach ($relativeManifest in @('serial\Cargo.toml', 'modbus\Cargo.toml')) {
    $manifestPath = Join-Path $projectRoot $relativeManifest
    $manifest = Get-Content -LiteralPath $manifestPath -Raw
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $match.Success -or $match.Groups[1].Value -ne $cargoVersion) {
        throw "$relativeManifest Cargo package version must match $cargoVersion."
    }
}

$issPath = Join-Path $PSScriptRoot 'pcanwork.iss'
$iss = [System.IO.File]::ReadAllText($issPath, $utf8Strict)
$issVersion = [regex]::Match($iss, '(?m)^#define\s+AppVer\s+"([^"]+)"')
if (-not $issVersion.Success -or $issVersion.Groups[1].Value -ne $appVersion) {
    throw "installer\pcanwork.iss AppVer must match $appVersion."
}

$iconsSection = [regex]::Match($iss, '(?ms)^\[Icons\]\s*(.*?)(?=^\[|\z)').Groups[1].Value
foreach ($shortcut in [regex]::Matches($iconsSection, '(?m)^Name:\s*"([^"]+)"')) {
    $shortcutLeaf = [System.IO.Path]::GetFileName($shortcut.Groups[1].Value)
    if ($shortcutLeaf.IndexOfAny([System.IO.Path]::GetInvalidFileNameChars()) -ge 0) {
        throw "Installer shortcut contains an invalid file name: $shortcutLeaf"
    }
}

$installerPath = Join-Path $PSScriptRoot "dist\PcanWork-Setup-$appVersion.exe"
if ((Test-Path -LiteralPath $installerPath) -and -not $AllowVersionOverwrite) {
    throw "Release $appVersion already exists. Increase the version before creating another release."
}

if (-not $SkipCargoBuild) {
    if ($FastBuild) {
        # MSVC matches the daily development toolchain. The dedicated profile
        # keeps its incremental cache separate from the archival ThinLTO build.
        # Content fingerprints let a version-only installer skip Cargo entirely
        # and prevent a CAN/DBC edit from touching Serial or Modbus packages.
        $env:CARGO_BUILD_JOBS = "$FastJobs"

        $toolchain = (& rustc -Vv) -join "`n"
        if ($LASTEXITCODE -ne 0) {
            throw 'Unable to identify the active Rust toolchain.'
        }
        $commonFiles = @('Cargo.toml', 'Cargo.lock', '.cargo\config.toml')
        $patchedSlint = @('vendor\slint\i-slint-core', 'vendor\slint\i-slint-backend-winit')
        $specs = @(
            [ordered]@{
                Key = 'pcanwork'
                Package = 'pcanwork'
                Executable = 'pcanwork.exe'
                Directories = @('src', 'ui', 'crates\pcanwork-core', 'crates\pcanwork-ui-features', 'shared') + $patchedSlint
                Files = @('build.rs', 'app.ico') + $commonFiles
            },
            [ordered]@{
                Key = 'serial_tool'
                Package = 'serial-tool'
                Executable = 'serial-tool.exe'
                Directories = @('serial', 'shared') + $patchedSlint
                Files = $commonFiles
            },
            [ordered]@{
                Key = 'modbus_tools'
                Package = 'modbus-tools'
                Executable = 'modbus-tools.exe'
                Directories = @('modbus', 'shared') + $patchedSlint
                Files = $commonFiles
            }
        )
        $fastBinaryDirectory = Join-Path $projectRoot 'target\release-fast'
        $statePath = Join-Path $fastBinaryDirectory '.pcanwork-build-state.json'
        $previousState = if (Test-Path -LiteralPath $statePath -PathType Leaf) {
            try { Get-Content -LiteralPath $statePath -Raw | ConvertFrom-Json } catch { $null }
        } else { $null }
        $fingerprints = [ordered]@{}
        $packagesToBuild = [System.Collections.Generic.List[string]]::new()
        foreach ($spec in $specs) {
            $fingerprint = Get-FastSourceFingerprint `
                -Directories $spec.Directories -Files $spec.Files -Toolchain $toolchain
            $fingerprints[$spec.Key] = $fingerprint
            $stored = $null
            if ($null -ne $previousState -and $null -ne $previousState.fingerprints) {
                $property = $previousState.fingerprints.PSObject.Properties[$spec.Key]
                if ($null -ne $property) { $stored = [string]$property.Value }
            }
            $executable = Join-Path $fastBinaryDirectory $spec.Executable
            if ($stored -ne $fingerprint -or -not (Test-Path -LiteralPath $executable -PathType Leaf)) {
                $packagesToBuild.Add($spec.Package)
            }
        }

        if ($packagesToBuild.Count -gt 0) {
            Write-Host "FastBuild packages: $($packagesToBuild -join ', ')"
            foreach ($package in $packagesToBuild) {
                # Separate invocations keep Slint feature resolution and Cargo
                # fingerprints stable between the three standalone products.
                & cargo build --profile release-fast --jobs $FastJobs -p $package
                if ($LASTEXITCODE -ne 0) {
                    throw "Fast Release build failed for $package with exit code $LASTEXITCODE."
                }
            }
            New-Item -ItemType Directory -Force -Path $fastBinaryDirectory | Out-Null
            [ordered]@{
                schema = 2
                generated_at = (Get-Date).ToString('o')
                toolchain = $toolchain
                fingerprints = $fingerprints
            } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath $statePath -Encoding utf8
        } else {
            Write-Host 'FastBuild: source fingerprints unchanged; Cargo skipped.'
        }
    } else {
        # Final archival release: MSVC + ThinLTO. Keep concurrency conservative
        # because linking the vendored Slint renderer is memory intensive.
        $env:CARGO_BUILD_JOBS = '2'
        foreach ($package in @('pcanwork', 'serial-tool', 'modbus-tools')) {
            & cargo build --release -p $package --jobs 2
            if ($LASTEXITCODE -ne 0) {
                throw "Release build failed for $package with exit code $LASTEXITCODE."
            }
        }
    }
}

$binaryProfile = if ($FastBuild) { 'release-fast' } else { 'release' }
$binaryDirectory = Join-Path $projectRoot "target\$binaryProfile"
& (Join-Path $projectRoot 'scripts\prepare-runtime.ps1') -OutputDirectory $binaryDirectory | Out-Null
if (-not $?) {
    throw 'Preparing runtime files failed.'
}
$pcanExe = Join-Path $binaryDirectory 'pcanwork.exe'
$serialExe = Join-Path $binaryDirectory 'serial-tool.exe'
$modbusExe = Join-Path $binaryDirectory 'modbus-tools.exe'
foreach ($executable in @($pcanExe, $serialExe, $modbusExe)) {
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        throw "Release executable is missing: $executable"
    }
}

$versionStamp = Join-Path $projectRoot 'tools\stamp-pe-version.ps1'
$stampDefinitions = @(
    @{ Path = $pcanExe; Product = 'PcanWork'; Description = 'PcanWork CAN/CAN FD Engineering Workbench' },
    @{ Path = $serialExe; Product = 'Serial Tool'; Description = 'Serial, Network and SSH Debugging Tool' },
    @{ Path = $modbusExe; Product = 'Modbus Tools'; Description = 'Modbus Engineering Tools' }
)
foreach ($definition in $stampDefinitions) {
    if ((Get-Item -LiteralPath $definition.Path).VersionInfo.FileVersion -ne $appVersion) {
        & $versionStamp -Executable $definition.Path -Version $appVersion `
            -ProductName $definition.Product -Description $definition.Description | Out-Null
    }
}
foreach ($executable in @($pcanExe, $serialExe, $modbusExe)) {
    if ((Get-Item -LiteralPath $executable).VersionInfo.FileVersion -ne $appVersion) {
        throw "Release executable has the wrong version: $executable"
    }
}

$integrityScript = Join-Path $projectRoot 'tools\sign-integrity.ps1'
& $integrityScript -Executable $pcanExe -Product pcanwork -AppVersion $appVersion -PrivateKey $IntegrityPrivateKey
& $integrityScript -Executable $modbusExe -Product modbus -AppVersion $appVersion -PrivateKey $IntegrityPrivateKey

$iscc = Get-Command 'ISCC.exe' -ErrorAction SilentlyContinue
if ($iscc) {
    $isccPath = $iscc.Source
} else {
    $isccCandidate = @(
        (Join-Path $env:LOCALAPPDATA 'Programs\Inno Setup 6\ISCC.exe')
        (Join-Path ${env:ProgramFiles(x86)} 'Inno Setup 6\ISCC.exe')
        (Join-Path $env:ProgramFiles 'Inno Setup 6\ISCC.exe')
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Leaf) } | Select-Object -First 1
    if (-not $isccCandidate) {
        $isccCandidate = Get-ChildItem -LiteralPath $env:LOCALAPPDATA -Filter 'ISCC.exe' -Recurse -ErrorAction SilentlyContinue |
            Where-Object { $_.FullName -match 'Inno Setup' } |
            Select-Object -First 1
    }
    if (-not $isccCandidate) {
        throw 'ISCC.exe not found. Install Inno Setup 6.'
    }
    $isccPath = if ($isccCandidate -is [System.IO.FileInfo]) { $isccCandidate.FullName } else { [string]$isccCandidate }
}

$isccArguments = @('/Qp', '/DSkipSigning')
if ($FastBuild) {
    $isccArguments += '/DFastPackage'
}
$isccArguments += $issPath
& $isccPath @isccArguments
if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $installerPath -PathType Leaf)) {
    throw "Installer build failed for $installerPath."
}

$releaseFiles = @($pcanExe, $serialExe, $modbusExe, $installerPath) | ForEach-Object {
    $item = Get-Item -LiteralPath $_
    [pscustomobject]@{
        File = $item.FullName
        Bytes = $item.Length
        Version = $item.VersionInfo.FileVersion
        SHA256 = (Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash
        Signature = (Get-AuthenticodeSignature -LiteralPath $_).Status.ToString()
    }
}
$releaseFiles | Format-List

$reportDirectory = Join-Path $PSScriptRoot "dist\reports\$appVersion"
New-Item -ItemType Directory -Force -Path $reportDirectory | Out-Null
$commit = (& git -C $projectRoot rev-parse HEAD 2>$null)
$dirty = [bool](& git -C $projectRoot status --porcelain 2>$null)
$report = [ordered]@{
    version = $appVersion
    generated_at = (Get-Date).ToString('o')
    git_commit = $commit
    dirty_worktree = $dirty
    cargo_profile = if ($FastBuild) { 'release-fast-msvc' } else { 'release-lto-msvc' }
    files = $releaseFiles
}
$report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (
    Join-Path $reportDirectory 'release-report.json'
) -Encoding utf8
$releaseFiles | ForEach-Object {
    "$($_.SHA256)  $([System.IO.Path]::GetFileName($_.File))"
} | Set-Content -LiteralPath (Join-Path $reportDirectory 'SHA256SUMS.txt') -Encoding ascii
$diffStat = (& git -C $projectRoot -c core.autocrlf=false diff --stat 2>$null)
@(
    "# PcanWork $appVersion"
    ""
    "Generated: $((Get-Date).ToString('o'))"
    "Commit: $commit"
    "Dirty worktree: $dirty"
    ""
    "## Changed files"
    ""
    $diffStat
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
Write-Host "Release evidence: $reportDirectory"
