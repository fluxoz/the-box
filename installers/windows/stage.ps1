# The Box — Windows hot-install stager.
#
# Run as Administrator from the payload directory (USB stick, mounted ISO, or
# an agent-fetched download). Stages the RAM-resident Box installer and
# reboots into it ONCE via a firmware bootsequence entry: if the installer
# never runs (Secure Boot on, staging broken), the next boot falls back to
# Windows untouched. The only irreversible step is the disk wipe, which the
# installer performs after it has verified the handoff consent file.
#
#   powershell -ExecutionPolicy Bypass -File stage.ps1
#
# Payload directory must contain: grubx64.efi, bzImage, initrd, box-marker,
# box-install.json (the handoff: erase_disk MUST be true — see docs/install.md).

param(
    [string]$PayloadDir = $PSScriptRoot,
    [switch]$NoReboot
)
$ErrorActionPreference = "Stop"

function Fail($msg) { Write-Error $msg; exit 1 }

# --- Preflight ---------------------------------------------------------------
$isAdmin = ([Security.Principal.WindowsPrincipal] `
    [Security.Principal.WindowsIdentity]::GetCurrent()
).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $isAdmin) { Fail "Run this script as Administrator." }

# UEFI detection that works on an installed Windows (not just WinPE).
# PEFirmwareType exists only during setup, so probe several sources.
function Test-IsUefi {
    # 1. bcdedit reports the firmware path for EFI systems.
    try {
        if ((bcdedit /enum '{current}' 2>$null) -match '\.efi') { return $true }
    } catch { }
    # 2. An EFI System Partition means GPT/UEFI boot.
    try {
        $esp = Get-Partition -ErrorAction Stop |
            Where-Object { $_.GptType -eq '{c12a7328-f81f-11d2-ba4b-00a0c93ec93b}' }
        if ($esp) { return $true }
    } catch { }
    # 3. The system-set firmware_type environment variable, if present.
    if ($env:firmware_type -eq 'UEFI') { return $true }
    return $false
}
if (-not (Test-IsUefi)) {
    Fail "This machine appears to boot in legacy BIOS mode. Use the Box USB installer instead."
}

foreach ($f in @("grubx64.efi", "bzImage", "initrd", "box-marker", "box-install.json")) {
    if (-not (Test-Path (Join-Path $PayloadDir $f))) {
        Fail "Payload file missing: $f (looked in $PayloadDir)"
    }
}

$handoff = Get-Content (Join-Path $PayloadDir "box-install.json") -Raw | ConvertFrom-Json
if ($handoff.erase_disk -ne $true) {
    Fail "box-install.json does not set erase_disk=true; refusing to stage a destructive install."
}

Write-Host ""
Write-Host "  *** THE BOX INSTALLER ***" -ForegroundColor Red
Write-Host "  On the next boot this machine will be ERASED - Windows and all data" -ForegroundColor Red
Write-Host "  on the target disk will be permanently destroyed - and replaced with Box OS." -ForegroundColor Red
Write-Host ""

# --- Stage payload on the Windows partition (ESP is too small for the initrd)
$dest = "C:\box-installer"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
foreach ($f in @("bzImage", "initrd", "box-marker", "box-install.json")) {
    Copy-Item (Join-Path $PayloadDir $f) $dest -Force
}
Write-Host "Payload staged in $dest"

# --- Put GRUB on the EFI System Partition ------------------------------------
$esp = "S:"
mountvol $esp /S | Out-Null
try {
    New-Item -ItemType Directory -Force -Path "$esp\EFI\box" | Out-Null
    Copy-Item (Join-Path $PayloadDir "grubx64.efi") "$esp\EFI\box\grubx64.efi" -Force
    Write-Host "Boot loader staged on EFI system partition"
} finally {
    mountvol $esp /D | Out-Null
}

# --- One-shot firmware boot entry --------------------------------------------
$copyOut = (bcdedit /copy '{bootmgr}' /d "Box Installer") | Out-String
if ($copyOut -match '\{[0-9a-fA-F-]+\}') { $guid = $Matches[0] }
else { Fail "bcdedit copy failed: $copyOut" }

bcdedit /set $guid path \EFI\box\grubx64.efi | Out-Null
bcdedit /set '{fwbootmgr}' bootsequence $guid | Out-Null
Write-Host "One-shot boot entry $guid registered (falls back to Windows if the installer doesn't run)"

if ($NoReboot) {
    Write-Host "Staged. Reboot to start the install."
} else {
    Write-Host "Rebooting into the Box installer in 10 seconds... (Ctrl+C to abort)"
    Start-Sleep -Seconds 10
    Restart-Computer -Force
}
