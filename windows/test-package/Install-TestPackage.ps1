# SPDX-License-Identifier: GPL-3.0-or-later

param(
  [switch]$InstallCertificateOnly
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$identityName = "__IDENTITY_NAME__"
$certificatePath = Join-Path $root "MyBrewFolio-Sync-Store-Test.cer"
$msixPath = Join-Path $root "MyBrewFolio-Sync-Store-Test.msix"
$vclibsPath = Join-Path $root "Microsoft.VCLibs.x64.14.00.Desktop.appx"
$expectedVclibsName = "Microsoft.VCLibs.140.00.UWPDesktop"
$expectedVclibsPublisher = "CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US"
$minimumVclibsVersion = [version]"14.0.24217.0"

function Get-AppxIdentity([string]$Path) {
  Add-Type -AssemblyName System.IO.Compression.FileSystem | Out-Null
  $resolvedPath = (Resolve-Path $Path).Path
  $archive = [System.IO.Compression.ZipFile]::OpenRead($resolvedPath)
  try {
    $manifestEntry = $archive.GetEntry("AppxManifest.xml")
    if (-not $manifestEntry) {
      throw "$resolvedPath does not contain AppxManifest.xml"
    }
    $reader = [System.IO.StreamReader]::new($manifestEntry.Open())
    try {
      [xml]$manifest = $reader.ReadToEnd()
    } finally {
      $reader.Dispose()
    }
  } finally {
    $archive.Dispose()
  }

  $namespace = New-Object System.Xml.XmlNamespaceManager($manifest.NameTable)
  $namespace.AddNamespace("f", "http://schemas.microsoft.com/appx/manifest/foundation/windows10")
  $identity = $manifest.SelectSingleNode("/f:Package/f:Identity", $namespace)
  if (-not $identity) {
    throw "$resolvedPath does not declare a package identity"
  }

  return [PSCustomObject]@{
    Name = [string]$identity.Name
    Publisher = [string]$identity.Publisher
    Version = [version]$identity.Version
    Architecture = [string]$identity.ProcessorArchitecture
  }
}

function Test-IsAdministrator {
  $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
  $principal = New-Object Security.Principal.WindowsPrincipal($identity)
  return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Get-BundledCertificate {
  if (-not (Test-Path $certificatePath)) {
    throw "The bundled test certificate is missing. Extract the complete ZIP before running this script."
  }
  return [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($certificatePath)
}

function Install-MachineCertificate {
  if (-not (Test-IsAdministrator)) {
    throw "Administrator rights are required only for trusting the temporary test certificate."
  }

  $certificate = Get-BundledCertificate
  $machineCertificatePath = "Cert:\LocalMachine\TrustedPeople\$($certificate.Thumbprint)"
  if (-not (Test-Path $machineCertificatePath)) {
    Import-Certificate `
      -FilePath $certificatePath `
      -CertStoreLocation "Cert:\LocalMachine\TrustedPeople" | Out-Null
  }

  if (-not (Test-Path $machineCertificatePath)) {
    throw "The temporary certificate was not added to Local Computer > Trusted People."
  }
  Write-Host "Temporary test certificate trusted for this computer."
}

function Invoke-ElevatedCertificateInstall {
  $arguments = @(
    "-NoProfile",
    "-ExecutionPolicy", "Bypass",
    "-File", "`"$PSCommandPath`"",
    "-InstallCertificateOnly"
  )

  try {
    $process = Start-Process `
      -FilePath "powershell.exe" `
      -Verb RunAs `
      -ArgumentList $arguments `
      -Wait `
      -PassThru
  } catch {
    throw "The administrator confirmation was cancelled. The test certificate was not installed."
  }

  if ($process.ExitCode -ne 0) {
    throw "The elevated certificate installation failed with exit code $($process.ExitCode)."
  }
}

function Write-AppPackageDiagnostics([System.Management.Automation.ErrorRecord]$errorRecord) {
  Write-Host ""
  Write-Host "Windows package deployment failed:"
  Write-Host ($errorRecord | Out-String)

  $activityMatch = [regex]::Match(
    $errorRecord.Exception.Message,
    "(?i)ActivityId\]\s*([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"
  )
  if ($activityMatch.Success) {
    $activityId = [Guid]$activityMatch.Groups[1].Value
    Write-Host "App package deployment log for ActivityId ${activityId}:"
    try {
      $deploymentLog = Get-AppPackageLog -ActivityID $activityId |
        Format-List |
        Out-String
      Write-Host $deploymentLog
    } catch {
      Write-Host "The detailed AppxDeployment log could not be read: $($_.Exception.Message)"
    }
  }
}

if ($InstallCertificateOnly) {
  Install-MachineCertificate
  return
}

foreach ($requiredPath in @($certificatePath, $msixPath, $vclibsPath)) {
  if (-not (Test-Path $requiredPath)) {
    throw "Required test bundle file is missing: $requiredPath. Extract the complete ZIP first."
  }
}

$vclibsIdentity = Get-AppxIdentity $vclibsPath
if (
  $vclibsIdentity.Name -ne $expectedVclibsName -or
  $vclibsIdentity.Publisher -ne $expectedVclibsPublisher -or
  $vclibsIdentity.Architecture -ne "x64" -or
  $vclibsIdentity.Version -lt $minimumVclibsVersion
) {
  throw (
    "The test ZIP contains the wrong Visual C++ framework package. " +
    "Expected $expectedVclibsName x64 version $minimumVclibsVersion or newer, " +
    "but found $($vclibsIdentity.Name) $($vclibsIdentity.Version) $($vclibsIdentity.Architecture)."
  )
}
Write-Host "Verified Visual C++ framework: $($vclibsIdentity.Name) $($vclibsIdentity.Version)."

$certificate = Get-BundledCertificate
$untrustedSignature = Get-AuthenticodeSignature -FilePath $msixPath
if (-not $untrustedSignature.SignerCertificate) {
  throw "The test MSIX does not contain a readable signing certificate."
}
if ($untrustedSignature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) {
  throw "The bundled certificate does not match the certificate that signed the test MSIX."
}

$machineCertificatePath = "Cert:\LocalMachine\TrustedPeople\$($certificate.Thumbprint)"
if (-not (Test-Path $machineCertificatePath)) {
  Write-Host "Windows needs one administrator confirmation to trust the temporary test certificate."
  if (Test-IsAdministrator) {
    Install-MachineCertificate
  } else {
    Invoke-ElevatedCertificateInstall
  }
}

if (-not (Test-Path $machineCertificatePath)) {
  throw "The temporary certificate is not present in Local Computer > Trusted People."
}

$trustedSignature = Get-AuthenticodeSignature -FilePath $msixPath
if ($trustedSignature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
  $signatureFailure = "$($trustedSignature.Status) - $($trustedSignature.StatusMessage)"
  throw "The test MSIX signature is not trusted after certificate installation: $signatureFailure"
}
Write-Host "Test package signature is valid and trusted."

$compatibleVclibs = Get-AppxPackage -Name $expectedVclibsName |
  Where-Object {
    $_.Architecture -eq "X64" -and
    [version]$_.Version -ge $minimumVclibsVersion
  }
$installParameters = @{
  Path = $msixPath
  ErrorAction = "Stop"
}
if ($compatibleVclibs) {
  Write-Host "A compatible Microsoft Visual C++ framework is already installed."
} else {
  Write-Host "Microsoft Visual C++ framework will be installed with the test package."
  $installParameters.DependencyPath = $vclibsPath
}

Write-Host "Installing the MyBrewFolio Sync Store test package..."
Get-AppxPackage -Name $identityName | Remove-AppxPackage -ErrorAction SilentlyContinue
try {
  Add-AppxPackage @installParameters
} catch {
  Write-AppPackageDiagnostics $_
  throw
}

$package = Get-AppxPackage -Name $identityName
if (-not $package) { throw "The MyBrewFolio Sync Store test package was not installed" }

Write-Host "Package registered successfully for $env:USERNAME."
Write-Host "Launching MyBrewFolio Sync..."
Start-Process explorer.exe "shell:AppsFolder\$($package.PackageFamilyName)!MyBrewFolioSync"
Write-Host "Test package installed. Run Uninstall-TestPackage.ps1 after testing."
