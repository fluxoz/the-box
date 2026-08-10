# EXPERIMENTAL hardware layer: a Box on an Apple Silicon Mac (M1/M2) via Asahi.
#
# There is no blow-away image here. Apple Silicon can't boot an arbitrary USB
# image and keeps a minimal macOS/firmware stub, so the Box rides on the Linux
# the Asahi installer sets up. The flow is: install Asahi Linux, then switch the
# system to a Box (the same OS-tier reconcile any Box update uses).
#
# Peripheral firmware (Wi-Fi/Bluetooth) is extracted from the target Mac; a real
# install points hardware.asahi at it. Extraction is off here so this config
# evaluates on any machine, without a Mac in front of us.
{ lib, ... }:
{
  hardware.asahi = {
    enable = true;
    # A real install extracts firmware from the target Mac and turns this on.
    extractPeripheralFirmware = lib.mkForce false;
  };

  # Asahi boots m1n1 -> U-Boot (which provides UEFI) -> systemd-boot.
  boot.loader.systemd-boot.enable = lib.mkDefault true;
  boot.loader.efi.canTouchEfiVariables = lib.mkDefault false;

  # Placeholder root so this evaluates; a real install uses the partition the
  # Asahi installer created.
  fileSystems."/" = lib.mkDefault {
    device = "/dev/disk/by-label/NIXOS";
    fsType = "ext4";
  };

  system.stateVersion = lib.mkDefault "25.05";
}
