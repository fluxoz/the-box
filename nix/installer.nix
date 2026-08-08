# The Box automated installer: boots entirely into RAM, finds a handoff file
# (its consent + configuration), wipes the chosen disk with disko and installs
# the embedded Box OS closure fully offline, then reboots into Box OS.
#
# The same module backs the ISO/USB image, the PXE netboot image, and the
# Windows-staged install — only the delivery of kernel/initrd/handoff differs.
{ config, lib, pkgs, modulesPath, boxSystem, diskoPkg, nixpkgsSrc, ... }:
let
  installScript = pkgs.writeShellApplication {
    name = "box-install";
    runtimeInputs = [
      diskoPkg
      pkgs.coreutils
      pkgs.curl
      pkgs.jq
      pkgs.nix
      pkgs.nixos-install-tools
      pkgs.systemd
      pkgs.util-linux
    ];
    text = ''
      log() { echo "[box-install] $*"; }

      # ---- 1. Locate the handoff file (consent + config) -------------------
      # The handoff is always copied to RAM before anything touches a disk:
      # in the staged-from-Windows flow it lives on the very disk being wiped.
      handoff=""
      # Orders injected into the kexec initrd (the curl|sh takeover): already in
      # RAM, in this installer's own root filesystem — nothing to mount, nothing
      # on any disk.
      if [ -f /box-installer/box-install.json ]; then
        log "using orders injected into the initrd"
        cp /box-installer/box-install.json /tmp/box-install.json
        handoff=/tmp/box-install.json
      fi
      url=$(tr ' ' '\n' < /proc/cmdline | sed -n 's/^box\.install-url=//p' | head -n1)
      if [ -z "$handoff" ] && [ -n "$url" ]; then
        log "fetching handoff from $url"
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
      # Staged installs (e.g. launched from Windows): scan every partition
      # for box-installer/box-install.json.
      if [ -z "$handoff" ] && grep -q 'box\.install-scan' /proc/cmdline; then
        mkdir -p /run/box-scan
        while read -r name type; do
          if [ "$type" != "part" ]; then continue; fi
          if mount -o ro "/dev/$name" /run/box-scan 2>/dev/null; then
            if [ -f /run/box-scan/box-installer/box-install.json ]; then
              log "found staged handoff on /dev/$name"
              cp /run/box-scan/box-installer/box-install.json /tmp/box-install.json
              handoff=/tmp/box-install.json
            fi
            umount /run/box-scan || true
          fi
          if [ -n "$handoff" ]; then break; fi
        done < <(lsblk -rno NAME,TYPE)
      fi
      if [ -z "$handoff" ]; then
        log "no handoff found (volume labeled BOX-INSTALL containing box-install.json,"
        log "box.install-url=<http url>, or a staged box-installer/ directory)."
        log "refusing to touch any disk. This machine is unchanged."
        exit 0
      fi
      log "using handoff: $handoff"
      jq . "$handoff" > /dev/null || { log "handoff is not valid JSON"; exit 1; }

      erase=$(jq -r '.erase_disk // false' "$handoff")
      if [ "$erase" != "true" ]; then
        log "handoff does not set \"erase_disk\": true — refusing to wipe anything."
        exit 1
      fi

      # ---- 2. Resolve the target disk --------------------------------------
      choice=$(jq -r '.disk // "auto"' "$handoff")
      min_gb=$(jq -r '.min_disk_gb // 8' "$handoff")

      if [ "$choice" = "auto" ]; then
        # Policy: largest internal (non-removable) disk of at least
        # min_disk_gb. Removable media (USB installer sticks) are never
        # candidates. Ambiguity is resolved toward the largest disk; pass an
        # explicit /dev/disk/by-id/... in the handoff for precise control.
        target=""
        best=0
        min_bytes=$((min_gb * 1024 * 1024 * 1024))
        while read -r name size rm type; do
          if [ "$type" != "disk" ] || [ "$rm" != "0" ]; then continue; fi
          if [ "$size" -lt "$min_bytes" ]; then continue; fi
          if [ "$size" -gt "$best" ]; then
            best=$size
            target="/dev/$name"
          fi
        done < <(lsblk -dnb -o NAME,SIZE,RM,TYPE)
        if [ -z "$target" ]; then
          log "auto disk selection found no eligible disk (internal, >= ''${min_gb}G)."
          exit 1
        fi
      else
        target=$(readlink -f "$choice")
        if [ ! -b "$target" ]; then
          log "requested disk $choice does not exist on this machine."
          exit 1
        fi
      fi
      log "target disk: $target ($(lsblk -dno SIZE "$target"))"

      # ---- 3. Reinstall guard ----------------------------------------------
      force=$(jq -r '.force // false' "$handoff")
      if [ "$force" != "true" ] && lsblk -rno LABEL "$target" | grep -qx box-root; then
        log "$target already contains a Box OS install; set \"force\": true to reinstall."
        log "doing nothing."
        exit 0
      fi

      # ---- 4. Partition + format via disko ---------------------------------
      log "partitioning $target with disko (this ERASES the disk)"
      echo "import /etc/box/disko-template.nix { device = \"$target\"; }" \
        > /tmp/box-disko.nix
      disko --mode destroy,format,mount --yes-wipe-all-disks \
        --root-mountpoint /mnt /tmp/box-disko.nix

      # ---- 5. Install the embedded Box OS closure (offline) ----------------
      system=$(cat /etc/box/system-store-path)
      log "installing Box OS from $system"
      mkdir -p /mnt/etc/box
      install -m 600 "$handoff" /mnt/etc/box/install-config.json
      nixos-install --system "$system" --no-root-passwd --no-channel-copy
      log "BOX INSTALL COMPLETE"

      # ---- 6. Finish --------------------------------------------------------
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

  # The Box OS closure rides inside the installer image so installation
  # needs no network at all.
  environment.etc."box/system-store-path".text = "${boxSystem}";
  environment.etc."box/disko-template.nix".source = ./disko-template.nix;
  environment.systemPackages = [ installScript diskoPkg pkgs.jq ];

  # disko evaluates its config at runtime; bake nixpkgs in so that also
  # works offline.
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
