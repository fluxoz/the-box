{
  description = "The Box — Nix-powered plug-and-play personal server platform";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, disko }:
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

      # The appliance system: one generic closure for every machine;
      # per-machine details arrive via the installer handoff file.
      boxOs = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          self.nixosModules.default
          ./nix/box-os.nix
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

                ( cd $out && find . -type f ! -name SHA256SUMS -print0 \
                    | sort -z | xargs -0 sha256sum > SHA256SUMS )
              '';
          };
        };

      nixosConfigurations = {
        box-os = boxOs;
        box-installer-iso = installerWith
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix";
        box-installer-netboot = installerWith
          "${nixpkgs}/nixos/modules/installer/netboot/netboot-minimal.nix";
      } // boxHosts;

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
