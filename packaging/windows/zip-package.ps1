# Build the Windows release zip with entry names separated by '/'.
#
# Compress-Archive was used here until 2026-11-03. Its entry-name separator
# depends on the PowerShell version (5.1 writes '\', 7.x writes '/'), and a
# package that changes shape with the runner is not something an updater can
# reason about: clients at 0.7.7/0.7.8 refuse any name containing a backslash,
# so a 5.1-built package would have been uninstallable. Writing the archive
# explicitly makes the output identical on any PowerShell.
param(
  [Parameter(Mandatory = $true)][string]$Source,
  [Parameter(Mandatory = $true)][string]$Destination
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$root = (Resolve-Path -LiteralPath $Source).Path
if (Test-Path -LiteralPath $Destination) { Remove-Item -LiteralPath $Destination -Force }
$out = [System.IO.Compression.ZipFile]::Open($Destination, 'Create')
try {
  Get-ChildItem -LiteralPath $root -Recurse -File | ForEach-Object {
    $rel = $_.FullName.Substring($root.Length + 1).Replace('\', '/')
    [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
      $out, $_.FullName, $rel, [System.IO.Compression.CompressionLevel]::Optimal)
  }
} finally {
  $out.Dispose()
}
Write-Output "wrote $Destination"
