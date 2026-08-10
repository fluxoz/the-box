# Per-model Pi Box tuning, shared by every flashable image AND the live-convert
# (both install the same system). The platform layer (boxd + Box services) and
# the model's vendor kernel/firmware come from the flake; this adds only image
# ergonomics and an operator login.
{ lib, ... }:
{
  # Operator login — key-only, passwordless sudo, mirroring the appliance.
  # (Production injects the operator's key at build/first-boot; this is the dev
  # key so the test images are reachable.)
  users.users.murphy = {
    isNormalUser = true;
    extraGroups = [ "wheel" ];
  };
  security.sudo.wheelNeedsPassword = false;

  # platform.nix points sshd at /etc/box/authorized_keys for every user.
  environment.etc."box/authorized_keys".text =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICRyw8DcPB6PN/KAuFNV47vjjKc4oNSc1yemko7hObTi murphy@tower\n";

  # Raw (uncompressed) image so it dd's straight onto the card.
  sdImage.compressImage = false;
}
