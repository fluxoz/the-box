# Per-model Pi Box tuning, shared by every flashable image AND the live-convert
# (both install the same system). The platform layer (boxd + Box services) and
# the model's vendor kernel/firmware come from the flake; this adds only image
# ergonomics.
#
# NOTE: this file deliberately contains no login of any kind. Every image built
# from it is byte-identical and belongs to nobody until somebody claims it. A
# Pi's operator arrives through the orders written into the image at download
# time: box-firstboot writes ssh_authorized_keys to /etc/box/authorized_keys
# (which platform.nix already points sshd at) and seeds the pairing code that
# makes the Box answer to its owner. See docs/claim-flow-spec.md.
{ config, lib, ... }:
{
  # The channel binding (boxd-channel-init) now comes from platform.nix, so
  # every Box gets it — it lived only here once, and x86 Boxes came up unable
  # to update themselves.

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

  # Same recovery hatch as the x86 appliance: physical access is the trust
  # boundary, and a headless Pi that lost its pairing code needs a way back in.
  # Remote password auth stays impossible (no password is ever set).
  services.getty.autologinUser = "root";
  users.users.root.hashedPassword = "!";
}
