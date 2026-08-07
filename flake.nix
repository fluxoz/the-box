{
  description = "The Box — Nix-powered plug-and-play personal server platform";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: rec {
        boxd = pkgs.rustPlatform.buildRustPackage {
          pname = "boxd";
          version = "0.1.0";
          src = ./boxd;
          cargoLock.lockFile = ./boxd/Cargo.lock;
          meta = {
            description = "The Box daemon: declarative service management, dashboard and agent API";
            mainProgram = "boxd";
          };
        };
        default = boxd;
      });

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
    };
}
