# SPDX-License-Identifier: GPL-3.0-or-later

param(
  [Parameter(Mandatory = $true)][string]$Msix,
  [Parameter(Mandatory = $true)][string]$Publisher,
  [Parameter(Mandatory = $true)][string]$IdentityName,
  [string]$Output = "MyBrewFolio-Sync-Store-Test.zip"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$bundle = Join-Path $env:RUNNER_TEMP "mybrewfolio-sync-msix-test"
if (Test-Path $bundle) { Remove-Item -Recurse -Force $bundle }
New-Item -ItemType Directory -Path $bundle | Out-Null

$signedMsix = Join-Path $bundle "MyBrewFolio-Sync-Store-Test.msix"
Copy-Item $Msix $signedMsix

$certificate = New-SelfSignedCertificate `
  -Subject $Publisher `
  -Type CodeSigningCert `
  -KeyAlgorithm RSA `
  -KeyLength 2048 `
  -HashAlgorithm SHA256 `
  -KeyExportPolicy NonExportable `
  -FriendlyName "MyBrewFolio Sync temporary MSIX test certificate" `
  -CertStoreLocation "Cert:\CurrentUser\My" `
  -NotAfter (Get-Date).AddDays(14)

try {
  $certificatePath = Join-Path $bundle "MyBrewFolio-Sync-Store-Test.cer"
  Export-Certificate -Cert $certificate -FilePath $certificatePath | Out-Null

  $signTool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" |
    Sort-Object FullName -Descending |
    Select-Object -First 1
  if (-not $signTool) { throw "signtool.exe was not found" }
  & $signTool.FullName sign /fd SHA256 /sha1 $certificate.Thumbprint $signedMsix
  if ($LASTEXITCODE -ne 0) { throw "The local MSIX test package could not be signed" }
  $verificationOutput = (& $signTool.FullName verify /pa /v $signedMsix 2>&1 | Out-String)
  $verificationExitCode = $LASTEXITCODE
  if ($verificationExitCode -ne 0) {
    $hasExpectedSelfSignedTrustError = $verificationOutput -match "(?is)(0x800B0109|root\s+certificate\s+which\s+is\s+not\s+trusted\s+by\s+the\s+trust\s+provider)"
    if (-not $hasExpectedSelfSignedTrustError) {
      Write-Host $verificationOutput
      throw "The local MSIX test signature could not be verified"
    }
    Write-Host "The test MSIX signature is intact and uses the expected self-signed test certificate."
  } else {
    Write-Host "The test MSIX signature was verified successfully."
  }

  $embeddedSignature = Get-AuthenticodeSignature -FilePath $signedMsix
  if (-not $embeddedSignature.SignerCertificate) {
    throw "The signed test MSIX does not expose a signing certificate"
  }
  if ($embeddedSignature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) {
    throw "The test MSIX signer does not match the bundled public certificate"
  }

  $vclibsCandidates = @(
    "${env:ProgramFiles(x86)}\Microsoft SDKs\Windows Kits\10\ExtensionSDKs\Microsoft.VCLibs.Desktop\*\Appx\Retail\x64\*.appx",
    "${env:ProgramFiles(x86)}\Microsoft SDKs\Windows Kits\10\ExtensionSDKs\Microsoft.VCLibs\*\Appx\Retail\x64\*.appx"
  )
  $vclibs = Get-ChildItem $vclibsCandidates -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending |
    Select-Object -First 1
  if (-not $vclibs) { throw "The x64 Microsoft VCLibs Desktop framework package was not found" }
  Copy-Item $vclibs.FullName (Join-Path $bundle "Microsoft.VCLibs.x64.14.00.Desktop.appx")

  Copy-Item (Join-Path $root "windows\test-package\Install-TestPackage.ps1") $bundle
  Copy-Item (Join-Path $root "windows\test-package\Uninstall-TestPackage.ps1") $bundle
  Copy-Item (Join-Path $root "windows\test-package\README.txt") $bundle
  (Get-Content (Join-Path $bundle "Install-TestPackage.ps1") -Raw).Replace("__IDENTITY_NAME__", $IdentityName) |
    Set-Content (Join-Path $bundle "Install-TestPackage.ps1") -Encoding utf8
  (Get-Content (Join-Path $bundle "Uninstall-TestPackage.ps1") -Raw).Replace("__IDENTITY_NAME__", $IdentityName) |
    Set-Content (Join-Path $bundle "Uninstall-TestPackage.ps1") -Encoding utf8

  $requiredBundleFiles = @(
    "MyBrewFolio-Sync-Store-Test.msix",
    "MyBrewFolio-Sync-Store-Test.cer",
    "Microsoft.VCLibs.x64.14.00.Desktop.appx",
    "Install-TestPackage.ps1",
    "Uninstall-TestPackage.ps1",
    "README.txt"
  )
  foreach ($relativePath in $requiredBundleFiles) {
    if (-not (Test-Path (Join-Path $bundle $relativePath))) {
      throw "The MSIX test bundle is missing $relativePath"
    }
  }

  Compress-Archive -Path (Join-Path $bundle "*") -DestinationPath $Output -Force

  $verificationDirectory = Join-Path $env:RUNNER_TEMP "mybrewfolio-sync-msix-test-verification"
  if (Test-Path $verificationDirectory) {
    Remove-Item -Recurse -Force $verificationDirectory
  }
  Expand-Archive -Path $Output -DestinationPath $verificationDirectory -Force
  foreach ($relativePath in $requiredBundleFiles) {
    if (-not (Test-Path (Join-Path $verificationDirectory $relativePath))) {
      throw "The generated MSIX test ZIP is missing $relativePath"
    }
  }

  Write-Host "Created locally installable MSIX test bundle: $Output"
} finally {
  Remove-Item "Cert:\CurrentUser\My\$($certificate.Thumbprint)" -Force -ErrorAction SilentlyContinue
}
