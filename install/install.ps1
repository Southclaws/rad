# Rad installer for Windows.
#
# From PowerShell:
#   irm https://radengine.dev/install.ps1 | iex
#
# From cmd.exe, a Run dialog, or another process:
#   powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://radengine.dev/install.ps1 | iex"
#
# Environment:
#   $env:RAD_VERSION   Install a specific release tag (default: latest release)
#   $env:RAD_INSTALL   Absolute install directory
#                      (default: %LOCALAPPDATA%\Programs\Rad)
#   $env:RAD_REPO      GitHub repository in owner/name form
#                      (default: Southclaws/rad)
#
# Installs the single rad.exe: database server, developer tool, and codegen CLI.

$ErrorActionPreference = "Stop"

# Windows PowerShell 5.1 on older .NET/Windows can default to obsolete TLS;
# enable TLS 1.2 without disabling anything already on.
[Net.ServicePointManager]::SecurityProtocol =
  [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$Repo = if ($env:RAD_REPO) { $env:RAD_REPO } else { "Southclaws/rad" }
if ($Repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') {
  throw "RAD_REPO must have the form owner/repository."
}

$Version = $env:RAD_VERSION
if ($Version -and $Version -notmatch '^[A-Za-z0-9._+-]+$') {
  throw "RAD_VERSION contains unsupported characters."
}

# LocalApplicationData is writable without elevation — a conventional per-user
# application location.
$DefaultInstallDir = Join-Path ([Environment]::GetFolderPath("LocalApplicationData")) "Programs\Rad"
$InstallDir = if ($env:RAD_INSTALL) { $env:RAD_INSTALL } else { $DefaultInstallDir }
if (-not [System.IO.Path]::IsPathRooted($InstallDir)) {
  throw "RAD_INSTALL must be an absolute path."
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
$TargetExe = Join-Path $InstallDir "rad.exe"

# PROCESSOR_ARCHITEW6432 reports the native OS architecture even from a 32-bit
# (WOW64) PowerShell; reject anything that isn't a shipped target rather than
# silently handing an x86 machine the amd64 binary.
$MachineArch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
switch ($MachineArch.ToUpperInvariant()) {
  "AMD64" { $Arch = "amd64" }
  "ARM64" { $Arch = "arm64" }
  default {
    throw "Rad does not support Windows architecture '$MachineArch'. Supported: AMD64, ARM64."
  }
}

$Asset = "rad-windows-$Arch.zip"
$Base = if ($Version) {
  "https://github.com/$Repo/releases/download/$Version"
} else {
  "https://github.com/$Repo/releases/latest/download"
}
$Url = "$Base/$Asset"
$SumsUrl = "$Base/SHA256SUMS"

$Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("rad-install-" + [Guid]::NewGuid().ToString("N"))

Write-Host "Downloading $Url"
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
New-Item -ItemType Directory -Force -Path $Tmp | Out-Null

try {
  $Zip = Join-Path $Tmp $Asset
  $ExtractDir = Join-Path $Tmp "extract"
  $ExtractedExe = Join-Path $ExtractDir "rad.exe"

  Invoke-WebRequest -Uri $Url -OutFile $Zip -UseBasicParsing

  # Verify the download against the release's published checksums before trusting
  # its contents. SHA256SUMS is `sha256sum` output: "<hash>  <filename>".
  $Sums = Join-Path $Tmp "SHA256SUMS"
  Invoke-WebRequest -Uri $SumsUrl -OutFile $Sums -UseBasicParsing
  $Expected = $null
  foreach ($Line in Get-Content -LiteralPath $Sums) {
    $Fields = -split $Line
    if ($Fields.Count -ge 2 -and $Fields[1].TrimStart('*') -ieq $Asset) {
      $Expected = $Fields[0]
      break
    }
  }
  if (-not $Expected) {
    throw "SHA256SUMS has no entry for $Asset."
  }
  $Actual = (Get-FileHash -LiteralPath $Zip -Algorithm SHA256).Hash
  if ($Actual -ine $Expected) {
    throw "Checksum mismatch for $Asset.`n  expected $Expected`n  actual   $Actual"
  }

  New-Item -ItemType Directory -Force -Path $ExtractDir | Out-Null
  Expand-Archive -LiteralPath $Zip -DestinationPath $ExtractDir -Force

  if (-not (Test-Path -LiteralPath $ExtractedExe -PathType Leaf)) {
    throw "Release archive does not contain rad.exe at its root."
  }

  # Validate the downloaded binary before replacing a working installation.
  # A native program's nonzero exit is not a terminating error, so check it.
  & $ExtractedExe --version | Out-Null
  if ($LASTEXITCODE -ne 0) {
    throw "Downloaded rad.exe failed its version check with exit code $LASTEXITCODE."
  }

  try {
    Move-Item -Force -LiteralPath $ExtractedExe -Destination $TargetExe
  } catch {
    throw "Could not install '$TargetExe'. Stop any running Rad processes and run the installer again. $($_.Exception.Message)"
  }
} finally {
  Remove-Item -Recurse -Force -LiteralPath $Tmp -ErrorAction SilentlyContinue
}

Write-Host ""
Write-Host "rad was installed to $TargetExe"
& $TargetExe --version
if ($LASTEXITCODE -ne 0) {
  throw "Installed rad.exe exited with code $LASTEXITCODE during verification."
}

# PATH: normalise entries (expand env vars, trim trailing separators) so an
# equivalent directory is not added twice, and keep the two scopes separate —
# the persisted user PATH for future processes, this process's PATH for now.
function Get-NormalizedPathEntry {
  param([string] $Path)
  if ([string]::IsNullOrWhiteSpace($Path)) { return $null }
  return [Environment]::ExpandEnvironmentVariables($Path.Trim()).TrimEnd('\')
}

$Normalized = Get-NormalizedPathEntry $InstallDir

$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
$UserEntries = @(
  if (-not [string]::IsNullOrWhiteSpace($UserPath)) {
    $UserPath -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
  }
)
$UserHas = @(
  $UserEntries | ForEach-Object { Get-NormalizedPathEntry $_ } | Where-Object { $_ -ieq $Normalized }
).Count -gt 0

$ChangedUserPath = $false
if (-not $UserHas) {
  [Environment]::SetEnvironmentVariable("Path", ((@($InstallDir) + $UserEntries) -join ';'), "User")
  $ChangedUserPath = $true
}

# Also update this process, so `rad` resolves in this session when the installer
# was invoked directly in it (a child powershell.exe cannot alter its parent).
$ProcessEntries = @(
  if (-not [string]::IsNullOrWhiteSpace($env:Path)) {
    $env:Path -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
  }
)
$ProcessHas = @(
  $ProcessEntries | ForEach-Object { Get-NormalizedPathEntry $_ } | Where-Object { $_ -ieq $Normalized }
).Count -gt 0
if (-not $ProcessHas) {
  $env:Path = (@($InstallDir) + $ProcessEntries) -join ';'
}

if ($ChangedUserPath) {
  Write-Host "Added $InstallDir to your user PATH. Open a new terminal to run rad by name."
}

Write-Host ""
Write-Host "Get started:"
Write-Host "  rad serve"
Write-Host "  rad schema migrate"
Write-Host "  rad generate"
Write-Host ""
Write-Host 'For in-memory storage in PowerShell:'
Write-Host '  $env:RAD_STORAGE = "memory"; rad serve'
