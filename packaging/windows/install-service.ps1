<#
.SYNOPSIS
  Installs OpenMouse Bridge as a Windows service that runs with or without a
  signed-in user, so device state survives screen lock, sign-out and sleep.

.DESCRIPTION
  Copies the service executable and the native-hid helper (with a bundled Node
  runtime, since the service runs as SYSTEM and has none of your PATH) into
  Program Files, secures the shared data directory, registers the service and
  starts it. Run from an elevated PowerShell.

  Build first:
    cargo build --release --no-default-features --bin openmouse-bridge-service
    (cd native-hid; npm ci)

  The tray app and the service both use port 17846; this script stops the tray
  app first and does not restart it.

.PARAMETER InstallDir
  Where the service lives. Default: C:\Program Files\OpenMouse Bridge Service

.PARAMETER NodeExe
  The node.exe to bundle. Default: the node on your PATH.

.PARAMETER NoStart
  Install but do not start the service.

.PARAMETER Uninstall
  Remove the service and its program files; the data directory is kept.
#>
[CmdletBinding()]
param(
  [string]$InstallDir = (Join-Path $env:ProgramFiles 'OpenMouse Bridge Service'),
  [string]$NodeExe = (Get-Command node -ErrorAction SilentlyContinue).Source,
  [switch]$NoStart,
  [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'
$ServiceName = 'OpenMouseBridge'
$Root = Resolve-Path (Join-Path $PSScriptRoot '..\..')

$principal = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw 'Run this from an elevated (administrator) PowerShell.'
}

function Stop-IfRunning([string]$Name) {
  Get-Process -Name $Name -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "Stopping $Name (PID $($_.Id))"
    $_ | Stop-Process -Force
  }
}

$installedExe = Join-Path $InstallDir 'openmouse-bridge-service.exe'

if ($Uninstall) {
  if (Test-Path $installedExe) { & $installedExe uninstall }
  if (Test-Path $InstallDir) { Remove-Item -Recurse -Force $InstallDir }
  Write-Host "Removed. Settings are kept in $env:ProgramData\OpenMouse\Bridge."
  return
}

$builtExe = Join-Path $Root 'target\release\openmouse-bridge-service.exe'
if (-not (Test-Path $builtExe)) {
  throw "Build the service first: cargo build --release --no-default-features --bin openmouse-bridge-service ($builtExe is missing)."
}
$helper = Join-Path $Root 'native-hid'
if (-not (Test-Path (Join-Path $helper 'node_modules'))) {
  throw "Run 'npm ci' in $helper first: the helper's dependencies are missing."
}
if (-not $NodeExe -or -not (Test-Path $NodeExe)) {
  throw 'Could not find node.exe. Pass -NodeExe <path>.'
}

# An existing install is replaced, not duplicated.
if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
  if (Test-Path $installedExe) { & $installedExe uninstall }
  else { sc.exe delete $ServiceName | Out-Null }
}
Stop-IfRunning 'openmouse-bridge'

Write-Host "Copying to $InstallDir"
New-Item -ItemType Directory -Force $InstallDir | Out-Null
Copy-Item $builtExe $installedExe -Force

$helperTarget = Join-Path $InstallDir 'native-hid'
if (Test-Path $helperTarget) { Remove-Item -Recurse -Force $helperTarget }
New-Item -ItemType Directory -Force $helperTarget | Out-Null
# robocopy exit codes below 8 mean success.
robocopy $helper $helperTarget /E /XF '*.test.mjs' 'test-delux-hardware.mjs' 'experiment-*.mjs' /NFL /NDL /NJH /NJS /NP | Out-Null
if ($LASTEXITCODE -ge 8) { throw "robocopy failed ($LASTEXITCODE)" }
$nodeTarget = Join-Path $helperTarget 'node'
New-Item -ItemType Directory -Force $nodeTarget | Out-Null
Copy-Item $NodeExe (Join-Path $nodeTarget 'node.exe') -Force

# Only administrators may change what a SYSTEM service runs.
icacls $InstallDir /inheritance:r /grant:r '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' '*S-1-5-32-545:(OI)(CI)RX' | Out-Null

& $installedExe install
if ($LASTEXITCODE -ne 0) { throw 'Registering the service failed.' }

if (-not $NoStart) {
  Start-Service -Name $ServiceName
  Write-Host "Started. Check it with:  Invoke-RestMethod http://127.0.0.1:17846/v1/device-state"
}
