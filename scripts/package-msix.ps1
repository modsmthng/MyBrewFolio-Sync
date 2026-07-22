# SPDX-License-Identifier: GPL-3.0-or-later

param(
  [Parameter(Mandatory = $true)][string]$IdentityName,
  [Parameter(Mandatory = $true)][string]$Publisher,
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$Executable,
  [string]$Output = "MyBrewFolio-Sync-Store.msix"
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$staging = Join-Path $env:RUNNER_TEMP "mybrewfolio-sync-msix"
if (Test-Path $staging) { Remove-Item -Recurse -Force $staging }
New-Item -ItemType Directory -Path (Join-Path $staging "Assets") | Out-Null
Copy-Item $Executable (Join-Path $staging "MyBrewFolioSync.exe")

$manifest = Get-Content (Join-Path $root "windows\Package.appxmanifest.xml") -Raw
$manifest = $manifest.Replace("__IDENTITY_NAME__", $IdentityName)
$manifest = $manifest.Replace("__PUBLISHER__", $Publisher.Replace("&", "&amp;").Replace('"', "&quot;"))
$manifest = $manifest.Replace("__VERSION__", $Version)
Set-Content -Path (Join-Path $staging "AppxManifest.xml") -Value $manifest -Encoding utf8

Add-Type -AssemblyName System.Drawing
$sourcePath = Join-Path $root "src-tauri\icons\icon.png"
function Write-Logo([int]$width, [int]$height, [string]$name) {
  $source = [System.Drawing.Image]::FromFile($sourcePath)
  try {
    $bitmap = New-Object System.Drawing.Bitmap($width, $height)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    try {
      $graphics.Clear([System.Drawing.Color]::Transparent)
      $size = [Math]::Min($width, $height)
      $x = [Math]::Floor(($width - $size) / 2)
      $y = [Math]::Floor(($height - $size) / 2)
      $graphics.DrawImage($source, $x, $y, $size, $size)
      $bitmap.Save((Join-Path $staging "Assets\$name"), [System.Drawing.Imaging.ImageFormat]::Png)
    } finally {
      $graphics.Dispose()
      $bitmap.Dispose()
    }
  } finally {
    $source.Dispose()
  }
}

Write-Logo 50 50 "StoreLogo.png"
Write-Logo 44 44 "Square44x44Logo.png"
Write-Logo 150 150 "Square150x150Logo.png"
Write-Logo 310 150 "Wide310x150Logo.png"

$makeAppx = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\makeappx.exe" |
  Sort-Object FullName -Descending |
  Select-Object -First 1
if (-not $makeAppx) { throw "makeappx.exe was not found" }
& $makeAppx.FullName pack /d $staging /p $Output /o
if ($LASTEXITCODE -ne 0) { throw "MSIX packaging failed" }
Write-Host "Created Store submission package: $Output"
