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

  systemd.services.box-firstboot = {
    description = "Apply install-time handoff configuration";
    wantedBy = [ "multi-user.target" ];
    before = [ "NetworkManager.service" "avahi-daemon.service" "boxd.service" ];
    after = [ "local-fs.target" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    path = [ pkgs.jq config.services.the-box.package ];
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

      # Enrollment code (its hash) from the handoff: makes the box pairable from
      # first boot with the code in the user's recovery kit — no SSH needed. We
      # seed it before boxd starts, owned by the boxd user.
      enroll_hash=$(jq -r '.enrollment_code_hash // empty' "$conf")
      if [ -n "$enroll_hash" ]; then
        install -d -o boxd -g boxd -m 750 /var/lib/boxd
        boxd --data-dir /var/lib/boxd auth import-code --hash "$enroll_hash" --label enrollment || true
      fi

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
}
