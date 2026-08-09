# The Box automated installer: boots entirely into RAM, obtains its orders
# (consent + configuration) and a storage layout, wipes the chosen disk(s) with
# disko and installs the embedded Box OS closure fully offline, then reboots.
#
# Two doors, one binary:
#   * orders present  -> box-installer resolves their storage policy (single /
#     mirror / pool) to a disko config; install proceeds unattended.
#   * no orders        -> box-installer runs the console wizard so an operator at
#     the screen picks a layout; if nobody responds it refuses and touches
#     nothing.
#
# The same module backs the ISO/USB image, the PXE netboot image, and the
# Windows-staged install — only the delivery of kernel/initrd/orders differs.
{ config, lib, pkgs, modulesPath, boxSystem, diskoPkg, boxInstaller, nixpkgsSrc, ... }:
let
  installScript = pkgs.writeShellApplication {
    name = "box-install";
    runtimeInputs = [
      boxInstaller
      diskoPkg
      pkgs.coreutils
      pkgs.curl
      pkgs.jq
      pkgs.kmod # modprobe, for disko's mdadm/lvm module loading
      pkgs.nix
      pkgs.nixos-install-tools
      pkgs.systemd
      pkgs.util-linux
    ];
    text = ''
      log() { echo "[box-install] $*"; }

      # ---- 1. Locate the orders (consent + config) ------------------------
      # Orders are always copied to RAM before anything touches a disk; in the
      # staged-from-Windows flow they live on the very disk being wiped.
      handoff=""
      diskocfg=""

      # Orders passed on the kernel command line (the curl|sh takeover): base64,
      # RAM-only, and survives the initramfs -> installer switch-root (files
      # injected into the initramfs do NOT — NixOS drops them at switch-root).
      b64=$(tr ' ' '\n' < /proc/cmdline | sed -n 's/^box\.install-b64=//p' | head -n1)
      if [ -z "$handoff" ] && [ -n "$b64" ]; then
        log "using orders from the kernel command line"
        if printf '%s' "$b64" | base64 -d > /tmp/box-install.json 2>/dev/null; then
          handoff=/tmp/box-install.json
        else
          log "failed to decode box.install-b64"
        fi
      fi
      url=$(tr ' ' '\n' < /proc/cmdline | sed -n 's/^box\.install-url=//p' | head -n1)
      if [ -z "$handoff" ] && [ -n "$url" ]; then
        log "fetching orders from $url"
        if curl -fsSL "$url" -o /tmp/box-install.json; then
          handoff=/tmp/box-install.json
        else
          log "failed to fetch $url"
        fi
      fi
      if [ -z "$handoff" ] && [ -e /dev/disk/by-label/BOX-INSTALL ]; then
        mkdir -p /run/box-handoff
        mount -o ro /dev/disk/by-label/BOX-INSTALL /run/box-handoff
        if [ -f /run/box-handoff/box-install.json ]; then
          cp /run/box-handoff/box-install.json /tmp/box-install.json
          handoff=/tmp/box-install.json
        fi
        umount /run/box-handoff || true
      fi
      # Staged installs (e.g. launched from Windows): scan every partition for
      # box-installer/box-install.json.
      if [ -z "$handoff" ] && grep -q 'box\.install-scan' /proc/cmdline; then
        mkdir -p /run/box-scan
        while read -r name type; do
          if [ "$type" != "part" ]; then continue; fi
          if mount -o ro "/dev/$name" /run/box-scan 2>/dev/null; then
            if [ -f /run/box-scan/box-installer/box-install.json ]; then
              log "found staged orders on /dev/$name"
              cp /run/box-scan/box-installer/box-install.json /tmp/box-install.json
              handoff=/tmp/box-install.json
            fi
            umount /run/box-scan || true
          fi
          if [ -n "$handoff" ]; then break; fi
        done < <(lsblk -rno NAME,TYPE)
      fi

      # ---- 2. No orders -> the console wizard (Door 2), else refuse --------
      # The wizard writes its own effective orders + disko config. It exits
      # non-zero if the operator cancels, or on its own if nobody responds
      # within a couple of minutes (headless boot with no orders) — so an
      # unattended machine that reached here by accident is never wiped.
      if [ -z "$handoff" ]; then
        log "no orders found — starting the setup wizard (refusing if no operator responds)."
        wtty=/dev/tty1; [ -c "$wtty" ] || wtty=/dev/console
        # setsid -c makes the wizard the session leader with $wtty as its
        # controlling terminal, so the kernel actually routes keystrokes to it
        # (a plain redirect leaves it as a background process the VT ignores);
        # -w waits and propagates its exit status.
        if TERM=linux setsid -w -c box-installer wizard \
             --orders-out /tmp/box-install.json \
             --disko-out /tmp/box-disko.nix <> "$wtty" >&0 2>&0; then
          handoff=/tmp/box-install.json
          diskocfg=/tmp/box-disko.nix
        fi
      fi
      if [ -z "$handoff" ]; then
        log "no orders (BOX-INSTALL volume, box.install-url=, box.install-b64=, staged dir)"
        log "and no console setup completed. Refusing to touch any disk. Machine unchanged."
        exit 0
      fi

      log "using orders: $handoff"
      jq . "$handoff" > /dev/null || { log "orders are not valid JSON"; exit 1; }

      erase=$(jq -r '.erase_disk // false' "$handoff")
      if [ "$erase" != "true" ]; then
        log "orders do not set \"erase_disk\": true — refusing to wipe anything."
        exit 1
      fi

      # ---- 3. Resolve the storage layout to a disko config ----------------
      # (The wizard already produced one; the pre-baked-orders path resolves the
      # storage policy — single / mirror / pool — against the real disks here.)
      if [ -z "$diskocfg" ]; then
        diskocfg=/tmp/box-disko.nix
        if ! box-installer plan --orders "$handoff" --out "$diskocfg"; then
          log "could not resolve a storage layout from the orders (see message above)."
          exit 1
        fi
      fi

      # ---- 4. Partition + format via disko --------------------------------
      log "partitioning with disko (this ERASES the target disk(s))"
      disko --mode destroy,format,mount --yes-wipe-all-disks \
        --root-mountpoint /mnt "$diskocfg"

      # ---- 5. Install the embedded Box OS closure (offline) ---------------
      system=$(cat /etc/box/system-store-path)
      log "installing Box OS from $system"
      mkdir -p /mnt/etc/box
      install -m 600 "$handoff" /mnt/etc/box/install-config.json
      nixos-install --system "$system" --no-root-passwd --no-channel-copy
      log "BOX INSTALL COMPLETE"

      # ---- 6. Finish ------------------------------------------------------
      finish=$(jq -r '.finish // "reboot"' "$handoff")
      umount -R /mnt || true
      case "$finish" in
        poweroff) systemctl poweroff ;;
        none) log "finish=none: leaving installer running" ;;
        *) systemctl reboot ;;
      esac
    '';
  };
in
{
  networking.hostName = "box-installer";
  # ttyS0 last = primary console, so installer progress is visible over serial.
  boot.kernelParams = [ "console=tty0" "console=ttyS0,115200" ];
  # Load RAID/device-mapper modules in the installer so disko can *build* a
  # mirror (mdadm) or pool (LVM) at install time — separate from the installed
  # OS assembling them at boot (see hardware-appliance.nix).
  boot.kernelModules = [ "md_mod" "dm_mod" "raid0" "raid1" "raid10" ];

  # Free tty1 so the setup wizard owns the screen (Door 2). Serial stays the
  # log/primary console; the wizard only appears where there's a monitor.
  systemd.services."getty@tty1".enable = lib.mkForce false;
  systemd.services."autovt@tty1".enable = lib.mkForce false;

  # The Box OS closure rides inside the installer image so installation needs
  # no network at all.
  environment.etc."box/system-store-path".text = "${boxSystem}";
  environment.systemPackages = [ installScript boxInstaller diskoPkg pkgs.jq ];

  # disko evaluates its config at runtime; bake nixpkgs in so that also works
  # offline.
  nix.nixPath = [ "nixpkgs=${nixpkgsSrc}" ];
  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  systemd.services.box-install = {
    description = "The Box automated installer";
    wantedBy = [ "multi-user.target" ];
    after = [ "local-fs.target" "systemd-udev-settle.service" ];
    wants = [ "systemd-udev-settle.service" ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      StandardOutput = "journal+console";
      StandardError = "journal+console";
    };
    path = [ installScript ];
    script = "box-install";
  };
}
