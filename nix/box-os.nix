# Box OS: the dedicated appliance system — the generic image the installer
# lays down. It is the platform layer plus the generic boot/hardware layer,
# plus the install-time bootstrap: machine-specific state (hostname, Wi-Fi,
# SSH keys, tunnel token) is NOT baked in — the installer drops a handoff file
# at /etc/box/install-config.json and box-firstboot applies it on every boot.
# One generic closure therefore serves every machine, which is what makes
# batch installs a matter of handing out different handoff files.
#
# A git-managed per-box config declares that same state in Nix instead, and
# composes the platform + hardware layers directly (see nodes/hosts/*).
{ config, lib, pkgs, ... }:
{
  imports = [
    ./platform.nix
    ./hardware-appliance.nix
  ];

  # Local console is a recovery hatch; physical access is the trust boundary
  # on an appliance. Remote password auth stays impossible.
  services.getty.autologinUser = "root";
  users.users.root.hashedPassword = "!";
}
