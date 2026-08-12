# The Box platform layer: the software that makes a machine a Box, with no
# assumptions about the machine's disk, identity or role. Every Box — the
# generic appliance image and each git-managed per-box config alike — composes
# on top of this. Hardware/boot lives in hardware-*.nix; identity and services
# come from the host layer.
{ config, lib, pkgs, ... }:
{
  # A default so the appliance image boots as "box.local"; per-box host configs
  # override it with their own name.
  networking.hostName = lib.mkDefault "box";
  networking.networkmanager.enable = true;
  networking.firewall.allowedTCPPorts = [ 2693 ];

  # Secrets are age-encrypted (agenix) and decrypted at runtime to /run/agenix
  # with the box's own SSH host key. boxd encrypts each secret to this key plus
  # the operator's key(s), so config + secrets can live in a git repo the user
  # owns without ever exposing a plaintext value in git or the Nix store.
  age.identityPaths = [ "/etc/ssh/ssh_host_ed25519_key" ];

  # NM must not reset a runtime-assigned hostname from /etc/hostname or DHCP.
  environment.etc."NetworkManager/conf.d/box-hostname.conf".text = ''
    [main]
    hostname-mode=none
  '';

  # <hostname>.local on the LAN, and the mDNS surface fleet discovery uses.
  services.avahi = {
    enable = true;
    nssmdns4 = true;
    # Advertise under the *runtime* hostname. The appliance's firstboot sets a
    # per-machine hostname at boot; an empty hostName makes avahi use
    # gethostname() rather than baking in the build-time "box", so a fleet of
    # appliance boxes don't all collide on box.local.
    hostName = "";
    publish = {
      enable = true;
      addresses = true;
    };
  };

  # Agent/remote management: keys only, never passwords.
  services.openssh = {
    enable = true;
    settings.PasswordAuthentication = false;
    settings.PermitRootLogin = "prohibit-password";
    authorizedKeysFiles = [ "/etc/box/authorized_keys" ];
  };

  services.the-box = {
    enable = true;
    listen = lib.mkDefault "0.0.0.0:2693";
  };

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  # Resolve `nixpkgs` to the exact one this system was built from, for anyone
  # running nix on the box by hand.
  nix.registry.nixpkgs.to = {
    type = "path";
    path = pkgs.path;
  };

  environment.systemPackages = [ pkgs.jq ];

  # Box Connect: WireGuard mesh via Tailscale/Headscale. tailscaled runs; boxd
  # drives it (`boxd connect enroll` / `cloud connect`). Which coordinator it
  # joins is the operator's choice — self-hosted Headscale (free) or the managed
  # relay (paid) — so nothing is enabled toward any server until they enroll.
  services.tailscale.enable = true;

  system.stateVersion = "25.11";
}
