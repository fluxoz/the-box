# Box OS: the dedicated appliance system. The whole OS is a Nix generation —
# kernel, services and boxd switch and roll back atomically together.
#
# Machine-specific state (hostname, Wi-Fi, SSH keys, tunnel token) is NOT
# baked into this closure: the installer drops a handoff file at
# /etc/box/install-config.json and box-firstboot applies it on every boot.
# One generic closure therefore serves every machine, which is what makes
# batch installs a matter of handing out different handoff files.
{ config, lib, pkgs, ... }:
{
  networking.hostName = "box";
  networking.networkmanager.enable = true;
  networking.firewall.allowedTCPPorts = [ 2693 ];

  # box-firstboot sets the (possibly per-machine) hostname; NM must not
  # reset it from /etc/hostname or DHCP afterwards.
  environment.etc."NetworkManager/conf.d/box-hostname.conf".text = ''
    [main]
    hostname-mode=none
  '';

  # box.local on the LAN
  services.avahi = {
    enable = true;
    nssmdns4 = true;
    publish = {
      enable = true;
      addresses = true;
    };
  };

  # Agent/remote management: keys come from the handoff file, never passwords.
  services.openssh = {
    enable = true;
    settings.PasswordAuthentication = false;
    settings.PermitRootLogin = "prohibit-password";
    authorizedKeysFiles = [ "/etc/box/authorized_keys" ];
  };

  services.the-box = {
    enable = true;
    listen = "0.0.0.0:2693";
  };

  # Local console is a recovery hatch; physical access is the trust boundary
  # on an appliance. Remote password auth stays impossible.
  services.getty.autologinUser = "root";
  users.users.root.hashedPassword = "!";

  systemd.services.box-firstboot = {
    description = "Apply install-time handoff configuration";
    wantedBy = [ "multi-user.target" ];
    before = [ "NetworkManager.service" "avahi-daemon.service" "boxd.service" ];
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    path = [ pkgs.jq ];
    script = ''
      conf=/etc/box/install-config.json
      [ -f "$conf" ] || exit 0

      # Hostname: "auto" derives a stable per-machine name for batch installs
      # so several Boxes on one LAN don't fight over box.local.
      name=$(jq -r '.hostname // "box"' "$conf")
      if [ "$name" = "auto" ]; then
        suffix=$(cut -c1-6 /etc/machine-id 2>/dev/null || echo 000000)
        name="box-$suffix"
      fi
      echo "$name" > /proc/sys/kernel/hostname

      # Wi-Fi: materialize a NetworkManager connection before NM starts.
      ssid=$(jq -r '.wifi.ssid // empty' "$conf")
      if [ -n "$ssid" ]; then
        psk=$(jq -r '.wifi.password // empty' "$conf")
        mkdir -p /etc/NetworkManager/system-connections
        {
          printf '[connection]\nid=box-wifi\ntype=wifi\nautoconnect=true\n\n'
          printf '[wifi]\nssid=%s\nmode=infrastructure\n\n' "$ssid"
          if [ -n "$psk" ]; then
            printf '[wifi-security]\nkey-mgmt=wpa-psk\npsk=%s\n\n' "$psk"
          fi
          printf '[ipv4]\nmethod=auto\n\n[ipv6]\nmethod=auto\n'
        } > /etc/NetworkManager/system-connections/box-wifi.nmconnection
        chmod 600 /etc/NetworkManager/system-connections/box-wifi.nmconnection
      fi

      # SSH keys for agents/operators.
      jq -r '.ssh_authorized_keys[]? // empty' "$conf" > /etc/box/authorized_keys

      # Cloudflare tunnel token: seed boxd's secret store and enable.
      token=$(jq -r '.cloudflare_tunnel_token // empty' "$conf")
      if [ -n "$token" ]; then
        install -d -m 755 /var/lib/boxd
        install -d -m 700 /var/lib/boxd/secrets
        printf '%s' "$token" > /var/lib/boxd/secrets/cloudflare-tunnel-token
        chmod 600 /var/lib/boxd/secrets/cloudflare-tunnel-token
        printf 'cloudflare_enabled = true\n' > /var/lib/boxd/network.toml
        chown -R boxd:boxd /var/lib/boxd
      fi
    '';
  };

  # Matches nix/disko-template.nix by filesystem label.
  fileSystems."/" = {
    device = "/dev/disk/by-label/box-root";
    fsType = "ext4";
  };
  fileSystems."/boot" = {
    device = "/dev/disk/by-label/BOX-ESP";
    fsType = "vfat";
  };

  # One generic image must boot on any machine: broad storage/input driver
  # set in the initrd, plus redistributable firmware (Wi-Fi, NICs, GPUs).
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

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  environment.systemPackages = [ pkgs.jq ];

  system.stateVersion = "25.11";
}
