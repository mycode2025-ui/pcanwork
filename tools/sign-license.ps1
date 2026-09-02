param(
    [string]$MachineCode = '',

    [switch]$Unbound,

    [string]$LicenseId = ("HB-" + (Get-Date -Format 'yyyyMMdd-HHmmss')),

    [string[]]$Products = @('pcanwork', 'modbus'),

    [string[]]$Features = @('*'),

    [ValidateRange(0, 36500)]
    [int]$ValidDays = 0,

    [ValidateRange(0, 876000)]
    [int]$ValidHours = 0,

    [string]$PrivateKey = 'D:\_LicenseSecrets\PcanWork\pcanwork-ed25519-private.pem',

    [string]$OutputPath = '.\license.pcanlic'
)

$ErrorActionPreference = 'Stop'

if ($ValidDays -gt 0 -and $ValidHours -gt 0) {
    throw 'Specify only one of -ValidDays or -ValidHours.'
}
if (-not $Unbound -and [string]::IsNullOrWhiteSpace($MachineCode)) {
    throw 'MachineCode is required unless -Unbound is specified.'
}

if (-not (Test-Path -LiteralPath $PrivateKey -PathType Leaf)) {
    throw "Ed25519 private key not found: $PrivateKey"
}

$openssl = (Get-Command openssl.exe -ErrorAction SilentlyContinue).Source
if (-not $openssl) {
    $candidate = 'C:\Strawberry\c\bin\openssl.exe'
    if (Test-Path -LiteralPath $candidate) {
        $openssl = $candidate
    } else {
        throw 'OpenSSL 3.x is required to sign .pcanlic files.'
    }
}

$issuedAt = [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$expiresAt = if ($ValidHours -gt 0) {
    $issuedAt + ([int64]$ValidHours * 3600)
} elseif ($ValidDays -gt 0) {
    $issuedAt + ([int64]$ValidDays * 86400)
} else {
    0
}
$nonceBytes = [byte[]]::new(16)
$rng = [Security.Cryptography.RandomNumberGenerator]::Create()
try { $rng.GetBytes($nonceBytes) } finally { $rng.Dispose() }
$nonce = -join ($nonceBytes | ForEach-Object { $_.ToString('X2') })
$machineGrouped = if ($Unbound) {
    '*'
} else {
    $normalizedMachine = ($MachineCode -replace '[^0-9A-Za-z]', '').ToUpperInvariant()
    ($normalizedMachine -split '(.{4})' | Where-Object { $_ }) -join '-'
}

$payloadObject = [ordered]@{
    version = 1
    license_id = $LicenseId
    machine_code = $machineGrouped
    products = @($Products | ForEach-Object { $_.ToLowerInvariant() })
    features = @($Features | ForEach-Object { $_.ToLowerInvariant() })
    issued_at = $issuedAt
    expires_at = $expiresAt
    nonce = $nonce
}
$payloadJson = $payloadObject | ConvertTo-Json -Compress -Depth 8
$payloadBytes = [Text.Encoding]::UTF8.GetBytes($payloadJson)

$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("pcanwork-license-" + [Guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($temporaryDirectory) | Out-Null
$payloadFile = Join-Path $temporaryDirectory 'payload.bin'
$signatureFile = Join-Path $temporaryDirectory 'signature.bin'
try {
    [IO.File]::WriteAllBytes($payloadFile, $payloadBytes)
    & $openssl pkeyutl -sign -rawin -inkey $PrivateKey -in $payloadFile -out $signatureFile
    if ($LASTEXITCODE -ne 0) {
        throw "OpenSSL signing failed with exit code $LASTEXITCODE"
    }
    $signatureBytes = [IO.File]::ReadAllBytes($signatureFile)
} finally {
    if (Test-Path -LiteralPath $payloadFile) { Remove-Item -LiteralPath $payloadFile -Force }
    if (Test-Path -LiteralPath $signatureFile) { Remove-Item -LiteralPath $signatureFile -Force }
    if (Test-Path -LiteralPath $temporaryDirectory) { Remove-Item -LiteralPath $temporaryDirectory -Force }
}

$payloadHex = -join ($payloadBytes | ForEach-Object { $_.ToString('X2') })
$signatureHex = -join ($signatureBytes | ForEach-Object { $_.ToString('X2') })
$envelope = [ordered]@{
    format = 'pcanlic-ed25519-v1'
    payload = $payloadHex
    signature = $signatureHex
}
$outputFullPath = [IO.Path]::GetFullPath($OutputPath)
$outputDirectory = [IO.Path]::GetDirectoryName($outputFullPath)
[IO.Directory]::CreateDirectory($outputDirectory) | Out-Null
[IO.File]::WriteAllText($outputFullPath, ($envelope | ConvertTo-Json -Depth 4), [Text.UTF8Encoding]::new($false))

[pscustomobject]@{
    LicenseFile = $outputFullPath
    LicenseId = $LicenseId
    MachineCode = $machineGrouped
    Binding = if ($Unbound) { 'Unbound (all CPUs)' } else { 'CPU-bound' }
    Products = ($Products -join ',')
    Features = ($Features -join ',')
    ExpiresAt = if ($expiresAt -eq 0) { 'Permanent' } else { [DateTimeOffset]::FromUnixTimeSeconds($expiresAt).UtcDateTime.ToString('u') }
}
