# Per-model Pi Box tuning, shared by every flashable image AND the live-convert
# (both install the same system). The platform layer (boxd + Box services) and
# the model's vendor kernel/firmware come from the flake; this adds only image
# ergonomics and an operator login.
{ lib, ... }:
let
  # Dev operator key. Production injects the operator's key at build/first-boot.
  operatorKey = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICRyw8DcPB6PN/KAuFNV47vjjKc4oNSc1yemko7hObTi murphy@tower";
in
{
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

  # Keys are baked declaratively into the immutable store, so StrictModes (which
  # rejects an authorized_keys file whose realpath is under group-writable
  # /nix/store) is counterproductive for an appliance image. Revisit for prod.
  services.openssh.settings.StrictModes = false;

  # Raw (uncompressed) image so it dd's straight onto the card.
  sdImage.compressImage = false;
}
