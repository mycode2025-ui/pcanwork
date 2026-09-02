[CmdletBinding()]
param(
    [ValidateRange(1, 32)]
    [int]$Jobs = 4,
    [switch]$SkipNativeDpi
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$previousToolchain = $env:RUSTUP_TOOLCHAIN
$previousTargetDirectory = $env:CARGO_TARGET_DIR
$previousBuildJobs = $env:CARGO_BUILD_JOBS
$env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
$env:CARGO_TARGET_DIR = Join-Path $root 'target'
$env:CARGO_BUILD_JOBS = [string]$Jobs
$started = Get-Date
$steps = [System.Collections.Generic.List[object]]::new()

function Invoke-VerificationStep([string]$Name, [scriptblock]$Action) {
    $stepStart = Get-Date
    & $Action
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE."
    }
    $steps.Add([ordered]@{
        name = $Name
        seconds = [math]::Round(((Get-Date) - $stepStart).TotalSeconds, 3)
        passed = $true
    })
}

Push-Location $root
try {
    Invoke-VerificationStep 'Git whitespace' { git diff --check }
    Invoke-VerificationStep 'Rust formatting' { cargo fmt --all -- --check }
    Invoke-VerificationStep 'CAN UI syntax' {
        slint-viewer --check ui/app.slint --component AppWindow
    }
    Invoke-VerificationStep 'Serial UI syntax' {
        slint-viewer --check serial/ui/app.slint --component AppWindow
    }
    Invoke-VerificationStep 'Modbus UI syntax' {
        slint-viewer --check modbus/ui/app.slint --component AppWindow
    }
    Invoke-VerificationStep 'UI render matrix' {
        $uiDirectory = Join-Path $root 'artifacts\release-verification\ui'
        New-Item -ItemType Directory -Force -Path $uiDirectory | Out-Null
        $darkData = Join-Path $uiDirectory 'en-dark.json'
        $darkJson = @{ dark = $true; 'lang-en' = $true } | ConvertTo-Json
        [System.IO.File]::WriteAllText(
            $darkData,
            $darkJson,
            [System.Text.UTF8Encoding]::new($false)
        )
        $renders = @(
            @('ui/app.slint', 'can-zh-light-100.png', '', 1.0),
            @('ui/app.slint', 'can-en-dark-150.png', $darkData, 1.5),
            @('ui/app.slint', 'can-en-dark-200.png', $darkData, 2.0),
            @('serial/ui/app.slint', 'serial-zh-light-100.png', '', 1.0),
            @('serial/ui/app.slint', 'serial-en-dark-125.png', $darkData, 1.25),
            @('serial/ui/app.slint', 'serial-en-dark-200.png', $darkData, 2.0),
            @('modbus/ui/app.slint', 'modbus-zh-light-100.png', '', 1.0),
            @('modbus/ui/app.slint', 'modbus-en-dark-150.png', $darkData, 1.5),
            @('modbus/ui/app.slint', 'modbus-en-dark-200.png', $darkData, 2.0)
        )
        $previousScaleFactor = $env:SLINT_SCALE_FACTOR
        $renderEvidence = [System.Collections.Generic.List[object]]::new()
        try {
            foreach ($render in $renders) {
                $env:SLINT_SCALE_FACTOR = ([double]$render[3]).ToString(
                    [Globalization.CultureInfo]::InvariantCulture
                )
                $output = Join-Path $uiDirectory $render[1]
                $arguments = @(
                    $render[0], '--component', 'AppWindow', '--backend', 'software',
                    '--screenshot', $output
                )
                if ($render[2]) { $arguments += @('--load-data', $render[2]) }
                & slint-viewer @arguments
                if ($LASTEXITCODE -ne 0) { throw "UI render failed: $($render[1])" }
                $image = Get-Item -LiteralPath $output
                if ($image.Length -lt 4096) {
                    throw "UI render is unexpectedly small: $($image.FullName)"
                }
                $png = [System.IO.File]::ReadAllBytes($output)
                if ($png.Length -lt 24) { throw "Invalid PNG evidence: $output" }
                $width = [Net.IPAddress]::NetworkToHostOrder([BitConverter]::ToInt32($png, 16))
                $height = [Net.IPAddress]::NetworkToHostOrder([BitConverter]::ToInt32($png, 20))
                if ($width -lt 640 -or $height -lt 400 -or $width -gt 7680 -or $height -gt 4320) {
                    throw "UI render dimensions are invalid: $($render[1]) ${width}x${height}"
                }
                $renderEvidence.Add([ordered]@{
                    file = $image.FullName
                    component = $render[0]
                    scale = [double]$render[3]
                    width = $width
                    height = $height
                    bytes = $image.Length
                    sha256 = (Get-FileHash -LiteralPath $output -Algorithm SHA256).Hash
                })
            }
        }
        finally {
            if ($null -eq $previousScaleFactor) {
                Remove-Item Env:SLINT_SCALE_FACTOR -ErrorAction SilentlyContinue
            } else {
                $env:SLINT_SCALE_FACTOR = $previousScaleFactor
            }
        }
        $renderEvidence | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (
            Join-Path $uiDirectory 'matrix.json'
        ) -Encoding utf8
    }
    if (-not $SkipNativeDpi) {
        Invoke-VerificationStep 'Native DPI matrix' {
            & (Join-Path $root 'scripts\capture-native-dpi-matrix.ps1') `
                -OutputDirectory (Join-Path $root 'artifacts\release-verification\native-dpi') `
                -ScaleFactors 1.0, 1.5, 2.0
        }
    }
    Invoke-VerificationStep 'Workspace tests' {
        cargo test --locked --workspace --all-targets --jobs $Jobs
    }
    Invoke-VerificationStep 'Clippy zero-warning audit' {
        cargo clippy --locked --workspace --all-targets --jobs $Jobs -- -D warnings
    }
    Invoke-VerificationStep 'Automation script syntax' {
        $powerShellScripts = @(
            Get-ChildItem -LiteralPath (Join-Path $root 'scripts') -Filter '*.ps1' -File
            Get-ChildItem -LiteralPath (Join-Path $root 'installer') -Filter '*.ps1' -File
            Get-ChildItem -LiteralPath (Join-Path $root 'tools') -Filter '*.ps1' -File
        )
        foreach ($script in $powerShellScripts) {
            $tokens = $null
            $errors = $null
            [System.Management.Automation.Language.Parser]::ParseFile(
                $script.FullName, [ref]$tokens, [ref]$errors
            ) | Out-Null
            if ($errors.Count -gt 0) {
                throw "PowerShell syntax failed ($($script.FullName)): $($errors.Message -join '; ')"
            }
        }
        $pythonScripts = @(
            Get-ChildItem -LiteralPath (Join-Path $root 'scripts') -Filter '*.py' -File
            Get-ChildItem -LiteralPath (Join-Path $root 'installer') -Filter '*.py' -File
        )
        $pythonSyntaxCheck = "import ast,pathlib,sys; ast.parse(pathlib.Path(sys.argv[1]).read_text(encoding='utf-8'))"
        foreach ($script in $pythonScripts) {
            python -c $pythonSyntaxCheck $script.FullName
            if ($LASTEXITCODE -ne 0) { throw "Python syntax failed: $($script.FullName)" }
        }
    }

    $requiredFiles = @(
        'zlgcan_x64\zlgcan.dll',
        'zlgcan_x64\kerneldlls\CANDevCore.dll',
        'zlgcan_x64\kerneldlls\CANDevice.dll',
        'zlgcan_x64\kerneldlls\USBCAN_E_64.dll',
        'zlgcan_x64\kerneldlls\USBCANFD.dll',
        'zlgcan_x64\kerneldlls\devices_property\usbcan-e-u.xml',
        'zlgcan_x64\kerneldlls\devices_property\usbcanfd-200u.xml',
        'drivers\zlg-usbcan-e-u\usbcan_e_u_x64.inf',
        'GCAN\x64\ECanVci64.dll',
        'GCAN\x64\CHUSBDLL64.dll',
        'zhcxCAN\x64\ControlCAN.dll',
        'assets\project.ico',
        'pcanwork.py'
    )
    foreach ($relativePath in $requiredFiles) {
        if (-not (Test-Path -LiteralPath (Join-Path $root $relativePath))) {
            throw "Required release dependency is missing: $relativePath"
        }
    }
    $installer = Get-Content -LiteralPath (Join-Path $root 'installer\pcanwork.iss') -Raw
    foreach ($requiredInstallerRule in @(
        '.pcprj', 'PcanWork.Project', 'zlgcan.dll',
        'kerneldlls', 'zlg-usbcan-e-u', 'ECanVci64.dll', 'ControlCAN.dll'
    )) {
        if (-not $installer.Contains($requiredInstallerRule)) {
            throw "Installer rule is missing: $requiredInstallerRule"
        }
    }

    $reportDirectory = Join-Path $root 'artifacts\release-verification'
    New-Item -ItemType Directory -Force -Path $reportDirectory | Out-Null
    $report = [ordered]@{
        passed = $true
        generated_at = (Get-Date).ToString('o')
        seconds = [math]::Round(((Get-Date) - $started).TotalSeconds, 3)
        jobs = $Jobs
        steps = $steps
        required_dependencies = $requiredFiles
    }
    $report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (
        Join-Path $reportDirectory 'latest.json'
    ) -Encoding utf8
    Write-Host "Release verification passed: $($report.seconds) s"
}
finally {
    Pop-Location
    if ($null -eq $previousToolchain) { Remove-Item Env:RUSTUP_TOOLCHAIN -ErrorAction SilentlyContinue }
    else { $env:RUSTUP_TOOLCHAIN = $previousToolchain }
    if ($null -eq $previousTargetDirectory) { Remove-Item Env:CARGO_TARGET_DIR -ErrorAction SilentlyContinue }
    else { $env:CARGO_TARGET_DIR = $previousTargetDirectory }
    if ($null -eq $previousBuildJobs) { Remove-Item Env:CARGO_BUILD_JOBS -ErrorAction SilentlyContinue }
    else { $env:CARGO_BUILD_JOBS = $previousBuildJobs }
}
