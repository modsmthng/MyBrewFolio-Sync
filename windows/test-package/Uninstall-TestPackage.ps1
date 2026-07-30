# SPDX-License-Identifier: GPL-3.0-or-later

param(
  [switch]$RemoveCertificateOnly
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$identityName = "__IDENTITY_NAME__"
$certificatePath = Join-Path $root "MyBrewFolio-Sync-Store-Test.cer"

function Test-IsAdministrator {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-BundledCertificate {
  if (-not (Test-Path $certificatePath)) {
    throw "The bundled test certificate is missing. Keep the extracted ZIP until cleanup is complete."
  }
  return [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($certificatePath)
}

function Remove-MachineCertificate {
  if (-not (Test-IsAdministrator)) {
    throw "Administrator rights are required only for removing the temporary test certificate."
  }

  $certificate = Get-BundledCertificate
  Remove-Item `
    "Cert:\LocalMachine\TrustedPeople\$($certificate.Thumbprint)" `
    -Force `
    -ErrorAction SilentlyContinue
  Write-Host "Temporary test certificate removed from this computer."
}

function Invoke-ElevatedCertificateRemoval {
  $arguments = @(
    "-NoProfile",
    "-ExecutionPolicy", "Bypass",
    "-File", "`"$PSCommandPath`"",
    "-RemoveCertificateOnly"
  )

  try {
    $process = Start-Process `
      -FilePath "powershell.exe" `
      -Verb RunAs `
      -ArgumentList $arguments `
      -Wait `
      -PassThru
  } catch {
    throw "The administrator confirmation was cancelled. The temporary test certificate remains installed."
  }

  if ($process.ExitCode -ne 0) {
    throw "The elevated certificate removal failed with exit code $($process.ExitCode)."
  }
}

if ($RemoveCertificateOnly) {
  Remove-MachineCertificate
  return
}

Get-AppxPackage -Name $identityName | Remove-AppxPackage -ErrorAction SilentlyContinue

$certificate = Get-BundledCertificate
$machineCertificatePath = "Cert:\LocalMachine\TrustedPeople\$($certificate.Thumbprint)"
if (Test-Path $machineCertificatePath) {
  Write-Host "Windows needs one administrator confirmation to remove the temporary test certificate."
  if (Test-IsAdministrator) {
    Remove-MachineCertificate
  } else {
    Invoke-ElevatedCertificateRemoval
  }
}

if (Get-AppxPackage -Name $identityName) {
  throw "The MyBrewFolio Sync Store test package is still registered for this user."
}
if (Test-Path $machineCertificatePath) {
  throw "The temporary test certificate is still present in Local Computer > Trusted People."
}

Write-Host "MyBrewFolio Sync Store test package and temporary certificate removed."
