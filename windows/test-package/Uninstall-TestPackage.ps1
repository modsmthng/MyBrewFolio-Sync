# SPDX-License-Identifier: GPL-3.0-or-later

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$identityName = "__IDENTITY_NAME__"
$certificatePath = Join-Path $root "MyBrewFolio-Sync-Store-Test.cer"

Get-AppxPackage -Name $identityName | Remove-AppxPackage -ErrorAction SilentlyContinue
$certificate = New-Object System.Security.Cryptography.X509Certificates.X509Certificate2($certificatePath)
$thumbprint = $certificate.Thumbprint
Remove-Item "Cert:\CurrentUser\TrustedPeople\$thumbprint" -Force -ErrorAction SilentlyContinue
Write-Host "MyBrewFolio Sync Store test package and temporary certificate removed."
