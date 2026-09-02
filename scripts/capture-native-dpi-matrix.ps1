[CmdletBinding()]
param(
    [string]$OutputDirectory = 'D:\_Xcharge\Pcanwork\artifacts\native-dpi',
    [double[]]$ScaleFactors = @(1.0, 1.5, 2.0)
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$output = [System.IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Force -Path $output | Out-Null
Add-Type -AssemblyName System.Drawing
if (-not ('PcanWorkNativeDpi.WindowCapture' -as [type])) {
    Add-Type @'
using System;
using System.Runtime.InteropServices;
namespace PcanWorkNativeDpi {
    public static class WindowCapture {
        private delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr lParam);
        [StructLayout(LayoutKind.Sequential)]
        public struct Rect { public int Left, Top, Right, Bottom; }
        [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hwnd, out Rect rect);
        [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out Rect rect);
        [DllImport("user32.dll", SetLastError = true)] public static extern bool SetWindowPos(
            IntPtr hwnd, IntPtr insertAfter, int x, int y, int width, int height, uint flags);
        [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
        [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
        [DllImport("user32.dll")] private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr lParam);
        [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint processId);
        [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr hwnd);
        public static IntPtr FindLargestWindow(uint targetProcessId) {
            IntPtr best = IntPtr.Zero;
            long bestArea = 0;
            EnumWindows((hwnd, unused) => {
                uint processId;
                GetWindowThreadProcessId(hwnd, out processId);
                Rect rect;
                if (processId == targetProcessId && IsWindowVisible(hwnd) && GetWindowRect(hwnd, out rect)) {
                    long area = Math.Max(0, rect.Right - rect.Left) * (long)Math.Max(0, rect.Bottom - rect.Top);
                    if (area > bestArea) { best = hwnd; bestArea = area; }
                }
                return true;
            }, IntPtr.Zero);
            return best;
        }
    }
}
'@
}

function Resize-ClientArea([IntPtr]$Handle, [int]$TargetWidth, [int]$TargetHeight) {
    # SetWindowPos sizes the complete native window. Compensate for the current
    # non-client frame so the Slint client area has the requested pixel size.
    $windowRect = [PcanWorkNativeDpi.WindowCapture+Rect]::new()
    $clientRect = [PcanWorkNativeDpi.WindowCapture+Rect]::new()
    if (-not [PcanWorkNativeDpi.WindowCapture]::GetWindowRect($Handle, [ref]$windowRect) -or
        -not [PcanWorkNativeDpi.WindowCapture]::GetClientRect($Handle, [ref]$clientRect)) {
        throw 'Unable to measure native window before resizing.'
    }
    $frameWidth = ($windowRect.Right - $windowRect.Left) - ($clientRect.Right - $clientRect.Left)
    $frameHeight = ($windowRect.Bottom - $windowRect.Top) - ($clientRect.Bottom - $clientRect.Top)
    $flags = 0x0004 -bor 0x0010 # SWP_NOZORDER | SWP_NOACTIVATE
    # Keep verification windows outside the visible desktop. PrintWindow can
    # still capture them, while Release builds no longer flash 18 test windows.
    if (-not [PcanWorkNativeDpi.WindowCapture]::SetWindowPos(
        $Handle, [IntPtr]::Zero, -32000, -32000, $TargetWidth + $frameWidth, $TargetHeight + $frameHeight, $flags)) {
        throw 'SetWindowPos failed while preparing the DPI viewport.'
    }
    Start-Sleep -Milliseconds 250
    $actual = [PcanWorkNativeDpi.WindowCapture+Rect]::new()
    if (-not [PcanWorkNativeDpi.WindowCapture]::GetClientRect($Handle, [ref]$actual)) {
        throw 'Unable to measure native window after resizing.'
    }
    $actualWidth = $actual.Right - $actual.Left
    $actualHeight = $actual.Bottom - $actual.Top
    if ([Math]::Abs($actualWidth - $TargetWidth) -gt 2 -or [Math]::Abs($actualHeight - $TargetHeight) -gt 2) {
        throw "Native client resize mismatch: requested ${TargetWidth}x${TargetHeight}, got ${actualWidth}x${actualHeight}."
    }
}

function Capture-Window([IntPtr]$Handle, [string]$Path) {
    $rect = [PcanWorkNativeDpi.WindowCapture+Rect]::new()
    if (-not [PcanWorkNativeDpi.WindowCapture]::GetWindowRect($Handle, [ref]$rect)) {
        throw 'GetWindowRect failed.'
    }
    $width = $rect.Right - $rect.Left
    $height = $rect.Bottom - $rect.Top
    if ($width -lt 640 -or $height -lt 400) {
        throw "Native window is unexpectedly small: ${width}x${height}"
    }
    $bitmap = [System.Drawing.Bitmap]::new($width, $height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    try {
        if (-not [PcanWorkNativeDpi.WindowCapture]::PrintWindow($Handle, $hdc, 2)) {
            throw 'PrintWindow failed.'
        }
    }
    finally {
        $graphics.ReleaseHdc($hdc)
        $graphics.Dispose()
    }
    try {
        $bitmap.Save($Path, [System.Drawing.Imaging.ImageFormat]::Png)
    }
    finally {
        $bitmap.Dispose()
    }
    [pscustomobject]@{
        width = $width
        height = $height
        dpi = [PcanWorkNativeDpi.WindowCapture]::GetDpiForWindow($Handle)
    }
}

$components = @(
    [pscustomobject]@{ Name = 'can'; Path = 'ui\app.slint'; PreferredWidth = 1320; PreferredHeight = 820; MinimumWidth = 900; MinimumHeight = 560 },
    [pscustomobject]@{ Name = 'serial'; Path = 'serial\ui\app.slint'; PreferredWidth = 940; PreferredHeight = 580; MinimumWidth = 920; MinimumHeight = 540 },
    [pscustomobject]@{ Name = 'modbus'; Path = 'modbus\ui\app.slint'; PreferredWidth = 1200; PreferredHeight = 800; MinimumWidth = 1000; MinimumHeight = 660 }
)
$viewports = @('preferred', 'minimum')
$previousScale = $env:SLINT_SCALE_FACTOR
$evidence = [System.Collections.Generic.List[object]]::new()
try {
    foreach ($component in $components) {
        foreach ($viewport in $viewports) {
            foreach ($scale in $ScaleFactors) {
                $scaleText = $scale.ToString([Globalization.CultureInfo]::InvariantCulture)
                $env:SLINT_SCALE_FACTOR = $scaleText
                $logicalWidth = if ($viewport -eq 'preferred') { $component.PreferredWidth } else { $component.MinimumWidth }
                $logicalHeight = if ($viewport -eq 'preferred') { $component.PreferredHeight } else { $component.MinimumHeight }
                $clientWidth = [Math]::Round($logicalWidth * $scale)
                $clientHeight = [Math]::Round($logicalHeight * $scale)
                $name = "$($component.Name)-$viewport-scale-$($scaleText.Replace('.', '_')).png"
                $path = Join-Path $output $name
                $process = Start-Process -FilePath 'slint-viewer.exe' -ArgumentList @(
                    (Join-Path $root $component.Path), '--component', 'AppWindow',
                    '--backend', 'software'
                ) -PassThru -WindowStyle Hidden
                try {
                $handle = [IntPtr]::Zero
                for ($attempt = 0; $attempt -lt 100 -and $handle -eq [IntPtr]::Zero; $attempt++) {
                    Start-Sleep -Milliseconds 100
                    $candidate = [PcanWorkNativeDpi.WindowCapture]::FindLargestWindow($process.Id)
                    if ($candidate -ne [IntPtr]::Zero) {
                        $rect = [PcanWorkNativeDpi.WindowCapture+Rect]::new()
                        if ([PcanWorkNativeDpi.WindowCapture]::GetWindowRect($candidate, [ref]$rect) -and
                            ($rect.Right - $rect.Left) -ge 640 -and ($rect.Bottom - $rect.Top) -ge 400) {
                            $handle = $candidate
                        }
                    }
                }
                if ($handle -eq [IntPtr]::Zero) {
                    throw "Native window did not appear: $($component.Name) viewport=$viewport scale=$scaleText"
                }
                Resize-ClientArea $handle $clientWidth $clientHeight
                $capture = Capture-Window $handle $path
                $item = Get-Item -LiteralPath $path
                if ($item.Length -lt 4096) { throw "Native capture is blank: $path" }
                $evidence.Add([ordered]@{
                    component = $component.Name
                    viewport = $viewport
                    logical_width = $logicalWidth
                    logical_height = $logicalHeight
                    requested_client_width = $clientWidth
                    requested_client_height = $clientHeight
                    requested_scale = $scale
                    dpi = $capture.dpi
                    width = $capture.width
                    height = $capture.height
                    bytes = $item.Length
                    sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
                    file = $item.FullName
                })
            }
                finally {
                    if ($process -and -not $process.HasExited) {
                        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
                    }
                }
            }
        }
    }
}
finally {
    if ($null -eq $previousScale) {
        Remove-Item Env:SLINT_SCALE_FACTOR -ErrorAction SilentlyContinue
    } else {
        $env:SLINT_SCALE_FACTOR = $previousScale
    }
}

foreach ($component in $components) {
    foreach ($viewport in $viewports) {
        $rows = @($evidence | Where-Object { $_.component -eq $component.Name -and $_.viewport -eq $viewport })
        $signatures = @($rows | ForEach-Object { "$($_.width)x$($_.height):$($_.sha256)" } | Select-Object -Unique)
        if ($ScaleFactors.Count -gt 1 -and $signatures.Count -lt 2) {
            throw "Scale factor had no observable native rendering effect: $($component.Name) viewport=$viewport"
        }
    }
}
$evidence | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (
    Join-Path $output 'matrix.json'
) -Encoding utf8
Write-Host "Native DPI matrix passed: $output"
