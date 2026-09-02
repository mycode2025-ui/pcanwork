param(
    [Parameter(Mandatory = $true)]
    [string]$Executable,
    [Parameter(Mandatory = $true)]
    [ValidateSet('pcanwork', 'modbus')]
    [string]$Product,
    [Parameter(Mandatory = $true)]
    [string]$AppVersion,
    [string]$PrivateKey = 'D:\_LicenseSecrets\PcanWork\pcanwork-ed25519-private.pem',
    [string]$OutputPath = ''
)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path -LiteralPath $Executable).Path
$keyPath = (Resolve-Path -LiteralPath $PrivateKey).Path
if (-not $OutputPath) {
    $OutputPath = "$exePath.integrity"
}

$payload = [ordered]@{
    version = 1
    product = $Product
    app_version = $AppVersion
    file_name = [IO.Path]::GetFileName($exePath)
    sha256 = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash
}
$payloadJson = $payload | ConvertTo-Json -Compress
$utf8 = [Text.UTF8Encoding]::new($false)
$tempRoot = Join-Path ([IO.Path]::GetTempPath()) ("pcanwork-integrity-" + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($tempRoot) | Out-Null
try {
    $payloadFile = Join-Path $tempRoot 'payload.json'
    $signatureFile = Join-Path $tempRoot 'signature.bin'
    [IO.File]::WriteAllText($payloadFile, $payloadJson, $utf8)
    & openssl pkeyutl -sign -rawin -inkey $keyPath -in $payloadFile -out $signatureFile
    if ($LASTEXITCODE -ne 0) { throw 'OpenSSL failed to sign the integrity payload.' }
    $payloadHex = -join ([IO.File]::ReadAllBytes($payloadFile) | ForEach-Object { $_.ToString('X2') })
    $signatureHex = -join ([IO.File]::ReadAllBytes($signatureFile) | ForEach-Object { $_.ToString('X2') })
    $envelope = [ordered]@{
        format = 'pcanwork-integrity-ed25519-v1'
        payload = $payloadHex
        signature = $signatureHex
    } | ConvertTo-Json -Compress
    [IO.File]::WriteAllText([IO.Path]::GetFullPath($OutputPath), $envelope, $utf8)
    Write-Output ([IO.Path]::GetFullPath($OutputPath))
}
finally {
    Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
