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
    # Multi-disk layouts: assemble RAID (mirror) and map LVM (pool) in initrd.
    "dm_mod"
    "md_mod"
    "raid0"
    "raid1"
    "raid10"
  ];
  hardware.enableRedistributableFirmware = true;

  # One generic closure installs every layout the wizard/resolver can pick
  # (single ext4, mirror = mdadm RAID1, pool = LVM linear), so the boot path
  # must handle all three without knowing which was chosen. systemd in the
  # initrd lets udev incrementally assemble md arrays and autoactivate LVM as
  # devices appear; root is then found by its box-root label, exactly as the
  # single-disk case. swraid pulls mdadm + its udev rules into the initrd.
  boot.initrd.systemd.enable = true;
  boot.swraid.enable = true;
  # Appliance has no MTA; give mdadm a sink so the monitor can't crash for want
  # of a mail address. Disk-failure signalling is surfaced through boxd health.
  boot.swraid.mdadmConf = "MAILADDR root";

  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = true;
  # ttyS0 last = primary console: appliance debugging happens over serial.
  boot.kernelParams = [ "console=tty0" "console=ttyS0,115200" ];
}
