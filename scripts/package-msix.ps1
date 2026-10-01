# SPDX-License-Identifier: GPL-3.0-or-later

param(
  [Parameter(Mandatory = $true)][string]$IdentityName,
  [Parameter(Mandatory = $true)][string]$Publisher,
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$Executable,
  [Parameter(Mandatory = $true)][string]$StartupExecutable,
  [string]$Output = "MyBrewFolio-Sync-Store.msix"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$staging = Join-Path $env:RUNNER_TEMP "mybrewfolio-sync-msix"
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
New-Item -ItemType Directory -Path (Join-Path $staging "Assets") | Out-Null
Copy-Item $Executable (Join-Path $staging "MyBrewFolioSync.exe")
Copy-Item $StartupExecutable (Join-Path $staging "MyBrewFolioSyncStartup.exe")

$manifest = Get-Content (Join-Path $root "windows\Package.appxmanifest.xml") -Raw
$manifest = $manifest.Replace("__IDENTITY_NAME__", $IdentityName)
$manifest = $manifest.Replace("__PUBLISHER__", $Publisher.Replace("&", "&amp;").Replace('"', "&quot;"))
$manifest = $manifest.Replace("__VERSION__", $Version)
Set-Content -Path (Join-Path $staging "AppxManifest.xml") -Value $manifest -Encoding utf8

[xml]$manifestXml = $manifest
$namespace = New-Object System.Xml.XmlNamespaceManager($manifestXml.NameTable)
$namespace.AddNamespace("f", "http://schemas.microsoft.com/appx/manifest/foundation/windows10")
$namespace.AddNamespace("desktop", "http://schemas.microsoft.com/appx/manifest/desktop/windows10")
$identity = $manifestXml.SelectSingleNode("/f:Package/f:Identity", $namespace)
if (-not $identity) { throw "The MSIX manifest does not contain a package identity" }
if ($identity.GetAttribute("Name") -ne $IdentityName) { throw "The MSIX identity name does not match the requested identity" }
if ($identity.GetAttribute("Publisher") -ne $Publisher) { throw "The MSIX publisher does not match the requested publisher" }
if ($identity.GetAttribute("Version") -ne $Version) { throw "The MSIX version does not match the requested version" }
if ($identity.GetAttribute("ProcessorArchitecture") -ne "x64") { throw "The Store MSIX must use the x64 architecture" }
if ($manifest -match "__[A-Z_]+__") { throw "The MSIX manifest still contains an unresolved placeholder" }
$vclibs = $manifestXml.SelectSingleNode("/f:Package/f:Dependencies/f:PackageDependency[@Name='Microsoft.VCLibs.140.00.UWPDesktop']", $namespace)
if (-not $vclibs) { throw "The MSIX manifest must declare Microsoft.VCLibs.140.00.UWPDesktop" }
$startupTask = $manifestXml.SelectSingleNode("/f:Package/f:Applications/f:Application/f:Extensions/desktop:Extension[@Category='windows.startupTask']/desktop:StartupTask[@TaskId='MyBrewFolioSyncStartup']", $namespace)
if (-not $startupTask) { throw "The MSIX manifest must declare the MyBrewFolio Sync startup task" }
$startupExtension = $startupTask.ParentNode
if ($startupExtension.GetAttribute("Executable") -ne "MyBrewFolioSyncStartup.exe") { throw "The MSIX startup task must launch MyBrewFolioSyncStartup.exe" }
if ($startupExtension.GetAttribute("EntryPoint") -ne "Windows.FullTrustApplication") { throw "The MSIX startup task must use Windows.FullTrustApplication" }
if ($startupTask.GetAttribute("Enabled") -ne "false") { throw "The MSIX startup task must remain opt-in" }
if ($startupTask.GetAttribute("DisplayName") -ne "MyBrewFolio Sync") { throw "The MSIX startup task must have the MyBrewFolio Sync display name" }

# Commit generated assets so packaging always includes the reviewed DPI variants.
$iconAssets = Join-Path $root "windows\Assets"
Copy-Item (Join-Path $iconAssets "*.png") (Join-Path $staging "Assets")

$dumpbinPath = $null
$dumpbinCommand = Get-Command "dumpbin.exe" -ErrorAction SilentlyContinue
if ($dumpbinCommand) {
  $dumpbinPath = $dumpbinCommand.Source
}

if (-not $dumpbinPath) {
  $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
  if (Test-Path $vswhere) {
    $dumpbinMatches = @(& $vswhere `
      -latest `
      -products * `
      -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 `
      -find "VC\Tools\MSVC\**\bin\Hostx64\x64\dumpbin.exe")
    if ($dumpbinMatches.Count -gt 0) {
      $dumpbinPath = $dumpbinMatches |
        Where-Object { $_ -and (Test-Path $_) } |
        Sort-Object -Descending |
        Select-Object -First 1
    }
  }
}

if (-not $dumpbinPath) {
  $dumpbin = Get-ChildItem "${env:ProgramFiles}\Microsoft Visual Studio\2022\*\VC\Tools\MSVC\*\bin\Hostx64\x64\dumpbin.exe" -ErrorAction SilentlyContinue |
    Sort-Object FullName -Descending |
    Select-Object -First 1
  if ($dumpbin) {
    $dumpbinPath = $dumpbin.FullName
  }
}

if (-not $dumpbinPath) { throw "dumpbin.exe was not found; the Store package cannot be verified" }
Write-Host "Using dumpbin.exe at $dumpbinPath"
if (-not (Test-Path $StartupExecutable)) { throw "The Store startup launcher does not exist: $StartupExecutable" }
$dependencies = (& $dumpbinPath /dependents $Executable 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0) { throw "Could not inspect Windows runtime dependencies" }
$needsVclibs = $dependencies -match "(?i)VCRUNTIME140(?:_1)?\.dll"
if ($needsVclibs -and -not $vclibs) { throw "The executable imports Visual C++ runtime DLLs without a VCLibs manifest dependency" }
$headers = (& $dumpbinPath /headers $Executable 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0 -or $headers -notmatch "(?i)Windows GUI") {
  throw "The Store executable must use the Windows GUI subsystem"
}
$startupDependencies = (& $dumpbinPath /dependents $StartupExecutable 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0) { throw "Could not inspect Windows startup launcher dependencies" }
if ($startupDependencies -match "(?i)VCRUNTIME140(?:_1)?\.dll" -and -not $vclibs) {
  throw "The startup launcher imports Visual C++ runtime DLLs without a VCLibs manifest dependency"
}
$startupHeaders = (& $dumpbinPath /headers $StartupExecutable 2>&1 | Out-String)
if ($LASTEXITCODE -ne 0 -or $startupHeaders -notmatch "(?i)Windows GUI") {
  throw "The Store startup launcher must use the Windows GUI subsystem"
}

$makeAppx = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" |
  Sort-Object FullName -Descending |
  Select-Object -First 1
if (-not $makeAppx) { throw "makeappx.exe was not found" }
& $makeAppx.FullName pack /d $staging /p $Output /o
if ($LASTEXITCODE -ne 0) { throw "MSIX packaging failed" }

$verification = Join-Path $env:RUNNER_TEMP "mybrewfolio-sync-msix-verification"
if (Test-Path $verification) { Remove-Item -Recurse -Force $verification }
& $makeAppx.FullName unpack /p $Output /d $verification /o | Out-Null
if ($LASTEXITCODE -ne 0) { throw "The generated MSIX could not be unpacked for verification" }
$requiredFiles = @(
  "AppxManifest.xml",
  "MyBrewFolioSync.exe",
  "MyBrewFolioSyncStartup.exe",
  "Assets\StoreLogo.png",
  "Assets\Square44x44Logo.png",
  "Assets\Square150x150Logo.png",
  "Assets\Wide310x150Logo.png",
  "Assets\Square44x44Logo.scale-200.png",
  "Assets\Square44x44Logo.scale-400.png",
  "Assets\Square150x150Logo.scale-200.png",
  "Assets\Square150x150Logo.scale-400.png",
  "Assets\Square44x44Logo.targetsize-256.png"
)
foreach ($relativePath in $requiredFiles) {
  if (-not (Test-Path (Join-Path $verification $relativePath))) {
    throw "The generated MSIX is missing $relativePath"
  }
}
[xml]$packedManifest = Get-Content (Join-Path $verification "AppxManifest.xml") -Raw
$packedNamespace = New-Object System.Xml.XmlNamespaceManager($packedManifest.NameTable)
$packedNamespace.AddNamespace("f", "http://schemas.microsoft.com/appx/manifest/foundation/windows10")
$packedNamespace.AddNamespace("desktop", "http://schemas.microsoft.com/appx/manifest/desktop/windows10")
$packedVclibs = $packedManifest.SelectSingleNode("/f:Package/f:Dependencies/f:PackageDependency[@Name='Microsoft.VCLibs.140.00.UWPDesktop']", $packedNamespace)
if (-not $packedVclibs) { throw "The packed MSIX lost its Visual C++ framework dependency" }
$packedStartupTask = $packedManifest.SelectSingleNode("/f:Package/f:Applications/f:Application/f:Extensions/desktop:Extension[@Category='windows.startupTask' and @Executable='MyBrewFolioSyncStartup.exe' and @EntryPoint='Windows.FullTrustApplication']/desktop:StartupTask[@TaskId='MyBrewFolioSyncStartup' and @Enabled='false' and @DisplayName='MyBrewFolio Sync']", $packedNamespace)
if (-not $packedStartupTask) { throw "The packed MSIX lost the opt-in MyBrewFolio Sync startup task" }
Write-Host "Created Store submission package: $Output"
