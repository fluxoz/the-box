# The Box — convert a running Windows machine into a Box.
#
#   $env:BOX_ORDERS_B64='<built at thebox.build>'; irm https://thebox.build/install.ps1 | iex
#
# One pasted command, with the orders riding base64 in an environment variable —
# the same rider the Linux one-liner takes. The orders carry the SSH public key,
# the pairing-code hash and the disk choice, so the takeover runs start to
# finish with nobody watching. Thin wrapper over stage.ps1, the Windows
# takeover: it downloads the Box-for-Windows payload, sets a one-shot boot
# entry, and reboots into the installer, which wipes the machine and installs
# Box OS. UEFI only; Secure Boot must be off until a signed loader ships.
$ErrorActionPreference = 'Stop'

# `#Requires -RunAsAdministrator` only guards a script run as a file; piped
# through iex it is a comment. Check for real, before downloading anything.
$who = [Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()
if (-not $who.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
  throw 'this needs an elevated (Run as administrator) PowerShell.'
}

$Base = if ($env:BOX_BASE) { $env:BOX_BASE } else { 'https://thebox.build' }

$work = Join-Path $env:TEMP 'box-install'
New-Item -ItemType Directory -Force -Path $work | Out-Null

# Orders: the rider first (the normal path — nothing to download or move), then
# a box-install.json file for anyone still carrying one around by hand.
$ordersPath = Join-Path $work 'box-install.json'
if ($env:BOX_ORDERS_B64) {
  $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($env:BOX_ORDERS_B64))
  # No BOM: the installer on the other side parses this as strict JSON, and
  # PowerShell's own UTF8 default would prepend one.
  [IO.File]::WriteAllText($ordersPath, $json, [Text.UTF8Encoding]::new($false))
} else {
  $orders = if ($env:BOX_ORDERS) { $env:BOX_ORDERS }
    elseif (Test-Path .\box-install.json) { (Resolve-Path .\box-install.json).Path }
    elseif (Test-Path "$HOME\Downloads\box-install.json") { "$HOME\Downloads\box-install.json" }
    else { $null }
  if (-not $orders) {
    throw "no orders. Build the command at https://thebox.build (it sets BOX_ORDERS_B64 for you), or put box-install.json in this folder."
  }
  Copy-Item $orders $ordersPath -Force
}
if (-not ((Get-Content $ordersPath -Raw) -match '"erase_disk"\s*:\s*true')) {
  throw 'orders do not consent to erase_disk:true — refusing.'
}

Write-Host "[box] downloading the Box-for-Windows payload from $Base ..." -ForegroundColor Yellow
foreach ($f in 'stage.ps1','bzImage','initrd','grubx64.efi','box-marker') {
  Invoke-WebRequest -UseBasicParsing "$Base/windows/$f" -OutFile (Join-Path $work $f)
}

Write-Host "[box] launching the takeover — this reboots into a wipe. If anything fails before the wipe, the next boot is Windows, untouched." -ForegroundColor Yellow
& (Join-Path $work 'stage.ps1')
