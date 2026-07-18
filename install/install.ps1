# Rad installer for Windows.
#
#   powershell -ExecutionPolicy Bypass -c "irm https://radengine.dev/install.ps1 | iex"
#
# Environment:
#   $env:RAD_VERSION   install a specific version tag (default: latest release)
#   $env:RAD_INSTALL   install directory (default: %LOCALAPPDATA%\Programs\Rad)
#   $env:RAD_REPO      GitHub repo to download from (default: Southclaws/rad)
#
# Installs the single rad.exe — the database server, the devtool, and the
# codegen CLI in one.

$ErrorActionPreference = "Stop"

$Repo = if ($env:RAD_REPO) { $env:RAD_REPO } else { "Southclaws/rad" }
# Per-user application installs belong under LocalAppData, not in the user's
# home/Documents area. This remains writable without elevation, unlike Program
# Files, and follows the convention used by Windows installers.
$DefaultInstallDir = Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "Programs\Rad"
$InstallDir = if ($env:RAD_INSTALL) { $env:RAD_INSTALL } else { $DefaultInstallDir }
$BinDir = $InstallDir

$Arch = if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "amd64" }

$Asset = "rad-windows-$Arch.zip"
$Url = if ($env:RAD_VERSION) {
  "https://github.com/$Repo/releases/download/$($env:RAD_VERSION)/$Asset"
} else {
  "https://github.com/$Repo/releases/latest/download/$Asset"
}

Write-Host "Downloading $Url"
New-Item -ItemType Directory -Force -Path $BinDir | Out-Null
$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("rad-install-" + [System.Guid]::NewGuid())
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null

try {
  $Zip = Join-Path $Tmp $Asset
  Invoke-WebRequest -Uri $Url -OutFile $Zip -UseBasicParsing
  Expand-Archive -Path $Zip -DestinationPath $Tmp -Force
  Move-Item -Force (Join-Path $Tmp "rad.exe") (Join-Path $BinDir "rad.exe")
} finally {
  Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "rad was installed to $BinDir\rad.exe"
& (Join-Path $BinDir "rad.exe") --version

# Add to the user PATH if missing.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ";") -notcontains $BinDir) {
  [Environment]::SetEnvironmentVariable("Path", "$BinDir;$UserPath", "User")
  $env:Path = "$BinDir;$env:Path"
  Write-Host "Added $BinDir to your user `$Path."
}

Write-Host ""
Write-Host "Get started:"
Write-Host "  rad serve                 # start a database (RAD_STORAGE=memory|file|s3)"
Write-Host "  rad schema migrate        # uses rad.config.yaml and rad.schema.yaml"
Write-Host "  rad generate"
