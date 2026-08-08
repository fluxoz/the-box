# How the generic Box image boots: one closure that must come up on any
# machine, so a broad initrd driver set, redistributable firmware, and disks
# addressed by the labels nix/disko-template.nix writes. Both the appliance
# image and a per-box config installed by the appliance share this layer.
{ lib, ... }:
{
  fileSystems."/" = {
    device = "/dev/disk/by-label/box-root";
    fsType = "ext4";
  };
  fileSystems."/boot" = {
    device = "/dev/disk/by-label/BOX-ESP";
    fsType = "vfat";
  };

  boot.initrd.availableKernelModules = [
    "ahci"
    "ehci_pci"
    "mmc_block"
    "nvme"
    "sd_mod"
    "sdhci_acpi"
    "sdhci_pci"
    "sr_mod"
    "uas"
    "usb_storage"
    "usbhid"
    "virtio_blk"
    "virtio_pci"
    "virtio_scsi"
    "xhci_pci"
  ];
  hardware.enableRedistributableFirmware = true;

  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
  # ttyS0 last = primary console: appliance debugging happens over serial.
  boot.kernelParams = [ "console=tty0" "console=ttyS0,115200" ];
}
