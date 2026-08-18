{
  description = "The Box — Nix-powered plug-and-play personal server platform";

  # Declare the binary caches so anyone building a Pi image (or boxd) pulls the
  # vendor kernel + boxd from cache instead of compiling them. Without this a
  # `nix build .#pi5-image` recompiles the Raspberry Pi kernel from source.
  nixConfig = {
    extra-substituters = [
      "https://nixos-raspberrypi.cachix.org"
      "https://fluxoz.cachix.org"
    ];
    extra-trusted-public-keys = [
      "nixos-raspberrypi.cachix.org-1:4iMO9LXa8BqhU+Rpg6LQKiGa2lsNh/j2oiYLNOQ5sPI="
      "fluxoz.cachix.org-1:yzuO7pZpCoHEIT6PQiYJ1eupby/rH3Ls3Q18+VA0Krc="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
    # Purpose-built Raspberry Pi 5 support (vendor kernel + firmware + boot),
    # with a binary cache for the kernel. Brings its own nixpkgs.
    nixos-raspberrypi.url = "github:nvmd/nixos-raspberrypi/main";
    # Linux on Apple Silicon Macs (M1/M2) via the Asahi project. Brings the
    # Asahi kernel + GPU support as a NixOS module. Experimental Box target: a
    # Mac boots this through Asahi (there is no blow-away Apple-Silicon image;
    # peripheral firmware must be extracted from the target Mac).
    apple-silicon.url = "github:nix-community/nixos-apple-silicon";
    # age-based secrets, decrypted at runtime to /run/agenix (never in the Nix
    # store or plaintext in git). The Box encrypts each secret to the box's own
    # host key plus the operator's key(s), so config + secrets can live in a git
    # repo the user owns.
    agenix.url = "github:ryantm/agenix";
    agenix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, disko, nixos-raspberrypi, apple-silicon, agenix }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # The Rust workspace source, filtered to just the crates + lockfile so the
      # (multi-GB) ISOs and web assets alongside them never enter the build.
      rustSrc = nixpkgs.lib.fileset.toSource {
        root = ./.;
        fileset = nixpkgs.lib.fileset.unions [
          ./Cargo.toml
          ./Cargo.lock
          ./boxd
          ./crates
        ];
      };

      # The platform binary cache the trimmed installer (and installed boxes,
      # for updates) pull the Box OS closure from, so it isn't compiled or
      # embedded. Create it at cachix.org and paste its public key here + set
      # CACHIX_AUTH_TOKEN in CI (see docs/publish.md). The placeholder key
      # builds fine; a real install/update needs the true key.
      boxCache = {
        substituters = [ "https://fluxoz.cachix.org" ];
        trustedPublicKeys = [ "fluxoz.cachix.org-1:yzuO7pZpCoHEIT6PQiYJ1eupby/rH3Ls3Q18+VA0Krc=" ];
      };
      # True once a real key is pasted above; gates both the initrd trim and the
      # installed box's update substituter, so nothing references a cache that
      # isn't stood up yet.
      cacheReady = !(nixpkgs.lib.any
        (nixpkgs.lib.hasInfix "REPLACE_WITH_CACHIX_PUBLIC_KEY") boxCache.trustedPublicKeys);

      # The appliance system: one generic closure for every machine;
      # per-machine details arrive via the installer handoff file.
      boxOs = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.default
          ./nix/box-os.nix
        ];
      };

      # The on-disk layout our flashed Pi images leave behind. While BUILDING an
      # image the sd-image module supplies this; a channel update rebuilds the
      # system ON the box, where no sd-image module is composed — so the
      # regenerated system must declare the very same layout or it would have no
      # root filesystem. Skipped automatically whenever sd-image is present.
      piFlashedDisk = { config, lib, options, ... }: {
        config = lib.mkIf (!(options ? sdImage)) {
          fileSystems."/" = {
            device = "/dev/disk/by-label/NIXOS_SD";
            fsType = "ext4";
            options = [ "x-initrd.mount" "noatime" ];
          };
          fileSystems."/boot/firmware" = {
            device = "/dev/disk/by-label/FIRMWARE";
            fsType = "vfat";
            options = [ "noatime" "noauto" "x-systemd.automount" "x-systemd.idle-timeout=1min" ];
          };
          # Our Pi 5 images boot via the new generational "kernel" bootloader
          # (the sd-image module overrides the base's deprecated "kernelboot"
          # default) — a rebuilt system must use the same boot method the
          # flashed FIRMWARE partition was laid out for. pi3/pi4 stay on the
          # base's uboot default, which the image path doesn't touch.
          boot.loader.raspberry-pi.bootloader =
            lib.mkIf (config.boot.loader.raspberry-pi.variant == "5") "kernel";
        };
      };

      # EVERY Box system composes through this one builder: the flashable Pi
      # image, the x86 appliance, and — critically — the per-box repo boxd
      # regenerates on a channel update (hostgen emits `the-box.lib.boxSystem`).
      # One entry point means the update path cannot drift from the image path:
      # a Pi switched onto a generic aarch64 system (no vendor kernel, no
      # firmware-partition boot method) comes back from its next reboot a brick.
      #   board = null              → generic appliance (x86/aarch64)
      #   board = "pi3"|"pi4"|"pi5" → nixos-raspberrypi vendor base + Pi disk layout
      #   gpu   = "nvidia"          → + driver, CUDA userspace and container CDI
      #                               (the spare-RTX-PC inference Box; generic
      #                               appliance only — Jetson arrives as a board)
      boxSystem = { system, board ? null, gpu ? null, modules ? [ ] }:
        assert nixpkgs.lib.assertMsg (gpu == null || gpu == "nvidia" || gpu == "amd")
          "boxSystem: unsupported gpu ${toString gpu} (nvidia|amd)";
        assert nixpkgs.lib.assertMsg (gpu == null || board == null)
          "boxSystem: gpu is the generic-appliance axis; a board brings its own GPU story";
        if board == null then
          nixpkgs.lib.nixosSystem {
            inherit system;
            modules = [
              self.nixosModules.platform
              self.nixosModules.hardwareAppliance
            ] ++ nixpkgs.lib.optional (gpu == "nvidia") ./nix/gpu-nvidia.nix
            ++ nixpkgs.lib.optional (gpu == "amd") ./nix/gpu-amd.nix
            ++ modules;
          }
        else
          assert nixpkgs.lib.assertMsg (system == "aarch64-linux")
            "boxSystem: board ${board} is aarch64-linux hardware, not ${system}";
          let
            base = {
              pi3 = nixos-raspberrypi.nixosModules.raspberry-pi-3.base;
              pi4 = nixos-raspberrypi.nixosModules.raspberry-pi-4.base;
              pi5 = nixos-raspberrypi.nixosModules.raspberry-pi-5.base;
            }.${board} or (throw "boxSystem: unsupported board ${board} (pi3|pi4|pi5)");
          in
          nixos-raspberrypi.lib.nixosSystemFull {
            specialArgs = { inherit nixos-raspberrypi; };
            modules = [
              base
              nixos-raspberrypi.nixosModules.trusted-nix-caches
              piFlashedDisk
              self.nixosModules.platform
              ./nix/pi.nix
            ] ++ modules;
          };

      # A Box for a given Raspberry Pi model, on nixos-raspberrypi's per-model
      # base (vendor kernel + firmware + boot method) — the generic aarch64 image
      # does not boot a Pi. Delivered as a flashable SD/USB image; that is the Pi
      # install path (live in-place conversion of a running Raspberry Pi OS is not
      # viable on this hardware — kexec is blocked by the RPi OS kernel). Pis are
      # a common appliance, so 3/4/5 are all first-class. Composed via boxSystem
      # (the same builder channel updates use) + the sd-image builder on top.
      mkPiBox = model: boxSystem {
        system = "aarch64-linux";
        board = "pi${model}";
        modules = [
          {
            imports = [ nixos-raspberrypi.nixosModules.sd-image ];
            # Raw (uncompressed) image so it dd's straight onto the card.
            sdImage.compressImage = false;
          }
        ];
      };
      boxPis = nixpkgs.lib.genAttrs [ "3" "4" "5" ] mkPiBox;

      # EXPERIMENTAL: a Box for Apple Silicon Macs (M1/M2), riding on the Asahi
      # project's kernel/GPU support. Unlike the Pi there is no flashable image:
      # Apple Silicon can't boot an arbitrary USB image and keeps a small macOS
      # firmware stub, so the flow is "install Asahi Linux, then switch the
      # system to this config" (the OS-tier reconciler path, same as any Box
      # update). Peripheral firmware (Wi-Fi/BT) is extracted from the target
      # Mac; extraction is off here so the config evaluates without hardware.
      macBox = nixpkgs.lib.nixosSystem {
        system = "aarch64-linux";
        modules = [
          apple-silicon.nixosModules.default
          self.nixosModules.platform
          ./nix/mac.nix
        ];
      };

      # A per-box config composes the reusable platform (boxd + Box software)
      # with a hardware layer and its own host/service modules — the OS tier of
      # the reconciler. Hosts are auto-discovered from nodes/hosts/ (drop a
      # directory in, get a nixosConfigurations.<id> out), which is what boxd's
      # generated dendritic config repo mirrors on a real fleet.
      mkBoxHost = id: nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.platform
          self.nixosModules.hardwareAppliance
          (./nodes/hosts + "/${id}")
        ];
      };
      boxHostNames = builtins.attrNames (builtins.readDir ./nodes/hosts);
      boxHosts = nixpkgs.lib.genAttrs boxHostNames mkBoxHost;

      # RAM-resident automated installer embedding the Box OS closure.
      # Delivered as an ISO (USB/virtual media), as netboot artifacts (PXE /
      # batch installs), or staged from a running OS (Windows path).
      installerWith = profileModule: nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        specialArgs = {
          boxSystem = boxOs.config.system.build.toplevel;
          diskoPkg = disko.packages.x86_64-linux.disko;
          boxInstaller = self.packages.x86_64-linux.box-installer;
          boxDaemon = self.packages.x86_64-linux.boxd;
          inherit boxCache;
          nixpkgsSrc = nixpkgs;
        };
        modules = [
          profileModule
          ./nix/installer.nix
        ];
      };

      # Build one workspace crate as its own package from the filtered source.
      # The one version literal for the whole platform, read from the Cargo
      # workspace so the Nix packages, the binaries' own --version, and the
      # release label stamped into /etc/box/platform.json can never disagree.
      boxVersion =
        (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

      mkCrate = pkgs: crate: extra: pkgs.rustPlatform.buildRustPackage ({
        pname = crate;
        version = boxVersion;
        src = rustSrc;
        # webauthn-rs (security-key sign-in) pulls openssl-sys.
        nativeBuildInputs = [ pkgs.pkg-config ];
        buildInputs = [ pkgs.openssl ];
        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [ "-p" crate ];
        cargoTestFlags = [ "-p" crate ];
        # The secrets tests shell out to age/age-keygen, and they are the proof
        # that a secret is never written in plaintext. Without these here they
        # used to skip themselves in the one place tests ran automatically — the
        # Nix build — and report success having checked nothing.
        nativeCheckInputs = [ pkgs.age pkgs.git ];
      } // extra);
    in
    {
      packages = nixpkgs.lib.recursiveUpdate
        (forAllSystems (pkgs: rec {
          boxd = mkCrate pkgs "boxd" {
            meta = {
              description = "The Box daemon: declarative service management, dashboard and agent API";
              mainProgram = "boxd";
            };
          };
          box-installer = mkCrate pkgs "box-installer" {
            # box-core is where the disk layouts and the refuse-on-ambiguity
            # rule live — the code that decides which disks get wiped. It ships
            # only inside this binary and has no package of its own, so its
            # tests ran in no automated build at all until they were named here.
            cargoTestFlags = [ "-p" "box-installer" "-p" "box-core" ];
            meta = {
              description = "The Box installer: disk probe, storage-layout wizard (TUI) and plan";
              mainProgram = "box-installer";
            };
          };
          default = boxd;
        }))
        {
          # Per-model Pi appliance images (raw .img, dd to SD/USB): pi3-image,
          # pi4-image, pi5-image. Each gets a `-personalizable` sibling: the
          # same image plus a fixed-size claim file on the FAT boot partition,
          # packed as a three-member gzip whose middle member is a STORED
          # deflate block. That lets a download be personalized with a
          # byte-for-byte overwrite at a known offset — no recompression and no
          # per-user artifact. See docs/claim-flow-spec.md.
          aarch64-linux =
            let
              pkgs = nixpkgs.legacyPackages.aarch64-linux;
              personalizable = name: sdImage:
                pkgs.runCommand "${name}-personalizable"
                  {
                    nativeBuildInputs = [ pkgs.python3 pkgs.mtools pkgs.util-linux ];
                  } ''
                  img=$(find ${sdImage} -name '*.img' | head -n1)
                  [ -n "$img" ] || { echo "no .img found in ${sdImage}" >&2; exit 1; }
                  mkdir -p "$out"
                  cp "$img" ./work.img
                  chmod +w ./work.img
                  python3 ${./scripts/personalizable-image.py} \
                    ./work.img "$out/${name}.img.gz" "$out/${name}.img.gz.manifest.json"
                  # Hash computed in the same derivation that produced the
                  # artifact, matching how install.sh's netboot hashes are
                  # stamped — so the published hash is provably of these bytes.
                  sha256sum "$out/${name}.img.gz" | cut -d' ' -f1 \
                    > "$out/${name}.img.gz.sha256"
                '';
            in
            nixpkgs.lib.mapAttrs'
              (m: cfg: nixpkgs.lib.nameValuePair "pi${m}-image"
                cfg.config.system.build.sdImage)
              boxPis
            // nixpkgs.lib.mapAttrs'
              (m: cfg: nixpkgs.lib.nameValuePair "pi${m}-image-personalizable"
                (personalizable "thebox-pi${m}" cfg.config.system.build.sdImage))
              boxPis;

          x86_64-linux =
            let
              pkgs = nixpkgs.legacyPackages.x86_64-linux;
              netbootBuild = self.nixosConfigurations.box-installer-netboot.config.system.build;
              # One standalone GRUB for every delivery of the RAM installer: it
              # searches every disk for the payload directory's marker file, so
              # the same binary boots from an NTFS Windows partition or the USB
              # image's FAT partition (hence both filesystems' modules). The
              # kernel params (init=/nix/store/.../init, root=fstab, …) are
              # derived at build time from the netboot iPXE script so the
              # closure-finder always gets exactly what the image expects; each
              # delivery adds only its own extra params.
              grubEfiWith = extraParams:
                let
                  grubCfg = pkgs.runCommand "box-grub-embedded.cfg" { } ''
                    params=$(grep '^kernel' ${netbootBuild.netbootIpxeScript}/netboot.ipxe \
                      | sed -e 's/^kernel bzImage //' \
                            -e 's/ initrd=initrd//' \
                            -e 's/ *''${cmdline}//')
                    cat > $out <<EOF
                    insmod part_gpt
                    insmod part_msdos
                    insmod ntfs
                    insmod fat
                    insmod search_fs_file
                    set timeout=0
                    search --no-floppy --file /box-installer/box-marker --set root
                    linux /box-installer/bzImage $params ${extraParams}
                    initrd /box-installer/initrd
                    boot
                    EOF
                  '';
                  # Explicitly embed every module the config needs — the default
                  # standalone set does not reliably include part_gpt/ntfs.
                  grubModules = [
                    "part_gpt"
                    "part_msdos"
                    "ntfs"
                    "ntfscomp"
                    "fat"
                    "search"
                    "search_fs_file"
                    "linux"
                    "normal"
                    "echo"
                    "test"
                    "configfile"
                    "boot"
                    "all_video"
                  ];
                in
                pkgs.runCommand "box-grub-efi"
                  { nativeBuildInputs = [ pkgs.grub2_efi ]; } ''
                  mkdir -p $out
                  grub-mkstandalone -O x86_64-efi -o $out/grubx64.efi \
                    --modules="${nixpkgs.lib.concatStringsSep " " grubModules}" \
                    "boot/grub/grub.cfg=${grubCfg}"
                '';
            in
            {
            installer-iso =
              self.nixosConfigurations.box-installer-iso.config.system.build.isoImage;
            installer-netboot =
              pkgs.symlinkJoin {
                name = "box-installer-netboot";
                paths = [ netbootBuild.kernel netbootBuild.netbootRamdisk netbootBuild.netbootIpxeScript ];
              };

            # Payload for the staged-from-Windows hot install: GRUB reads the
            # kernel/initrd from the NTFS Windows partition (the ESP is far
            # too small for the initrd), boots the RAM installer with
            # partition scanning enabled, and the installer takes it from
            # there. stage.ps1 is the user-facing entry point.
            installer-windows = pkgs.runCommand "box-installer-windows" { } ''
              mkdir -p $out
              cp ${grubEfiWith "box.install-scan=1"}/grubx64.efi $out/grubx64.efi
              cp ${netbootBuild.kernel}/bzImage $out/bzImage
              cp ${netbootBuild.netbootRamdisk}/initrd $out/initrd
              touch $out/box-marker
              cp ${./installers/windows/stage.ps1} $out/stage.ps1
            '';

            # The flashable x86 image (thebox-x86): a GPT disk image whose one
            # FAT32 partition — labeled BOX-INSTALL — carries that same GRUB
            # and RAM installer. Write it to a USB stick, boot the machine from
            # it, and the installer reads its orders from box-claim.txt on the
            # same partition, written there as the image downloaded (the same
            # stored-deflate mechanism as the Pi images; the packer in
            # scripts/personalizable-image.py adds the placeholder). An
            # unpersonalized copy carries only the placeholder, so it falls
            # back to the on-box setup wizard and touches nothing by itself.
            installer-usb = pkgs.runCommand "box-installer-usb"
              {
                nativeBuildInputs = [ pkgs.dosfstools pkgs.mtools pkgs.util-linux ];
              } ''
              mkdir -p work/box-installer work/EFI/BOOT
              cp ${netbootBuild.kernel}/bzImage        work/box-installer/bzImage
              cp ${netbootBuild.netbootRamdisk}/initrd work/box-installer/initrd
              touch work/box-installer/box-marker
              cp ${grubEfiWith ""}/grubx64.efi work/EFI/BOOT/BOOTX64.EFI

              # ESP sized to the payload plus FAT overhead and the claim file
              # the packer adds later, in whole MiB.
              payload=$(du -sb work | cut -f1)
              esp_mib=$(( payload / 1048576 + 65 ))

              # Built as a bare partition image (a sandboxed build has no loop
              # devices), then placed at the 1 MiB alignment boundary. The
              # label is the one the installer already mounts orders from.
              # Serial and GUIDs pinned: sfdisk and mkfs otherwise mint random
              # ones, and a single random byte shifts every offset in the
              # packed download, making local and CI manifests incomparable.
              truncate -s "$esp_mib"M esp.img
              mkfs.vfat -F 32 -n BOX-INSTALL -i 26942694 esp.img
              mcopy -s -i esp.img work/EFI work/box-installer ::/

              mkdir -p $out
              img=$out/thebox-x86.img
              truncate -s $(( (esp_mib + 2) * 1048576 )) "$img"
              sfdisk --quiet "$img" <<EOF
              label: gpt
              label-id: 4B0F2694-C0DE-4B0F-9000-000000002694
              start=2048, size=$(( esp_mib * 2048 )), type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, uuid=4B0F2694-C0DE-4B0F-9000-0000000000E5, name=ESP
              EOF
              dd if=esp.img of="$img" bs=1M seek=1 conv=notrunc status=none
              sfdisk --verify "$img"
            '';

            # The publishable thebox.build bundle: netboot artifacts, the curl|sh
            # installer with real checksums stamped in, the Windows one-liner,
            # the landing page, and SHA256SUMS over everything.
            site =
              let
                pkgs = nixpkgs.legacyPackages.x86_64-linux;
                netboot = self.packages.x86_64-linux.installer-netboot;
              in
              pkgs.runCommand "thebox-site" { } ''
                mkdir -p $out/netboot
                cp ${netboot}/bzImage      $out/netboot/bzImage
                cp ${netboot}/initrd       $out/netboot/initrd
                cp ${netboot}/netboot.ipxe $out/netboot/netboot.ipxe

                # Stamp the real artifact hashes into the published installer so
                # it verifies what it fetches before kexec.
                bz=$(sha256sum $out/netboot/bzImage      | cut -d' ' -f1)
                ir=$(sha256sum $out/netboot/initrd       | cut -d' ' -f1)
                ip=$(sha256sum $out/netboot/netboot.ipxe | cut -d' ' -f1)
                substitute ${./installers/linux/install.sh} $out/install.sh \
                  --replace @BZIMAGE_SHA256@ "$bz" --replace @INITRD_SHA256@ "$ir" \
                  --replace @IPXE_SHA256@ "$ip"
                chmod +x $out/install.sh

                cp ${./installers/windows/install.ps1} $out/install.ps1
                cp ${./installers/macos/README.md}     $out/mac.txt
                cp ${./site/index.html}                $out/index.html
                cp ${./site/docs.css}                  $out/docs.css
                # Brand + social preview assets.
                cp ${./site/favicon.svg}               $out/favicon.svg
                cp ${./site/logo.svg}                  $out/logo.svg
                cp ${./site/og.png}                    $out/og.png
                cp -r ${./site/docs}                   $out/docs
                # The one-liner that connects a coding agent to a Box:
                #   curl -fsSL https://thebox.build/connect | sh
                cp ${./site/connect}                   $out/connect
                # Agent entry points: point an agent at thebox.build and it can
                # provision + manage a Box from these alone (llms.txt convention).
                cp ${./site/llms.txt}                  $out/llms.txt
                cp ${./site/llms-full.txt}             $out/llms-full.txt
                cp ${./site/prompt.md}                 $out/prompt.md
                # The pre-install Configurator (self-contained), served live so
                # the install docs' references to it are real.
                mkdir -p $out/configurator
                cp ${./configurator/index.html}        $out/configurator/index.html
                # The store: hardware at cost + flat fee, live Stripe checkout.
                mkdir -p $out/store
                cp ${./site/store/index.html}          $out/store/index.html
                cp -r ${./site/store/img}              $out/store/img
                # The argument: why own the machine at all.
                mkdir -p $out/why
                cp ${./site/why/index.html}            $out/why/index.html

                ( cd $out && find . -type f ! -name SHA256SUMS -print0 \
                    | sort -z | xargs -0 sha256sum > SHA256SUMS )
              '';
          };
        };

      nixosConfigurations = {
        box-os = boxOs;
        box-os-mac = macBox; # experimental: Apple Silicon (M1/M2) via Asahi
        box-installer-iso = installerWith
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix";
        box-installer-netboot = installerWith
          "${nixpkgs}/nixos/modules/installer/netboot/netboot-minimal.nix";
      } // boxHosts
        // (nixpkgs.lib.mapAttrs'
             (m: cfg: nixpkgs.lib.nameValuePair "box-os-pi${m}" cfg) boxPis);

      # Live VM proof that the OS-tier switch + system rollback work on a
      # booted Box (exercises the exact command sequence ostier.rs drives).
      checks.x86_64-linux.os-switch = import ./nix/tests/os-switch.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };

      # Two Boxes on a shared network find each other over mDNS (fleet discovery).
      checks.x86_64-linux.fleet-discovery = import ./nix/tests/fleet-discovery.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };

      # An operator manages Boxes remotely; management is gated per-Box, scoped to
      # one Box, and revocable (fleet write-authz).
      checks.x86_64-linux.fleet-manage = import ./nix/tests/fleet-manage.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };

      # Boxes join a Headscale WireGuard mesh; the tunnel carries traffic (one Box
      # reaches another's dashboard over the tailnet) + tailnet fleet discovery.
      checks.x86_64-linux.fleet-mesh = import ./nix/tests/fleet-mesh.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      checks.x86_64-linux.unattended-claim = import ./nix/tests/unattended-claim.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      checks.x86_64-linux.installer-beacon = import ./nix/tests/installer-beacon.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      checks.x86_64-linux.service-proxy = import ./nix/tests/service-proxy.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      checks.x86_64-linux.container = import ./nix/tests/container.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      # The path a user actually takes: deploy through boxd, not by hand-writing
      # `services.the-box.containers.*` into a module.
      checks.x86_64-linux.deploy-through-boxd = import ./nix/tests/deploy-through-boxd.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      checks.x86_64-linux.secret-env = import ./nix/tests/secret-env.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      # The sandboxed build step: rootless podman as the boxd system user,
      # builder image from the closure, no registry, no network. The subuid /
      # wrapper / delegation plumbing this proves fails silently everywhere else.
      checks.x86_64-linux.build-step = import ./nix/tests/build-step.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };
      checks.x86_64-linux.restore = import ./nix/tests/restore.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };

      # Eval-level proof the BYO-GPU layer composes: a gpu = "nvidia" appliance
      # instantiates to a system derivation — driver wired, CDI toolkit on,
      # unfree allowed for exactly the driver. CI has no GPU, so evaluation is
      # the check; the first spare-RTX machine is the live verification.
      # Same proof for the unified-memory (Strix Halo) layer: gpu = "amd"
      # evaluates with the GTT/TTM boot params that let the iGPU take the
      # memory — the exact settings the store's tokens-per-second guarantee
      # stands on. All free software; the check is that nothing regresses it.
      checks.x86_64-linux.gpu-amd-eval =
        let
          sys = boxSystem {
            system = "x86_64-linux";
            gpu = "amd";
            modules = [{ networking.hostName = "amdbox"; }];
          };
          ok =
            assert nixpkgs.lib.assertMsg
              (builtins.elem "ttm.pages_limit=32505856" sys.config.boot.kernelParams)
              "gpu-amd-eval: the unified-memory boot params must be present";
            assert nixpkgs.lib.assertMsg
              (sys.config.services.the-box.gpuContainerDevices == [ "/dev/dri:/dev/dri" ])
              "gpu-amd-eval: gpu containers must receive /dev/dri, not CDI";
            builtins.unsafeDiscardStringContext sys.config.system.build.toplevel.drvPath;
        in
        nixpkgs.legacyPackages.x86_64-linux.runCommand "gpu-amd-eval-ok"
          { inherit ok; } "echo $ok > $out";

      checks.x86_64-linux.gpu-eval =
        let
          sys = boxSystem {
            system = "x86_64-linux";
            gpu = "nvidia";
            modules = [{ networking.hostName = "gpubox"; }];
          };
          # unsafeDiscardStringContext: instantiating the toplevel proves the
          # whole system EVALUATES (drivers, CDI, unfree gate); keeping the
          # context would make this check BUILD the driver stack, which CI has
          # no business doing.
          ok =
            assert nixpkgs.lib.assertMsg
              sys.config.hardware.nvidia-container-toolkit.enable
              "gpu-eval: the container toolkit (CDI) must be on for a gpu box";
            builtins.unsafeDiscardStringContext sys.config.system.build.toplevel.drvPath;
        in
        nixpkgs.legacyPackages.x86_64-linux.runCommand "gpu-eval-ok"
          { inherit ok; } "echo $ok > $out";

      # Eval-level proof that a channel-update rebuild produces the SAME bootable
      # Pi system as the flashed image: identical vendor kernel derivation, boot
      # method, and disk layout. This is what stands between a Pi and an update
      # that bricks on the next reboot — if boxSystem and the image ever drift,
      # this check fails before anything ships.
      checks.x86_64-linux.pi-update-parity =
        let
          parityOk = model:
            let
              runtime = boxSystem {
                system = "aarch64-linux";
                board = "pi${model}";
                modules = [ { networking.hostName = "parity"; } ];
              };
              image = boxPis.${model};
              eq = name: a: b:
                nixpkgs.lib.assertMsg (a == b)
                  "pi${model} update parity: ${name} differs between the flashed image and a channel-update rebuild (${builtins.toJSON a} vs ${builtins.toJSON b})";
            in
            assert eq "vendor kernel"
              runtime.config.boot.kernelPackages.kernel.drvPath
              image.config.boot.kernelPackages.kernel.drvPath;
            assert eq "boot method"
              runtime.config.boot.loader.raspberry-pi.bootloader
              image.config.boot.loader.raspberry-pi.bootloader;
            assert eq "boot variant"
              runtime.config.boot.loader.raspberry-pi.variant
              image.config.boot.loader.raspberry-pi.variant;
            assert eq "root filesystem"
              runtime.config.fileSystems."/".device
              image.config.fileSystems."/".device;
            assert eq "root fsType"
              runtime.config.fileSystems."/".fsType
              image.config.fileSystems."/".fsType;
            assert eq "firmware partition"
              runtime.config.fileSystems."/boot/firmware".device
              image.config.fileSystems."/boot/firmware".device;
            # A box rebuilt by a channel update must report the release it is
            # actually running. Nothing used to set this, so an updated box
            # kept reporting "dev" forever.
            assert eq "platform release"
              runtime.config.services.the-box.platform.release
              image.config.services.the-box.platform.release;
            assert nixpkgs.lib.assertMsg
              (runtime.config.services.the-box.platform.release == boxVersion)
              "pi${model}: platform release label is not the workspace version (${boxVersion})";
            true;
        in
        assert nixpkgs.lib.all parityOk [ "3" "4" "5" ];
        nixpkgs.legacyPackages.x86_64-linux.runCommand "pi-update-parity-ok" { }
          "echo 'channel-update rebuild == flashed image (kernel, boot, disk) for pi3/pi4/pi5' > $out";

      # One environment for everything: `nix develop -c <cmd>`. Every tool the
      # work needs lives here, pinned to this flake's nixpkgs, so it resolves to
      # the same store paths every time. Reaching for `nix shell nixpkgs#thing`
      # instead pulls from the mutable registry and mints a fresh closure per
      # invocation, which is how a store fills up with near-duplicates.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust
            cargo
            rustc
            pkg-config
            openssl
            rustfmt
            clippy
            rust-analyzer
            # Nix + CI files
            nixpkgs-fmt
            yq-go # reading/validating .github workflows
            shellcheck # the CI shell scripts
            sqlite # inspecting the nix db, boxd state
            # The console: syntax-checking dash.js, and driving a real browser
            # to verify the dashboard end to end (screenshots, SPA navigation).
            nodejs
            chromium
            chromedriver
            (python3.withPackages (ps: [ ps.selenium ]))
            # Runtime deps boxd shells out to, so the dev loop matches a Box.
            age
            git
            restic
            curl # cfapi, and the connect one-liner the tests drive
          ];
          RUST_BACKTRACE = "1";
          # Selenium needs to find the pinned browser rather than a system one.
          CHROME_BIN = "${pkgs.chromium}/bin/chromium";
        };

        # Lean shell for building ON a Box (see scripts/dev-pi.sh): the Rust
        # toolchain and the binaries boxd shells out to, and nothing else. The
        # default shell carries a browser and a Python env for driving the
        # console, which have no business being fetched onto a Raspberry Pi.
        build = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc pkg-config openssl age git ];
          RUST_BACKTRACE = "1";
        };
      });

      # The composition entry point for generated per-box repos: hostgen emits
      # `the-box.lib.boxSystem { system; board; modules; }`, so the platform —
      # not the generated file — decides how a board becomes a bootable system.
      lib.boxSystem = boxSystem;

      nixosModules.default = { pkgs, lib, ... }: {
        # module.nix is the boxd service; agenix provides the age.* secret
        # options every Box uses. Both are part of the universal base so any
        # composition path (box-os, per-box flakes, the installer) gets them.
        imports = [ ./nix/module.nix agenix.nixosModules.default ];
        services.the-box.package =
          lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.boxd;
        # Stamp which platform release this closure is. Set here, in the base
        # every composition path imports, so an image AND a box rebuilt by a
        # channel update (hostgen -> lib.boxSystem) both report the truth —
        # previously nothing set it, so an updated box still said "dev".
        services.the-box.platform.release = lib.mkDefault boxVersion;
        # So a channel update downloads the prebuilt platform closure from the
        # cache instead of compiling it on the box. Empty until a real key is set.
        services.the-box.platform.substituters =
          lib.mkDefault (lib.optionals cacheReady boxCache.substituters);
        services.the-box.platform.trustedPublicKeys =
          lib.mkDefault (lib.optionals cacheReady boxCache.trustedPublicKeys);
      };

      # The platform layer a per-box config composes on: the Box software
      # (boxd) plus the shared platform defaults, with the boxd package wired.
      nixosModules.platform = {
        imports = [
          self.nixosModules.default
          ./nix/platform.nix
        ];
      };

      # How a Box installed by the appliance boots. Exposed as a module so
      # both the in-repo hosts and boxd's generated standalone flakes compose
      # the exact same layer. (disko-derived hardware supersedes this later.)
      nixosModules.hardwareAppliance = ./nix/hardware-appliance.nix;
    };
}
