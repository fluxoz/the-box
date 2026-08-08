# A per-box host node. This is the shape boxd generates on a real fleet:
# identity + a hardware layer + auto-discovered service modules, each one
# compiled from a box.toml entry. Drop a .nix file in services/ and it joins
# the system on the next build — the dendritic property boxd relies on.
{ lib, ... }:
let
  servicesDir = ./services;
  serviceModules = lib.optionals (builtins.pathExists servicesDir) (
    map (name: servicesDir + "/${name}") (
      builtins.filter (lib.hasSuffix ".nix") (
        builtins.attrNames (builtins.readDir servicesDir)
      )
    )
  );
in
{
  # Platform + hardware layers are composed by the flake; this node contributes
  # identity and its auto-discovered, box.toml-compiled service modules.
  imports = serviceModules;

  networking.hostName = "demo-box";
}
