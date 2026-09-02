[CmdletBinding()]
param(
    [ValidateRange(1, 168)]
    [int]$SoakHours = 24,
    [ValidateRange(100, 1000000)]
    [int]$CaptureFramesPerSecond = 20000,
    [switch]$SkipRelease,
    [string]$CertificateThumbprint
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repository = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$artifactDirectory = Join-Path $repository "artifacts\product-gates\$stamp"
New-Item -ItemType Directory -Force -Path $artifactDirectory | Out-Null
$transcript = Join-Path $artifactDirectory 'gate.log'
$env:RUSTUP_TOOLCHAIN = 'stable-x86_64-pc-windows-msvc'
$env:CARGO_BUILD_JOBS = '1'

function Invoke-GateStep {
    param(
        [Parameter(Mandatory)]
        [string]$Name,
        [Parameter(Mandatory)]
        [scriptblock]$Action
    )

    $started = Get-Date
    "[$($started.ToString('o'))] START $Name" | Tee-Object -FilePath $transcript -Append
    & $Action 2>&1 | Tee-Object -FilePath $transcript -Append
    if ($LASTEXITCODE -ne 0) {
        throw "$Name failed with exit code $LASTEXITCODE"
    }
    $elapsed = (Get-Date) - $started
    "[$((Get-Date).ToString('o'))] PASS $Name ($([math]::Round($elapsed.TotalSeconds, 1)) s)" |
        Tee-Object -FilePath $transcript -Append
}

function Invoke-NativeCommand {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,
        [Parameter()]
        [string[]]$ArgumentList = @()
    )

    & $FilePath @ArgumentList
    if ($LASTEXITCODE -ne 0) {
        throw "$FilePath failed with exit code $LASTEXITCODE"
    }
}

Push-Location $repository
try {
    Invoke-GateStep 'Rust formatting' { cargo fmt --all -- --check }
    Invoke-GateStep 'Release version consistency' {
        $rootManifest = Get-Content -LiteralPath (Join-Path $repository 'Cargo.toml') -Raw
        $rootVersionMatch = [regex]::Match($rootManifest, '(?m)^version\s*=\s*"([^"]+)"')
        if (-not $rootVersionMatch.Success) {
            throw 'Unable to read the product version from Cargo.toml'
        }
        $cargoVersion = $rootVersionMatch.Groups[1].Value
        foreach ($relativeManifest in @('serial\Cargo.toml', 'modbus\Cargo.toml')) {
            $manifest = Get-Content -LiteralPath (Join-Path $repository $relativeManifest) -Raw
            $versionMatch = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"')
            if (-not $versionMatch.Success -or $versionMatch.Groups[1].Value -ne $cargoVersion) {
                throw "Cargo package version does not match $cargoVersion`: $relativeManifest"
            }
        }

        $productVersion = (Get-Content -LiteralPath (
            Join-Path $repository 'product-version.txt'
        ) -Raw).Trim()
        $installerDefinition = Get-Content -LiteralPath (
            Join-Path $repository 'installer\pcanwork.iss'
        ) -Raw
        $installerVersion = [regex]::Match(
            $installerDefinition,
            '(?m)^#define\s+AppVer\s+"([^"]+)"'
        )
        if (-not $installerVersion.Success -or $installerVersion.Groups[1].Value -ne $productVersion) {
            throw "Installer version does not match product version $productVersion"
        }
    }
    Invoke-GateStep 'Private-key source scan' {
        $matches = & rg --hidden --glob '!target/**' --glob '!.git/**' `
            --glob '!installer/dist/**' --glob '!artifacts/**' `
            'BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY' .
        $searchExit = $LASTEXITCODE
        if ($searchExit -eq 0) {
            $matches
            throw 'A private key is present in the source or packaging tree'
        }
        if ($searchExit -ne 1) {
            throw "rg failed with exit code $searchExit"
        }
        $script:LASTEXITCODE = 0
    }
    Invoke-GateStep 'CAN UI syntax' { slint-viewer --check ui/app.slint --component AppWindow }
    Invoke-GateStep 'Serial UI syntax' {
        slint-viewer --check serial/ui/app.slint --component AppWindow
    }
    Invoke-GateStep 'Modbus UI syntax' {
        slint-viewer --check modbus/ui/app.slint --component AppWindow
    }
    Invoke-GateStep 'Single-click UI focus boundary' {
        $uiFiles = @(
            (Join-Path $repository 'ui\common.slint'),
            (Join-Path $repository 'serial\ui\app.slint'),
            (Join-Path $repository 'modbus\ui\app.slint')
        )
        foreach ($uiFile in $uiFiles) {
            $source = Get-Content -LiteralPath $uiFile -Raw
            if ($source.Contains('key-focus.focus()')) {
                throw "Mouse activation must not imperatively focus a custom control: $uiFile"
            }

            $focusScopes = [regex]::Matches(
                $source,
                'key-focus\s*:=\s*FocusScope\s*\{'
            ).Count
            $nonPointerFocusScopes = [regex]::Matches(
                $source,
                'key-focus\s*:=\s*FocusScope\s*\{\s*x:\s*0px;\s*width:\s*0px;',
                [System.Text.RegularExpressions.RegexOptions]::Singleline
            ).Count
            if ($focusScopes -ne $nonPointerFocusScopes) {
                throw "Every custom key-focus scope must have width 0 so the first mouse click reaches its TouchArea: $uiFile"
            }
        }
    }
    Invoke-GateStep 'Shared UI palette boundary' {
        $violations = & rg --glob '*.slint' --glob '!design-system.slint' `
            '#[0-9A-Fa-f]{3,8}|rgb\(|rgba\(' ui serial/ui modbus/ui
        $searchExit = $LASTEXITCODE
        if ($searchExit -eq 0) {
            $violations
            throw 'Literal UI colors exist outside ui/design-system.slint'
        }
        if ($searchExit -ne 1) {
            throw "rg failed with exit code $searchExit"
        }
        $script:LASTEXITCODE = 0
    }
    Invoke-GateStep 'Shared product header boundary' {
        $headerFiles = @(
            (Join-Path $repository 'serial\ui\app.slint'),
            (Join-Path $repository 'modbus\ui\app.slint')
        )
        foreach ($headerFile in $headerFiles) {
            $source = Get-Content -LiteralPath $headerFile -Raw
            foreach ($required in @(
                'ProductDesign.product-header-height',
                'ProductLanguageSwitch',
                'ProductThemeSwitch',
                'ProductHeaderCheck'
            )) {
                if (-not $source.Contains($required)) {
                    throw "Shared header token/component missing ($required): $headerFile"
                }
            }
        }
    }
    Invoke-GateStep 'Bounded queue boundary' {
        $matches = & rg --fixed-strings --glob '*.rs' --glob '!vendor/**' --glob '!target/**' `
            -e 'unbounded(' -e 'unbounded_channel' -e 'std::sync::mpsc::channel' .
        $searchExit = $LASTEXITCODE
        if ($searchExit -eq 0) {
            $matches
            throw 'An unbounded runtime queue exists in the production source tree'
        }
        if ($searchExit -ne 1) {
            throw "rg failed with exit code $searchExit"
        }
        $script:LASTEXITCODE = 0
    }
    Invoke-GateStep 'UI render evidence' {
        $uiDirectory = Join-Path $artifactDirectory 'ui'
        New-Item -ItemType Directory -Force -Path $uiDirectory | Out-Null
        $terminalData = Join-Path $uiDirectory 'serial-terminal.json'
        $darkData = Join-Path $uiDirectory 'modbus-en-dark.json'
        @{
            'work-mode' = 1
            connected = $true
            'terminal-text' = "root@linux:~# uname -a`nLinux target 6.8.0`nroot@linux:~# "
            'terminal-command' = "echo first`necho second"
        } | ConvertTo-Json | Set-Content -Encoding UTF8 $terminalData
        @{
            dark = $true
            'lang-en' = $true
        } | ConvertTo-Json | Set-Content -Encoding UTF8 $darkData
        Invoke-NativeCommand slint-viewer @(
            'ui/app.slint', '--component', 'AppWindow',
            '--screenshot', (Join-Path $uiDirectory 'can.png'),
            '--backend', 'software'
        )
        $serialWarmup = Join-Path $uiDirectory 'serial-normal-warmup.png'
        Invoke-NativeCommand slint-viewer @(
            'serial/ui/app.slint', '--component', 'AppWindow',
            '--screenshot', $serialWarmup, '--backend', 'software'
        )
        Invoke-NativeCommand slint-viewer @(
            'serial/ui/app.slint', '--component', 'AppWindow',
            '--screenshot', (Join-Path $uiDirectory 'serial-normal.png'),
            '--backend', 'software'
        )
        Remove-Item -LiteralPath $serialWarmup
        Invoke-NativeCommand slint-viewer @(
            'serial/ui/app.slint', '--component', 'AppWindow',
            '--load-data', $terminalData,
            '--screenshot', (Join-Path $uiDirectory 'serial-terminal.png'),
            '--backend', 'software'
        )
        Invoke-NativeCommand slint-viewer @(
            'serial/ui/app.slint', '--component', 'AppWindow',
            '--load-data', $darkData,
            '--screenshot', (Join-Path $uiDirectory 'serial-en-dark.png'),
            '--backend', 'software'
        )
        Invoke-NativeCommand slint-viewer @(
            'modbus/ui/app.slint', '--component', 'AppWindow',
            '--screenshot', (Join-Path $uiDirectory 'modbus-zh-light.png'),
            '--backend', 'software'
        )
        Invoke-NativeCommand slint-viewer @(
            'modbus/ui/app.slint', '--component', 'AppWindow',
            '--load-data', $darkData,
            '--screenshot', (Join-Path $uiDirectory 'modbus-en-dark.png'),
            '--backend', 'software'
        )
    }
    Invoke-GateStep 'Workspace tests' { cargo test --locked --workspace --all-targets --jobs 1 }
    Invoke-GateStep 'Clippy zero-warning audit' {
        cargo clippy --locked --workspace --all-targets --jobs 1 -- -D warnings
    }
    Invoke-GateStep 'Visible-selection regression' {
        Push-Location (Join-Path $repository 'vendor\slint\i-slint-core')
        try {
            Invoke-NativeCommand cargo @(
                'test', '--locked',
                'text_input_selection_is_clipped_to_visible_viewport',
                '--jobs', '1'
            )
        }
        finally {
            Pop-Location
        }
    }

    $env:PCANWORK_SOAK_SECONDS = [string]($SoakHours * 60 * 60)
    $env:PCANWORK_SOAK_FPS = [string]$CaptureFramesPerSecond
    Invoke-GateStep "Capture soak ${SoakHours}h @ ${CaptureFramesPerSecond} fps" {
        cargo test --locked -p pcanwork capture_queue_soak_has_no_hidden_loss --jobs 1 -- --ignored --nocapture
    }

    if (-not $SkipRelease) {
        Invoke-GateStep 'Release workspace build' {
            cargo build --locked --workspace --release --jobs 1
        }
    }

    if ($CertificateThumbprint) {
        Invoke-GateStep 'Signed release and installer' {
            & (Join-Path $repository 'installer\build-signed-release.ps1') `
                -CertificateThumbprint $CertificateThumbprint
        }
    }

    $summary = [ordered]@{
        completed_at = (Get-Date).ToString('o')
        soak_hours = $SoakHours
        capture_frames_per_second = $CaptureFramesPerSecond
        release_built = -not $SkipRelease
        signed = [bool]$CertificateThumbprint
        log = $transcript
    }
    $summary | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $artifactDirectory 'summary.json')
    Write-Host "All requested gates passed. Evidence: $artifactDirectory"
}
finally {
    Remove-Item Env:PCANWORK_SOAK_SECONDS -ErrorAction SilentlyContinue
    Remove-Item Env:PCANWORK_SOAK_FPS -ErrorAction SilentlyContinue
    Pop-Location
}
