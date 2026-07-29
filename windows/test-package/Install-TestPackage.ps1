# SPDX-License-Identifier: GPL-3.0-or-later

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$identityName = "__IDENTITY_NAME__"
$certificatePath = Join-Path $root "MyBrewFolio-Sync-Store-Test.cer"
$msixPath = Join-Path $root "MyBrewFolio-Sync-Store-Test.msix"
$vclibsPath = Join-Path $root "Microsoft.VCLibs.x64.14.00.Desktop.appx"

Write-Host "Installing the temporary test certificate for the current user..."
$certificate = Import-Certificate -FilePath $certificatePath -CertStoreLocation "Cert:\CurrentUser\TrustedPeople"
if (-not $certificate) { throw "The temporary test certificate could not be installed" }

Write-Host "Installing Microsoft Visual C++ framework dependency..."
$installedVclibs = Get-AppxPackage -Name "Microsoft.VCLibs.140.00.UWPDesktop"
if (-not $installedVclibs) {
  Add-AppxPackage -Path $vclibsPath -ErrorAction Stop
} else {
  Write-Host "Microsoft Visual C++ framework is already installed."
}

Write-Host "Installing the MyBrewFolio Sync Store test package..."
Get-AppxPackage -Name $identityName | Remove-AppxPackage -ErrorAction SilentlyContinue
Add-AppxPackage -Path $msixPath -DependencyPath $vclibsPath -ErrorAction Stop
$package = Get-AppxPackage -Name $identityName
if (-not $package) { throw "The MyBrewFolio Sync Store test package was not installed" }

Write-Host "Launching MyBrewFolio Sync..."
Start-Process explorer.exe "shell:AppsFolder\$($package.PackageFamilyName)!MyBrewFolioSync"
Write-Host "Test package installed. Run Uninstall-TestPackage.ps1 after testing."
