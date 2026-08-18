# The install-time bootstrap, shared by every Box regardless of how it was
# installed.
#
# Machine-specific state (hostname, Wi-Fi, SSH keys, tunnel token, and above all
# the pairing code that says who owns this Box) is NOT baked into the image. One
# generic closure serves every machine and the specifics arrive as a handoff
# file, which is what makes batch installs a matter of handing out different
# files.
#
# This lives here rather than in box-os.nix because the flashed Pi images do not
# import box-os.nix: they compose platform.nix directly. When the handoff logic
# lived there, a Pi image had no way to read the orders written into it at
# download time, which is the whole point of the flashed route.
{ config, lib, pkgs, ... }:
{
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

      # The flashed-image route has no installer to write install-config.json:
      # the orders travel in a fixed-size file on the FAT boot partition, put
      # there when the image was downloaded. Whoever created the install medium
      # is the owner, so this is where that ownership arrives. See
      # docs/claim-flow-spec.md.
      #
      # The file is padded with NULs to a fixed length; take everything up to
      # the first one and check it parses before trusting it.
      for claim in /boot/box-claim.txt /boot/firmware/box-claim.txt; do
        [ -f "$claim" ] || continue
        [ -f "$conf" ] && break
        candidate=$(tr -d '\000' < "$claim")
        printf '%s' "$candidate" | jq -e . >/dev/null 2>&1 || continue
        # Still carrying the build-time magic: nobody personalized this image.
        case "$candidate" in *BOXCLAIM-PLACEHOLDER*) continue ;; esac
        install -d -m 755 /etc/box
        printf '%s' "$candidate" > "$conf"
        chmod 600 "$conf"
        break
      done

      [ -f "$conf" ] || exit 0

      # Hostname: "auto" derives a stable per-machine name for batch installs
      # so several Boxes on one LAN don't fight over box.local.
      name=$(jq -r '.hostname // "box"' "$conf")
      if [ "$name" = "auto" ]; then
        suffix=$(cut -c1-6 /etc/machine-id 2>/dev/null || echo 000000)
        name="box-$suffix"
      fi
      echo "$name" > /proc/sys/kernel/hostname

      # Everything below is the ONE-TIME handoff: it sets the box up, and from
      # then on the box's own state is the truth. Re-applying it every boot
      # undid the operator afterwards — it rewrote authorized_keys and
      # network.toml (silently re-enabling a tunnel they had turned off), and
      # re-seeded the "single-use" enrollment code, so a code that had been
      # redeemed or had expired came back at every reboot.
      #
      # The hostname above stays outside this guard: it is written to
      # /proc/sys/kernel/hostname, which does not survive a reboot.
      stamp=/etc/box/.handoff-applied
      [ -e "$stamp" ] && exit 0

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

      # Applied. Later boots leave the box's own state alone.
      touch "$stamp"
    '';
  };
}
