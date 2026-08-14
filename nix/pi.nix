# Per-model Pi Box tuning, shared by every flashable image AND the live-convert
# (both install the same system). The platform layer (boxd + Box services) and
# the model's vendor kernel/firmware come from the flake; this adds only image
# ergonomics and an operator login.
{ config, lib, ... }:
let
  # Dev operator key. Production injects the operator's key at build/first-boot.
  operatorKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICRyw8DcPB6PN/KAuFNV47vjjKc4oNSc1yemko7hObTi murphy@tower";
in
{
  # The channel binding (boxd-channel-init) now comes from platform.nix, so
  # every Box gets it — it lived only here once, and x86 Boxes came up unable
  # to update themselves.

  users.users.murphy = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
    # The blessed NixOS mechanism (-> /etc/ssh/authorized_keys.d/murphy). NOT
    # environment.etc."box/authorized_keys": that resolves (realpath) into the
    # group-writable /nix/store, which sshd StrictModes rejects. The x86 appliance
    # gets away with /etc/box/authorized_keys because box-os.nix writes it as a
    # REAL file at firstboot; the Pi images provision the key declaratively.
    openssh.authorizedKeys.keys = [ operatorKey ];
    # Console/recovery password (key-only SSH stays the norm). Password: box-jdt0yua5
    hashedPassword = "$6$6B6jX/O8HxYLLjI.$SfmkmjHaNuNl9yMNJWEswme2Yn8fcBz7aKxnt188TqXR8eOn97p6stATn0rFBSeej15o3syYBVmN1rWfwsaE70";
  };
  security.sudo.wheelNeedsPassword = false;

  # The sd-image/base profile drags ZFS in. The Box never uses ZFS anywhere
  # (the x86 appliance ships ext4 + vfat only), and on a Pi it is worse than
  # dead weight: the vendor kernel is a multi-output derivation whose `dev`
  # output is on no binary cache, and zfs-kernel is the ONLY consumer of that
  # output. Nix cannot substitute a subset of a derivation's outputs, so one
  # uncached `dev` forces a full kernel rebuild — which is the entire reason
  # Pi 3/4 releases spent ~1h33m compiling a kernel that Pi 5 downloaded.
  # Turning it off aligns the Pi with the appliance and lets the vendor kernel
  # substitute. Re-enable per-box if a Pi ever needs ZFS.
  boot.supportedFilesystems.zfs = lib.mkForce false;

  # Keys are baked declaratively into the immutable store, so StrictModes (which
  # rejects an authorized_keys file whose realpath is under group-writable
  # /nix/store) is counterproductive for an appliance image. Revisit for prod.
  services.openssh.settings.StrictModes = false;
}
