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
  # Bind a freshly flashed Pi to the platform update channel on first boot:
  # host id from the hostname, board auto-detected from the device tree. This
  # is what makes the dashboard's "Update now" work out of the box — without
  # it, the channel would need a one-time `boxd channel set` over SSH.
  # Idempotent: a box that already has a binding is left alone (so this is
  # also harmless on systems rebuilt by a channel update, which import this
  # module via boxSystem).
  systemd.services.boxd-channel-init = {
    description = "The Box: bind the platform update channel (first boot)";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" ];
    path = [ config.services.the-box.package ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      [ -e /var/lib/boxd/channel.toml ] && exit 0
      mkdir -p /var/lib/boxd
      boxd --data-dir /var/lib/boxd channel set \
        --host-id "$(cat /proc/sys/kernel/hostname)" \
        --system aarch64-linux
    '';
  };

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
}
