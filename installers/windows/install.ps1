# The Box — convert a running Windows machine into a Box.
#
#   irm https://thebox.build/install.ps1 | iex
#
# with your orders (box-install.json, from the Configurator) in the current
# folder or your Downloads. Thin wrapper over stage.ps1, the Windows takeover:
# it downloads the Box-for-Windows payload, sets a one-shot boot entry, and
# reboots into the installer, which wipes the machine and installs Box OS.
# UEFI only; Secure Boot must be off until a signed loader ships.
#Requires -RunAsAdministrator
$ErrorActionPreference = 'Stop'

$Base = if ($env:BOX_BASE) { $env:BOX_BASE } else { 'https://thebox.build' }

$orders = if ($env:BOX_ORDERS) { $env:BOX_ORDERS }
  elseif (Test-Path .\box-install.json) { (Resolve-Path .\box-install.json).Path }
  elseif (Test-Path "$HOME\Downloads\box-install.json") { "$HOME\Downloads\box-install.json" }
  else { $null }
if (-not $orders) {
  throw "box-install.json not found. Generate it in the Configurator and put it in this folder or Downloads (or set BOX_ORDERS)."
}
if (-not ((Get-Content $orders -Raw) -match '"erase_disk"\s*:\s*true')) {
  throw "orders do not consent to erase_disk:true — refusing."
}

$work = Join-Path $env:TEMP 'box-install'
New-Item -ItemType Directory -Force -Path $work | Out-Null
Write-Host "[box] downloading the Box-for-Windows payload from $Base ..." -ForegroundColor Yellow
foreach ($f in 'stage.ps1','bzImage','initrd','grubx64.efi','box-marker') {
  Invoke-WebRequest -UseBasicParsing "$Base/windows/$f" -OutFile (Join-Path $work $f)
}
Copy-Item $orders (Join-Path $work 'box-install.json') -Force

Write-Host "[box] launching the takeover — this reboots into a wipe. If anything fails before the wipe, the next boot is Windows, untouched." -ForegroundColor Yellow
& (Join-Path $work 'stage.ps1')
