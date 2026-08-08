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

  # NM must not reset a runtime-assigned hostname from /etc/hostname or DHCP.
  environment.etc."NetworkManager/conf.d/box-hostname.conf".text = ''
    [main]
    hostname-mode=none
  '';

  # <hostname>.local on the LAN, and the mDNS surface fleet discovery uses.
  services.avahi = {
    enable = true;
    nssmdns4 = true;
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
  environment.systemPackages = [ pkgs.jq ];

  system.stateVersion = "25.11";
}
