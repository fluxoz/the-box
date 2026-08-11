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
  };

  outputs = { self, nixpkgs, disko, nixos-raspberrypi, apple-silicon }:
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

      # A Box for a given Raspberry Pi model, on nixos-raspberrypi's per-model
      # base (vendor kernel + firmware + boot method) — the generic aarch64 image
      # does not boot a Pi. Delivered as a flashable SD/USB image; that is the Pi
      # install path (live in-place conversion of a running Raspberry Pi OS is not
      # viable on this hardware — kexec is blocked by the RPi OS kernel). Pis are
      # a common appliance, so 3/4/5 are all first-class.
      mkPiBox = model: nixos-raspberrypi.lib.nixosSystemFull {
        specialArgs = { inherit nixos-raspberrypi; };
        modules = [
          {
            imports = [
              nixos-raspberrypi.nixosModules.sd-image
              nixos-raspberrypi.nixosModules.trusted-nix-caches
              nixos-raspberrypi.nixosModules."raspberry-pi-${model}".base
            ];
            # Raw (uncompressed) image so it dd's straight onto the card.
            sdImage.compressImage = false;
          }
          self.nixosModules.platform
          ./nix/pi.nix
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
      mkCrate = pkgs: crate: extra: pkgs.rustPlatform.buildRustPackage ({
        pname = crate;
        version = "0.1.0";
        src = rustSrc;
        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [ "-p" crate ];
        cargoTestFlags = [ "-p" crate ];
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
            meta = {
              description = "The Box installer: disk probe, storage-layout wizard (TUI) and plan";
              mainProgram = "box-installer";
            };
          };
          default = boxd;
        }))
        {
          # Per-model Pi appliance images (raw .img, dd to SD/USB): pi3-image,
          # pi4-image, pi5-image.
          aarch64-linux = nixpkgs.lib.mapAttrs'
            (m: cfg: nixpkgs.lib.nameValuePair "pi${m}-image"
              cfg.config.system.build.sdImage)
            boxPis;

          x86_64-linux = {
            installer-iso =
              self.nixosConfigurations.box-installer-iso.config.system.build.isoImage;
            installer-netboot =
              let
                build = self.nixosConfigurations.box-installer-netboot.config.system.build;
                pkgs = nixpkgs.legacyPackages.x86_64-linux;
              in
              pkgs.symlinkJoin {
                name = "box-installer-netboot";
                paths = [ build.kernel build.netbootRamdisk build.netbootIpxeScript ];
              };

            # Payload for the staged-from-Windows hot install: GRUB reads the
            # kernel/initrd from the NTFS Windows partition (the ESP is far
            # too small for the initrd), boots the RAM installer with
            # partition scanning enabled, and the installer takes it from
            # there. stage.ps1 is the user-facing entry point.
            installer-windows =
              let
                build = self.nixosConfigurations.box-installer-netboot.config.system.build;
                pkgs = nixpkgs.legacyPackages.x86_64-linux;
                # The payload lives on the NTFS Windows partition; GRUB must
                # load GPT + NTFS drivers before it can enumerate and read it.
                # The kernel params (init=/nix/store/.../init, root=fstab, …)
                # are derived at build time from the netboot iPXE script so the
                # closure-finder always gets exactly what the image expects;
                # only box.install-scan=1 is added on top.
                grubCfg = pkgs.runCommand "box-grub-embedded.cfg" { } ''
                  params=$(grep '^kernel' ${build.netbootIpxeScript}/netboot.ipxe \
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
                  linux /box-installer/bzImage $params box.install-scan=1
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
              pkgs.runCommand "box-installer-windows"
                { nativeBuildInputs = [ pkgs.grub2_efi ]; } ''
                mkdir -p $out
                grub-mkstandalone -O x86_64-efi -o $out/grubx64.efi \
                  --modules="${nixpkgs.lib.concatStringsSep " " grubModules}" \
                  "boot/grub/grub.cfg=${grubCfg}"
                cp ${build.kernel}/bzImage $out/bzImage
                cp ${build.netbootRamdisk}/initrd $out/initrd
                touch $out/box-marker
                cp ${./installers/windows/stage.ps1} $out/stage.ps1
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
                bz=$(sha256sum $out/netboot/bzImage | cut -d' ' -f1)
                ir=$(sha256sum $out/netboot/initrd  | cut -d' ' -f1)
                substitute ${./installers/linux/install.sh} $out/install.sh \
                  --replace @BZIMAGE_SHA256@ "$bz" --replace @INITRD_SHA256@ "$ir"
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
                # Agent entry points: point an agent at thebox.build and it can
                # provision + manage a Box from these alone (llms.txt convention).
                cp ${./site/llms.txt}                  $out/llms.txt
                cp ${./site/llms-full.txt}             $out/llms-full.txt
                # The pre-install Configurator (self-contained), served live so
                # the install docs' references to it are real.
                mkdir -p $out/configurator
                cp ${./configurator/index.html}        $out/configurator/index.html

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
      checks.x86_64-linux.installer-beacon = import ./nix/tests/installer-beacon.nix {
        inherit self nixpkgs;
        system = "x86_64-linux";
      };

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            nixpkgs-fmt
          ];
          RUST_BACKTRACE = "1";
        };
      });

      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/module.nix ];
        services.the-box.package =
          lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.boxd;
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
