# Live proof of the OS-tier switch + system rollback in a booted Box VM.
#
# It boots system A, then drives the *exact* command sequence ostier.rs uses —
# `nix-env -p <system profile> --set B` + `switch-to-configuration switch`, then
# `nix-env --rollback` + re-switch — asserting each time that the *running*
# system actually changed (hostname + the nginx-served page) and that boxd
# survives the switch (ostier's health gate). This is the part unit tests can't
# reach: that the mechanism works on a live system.
#
# System B is a specialisation of the node so it inherits the test harness's
# backdoor/instrumentation (otherwise switching away from the base config would
# cut the driver's command channel). We still register and activate it through
# the system profile exactly as ostier does — the specialisation is just how a
# second full system toplevel is made available inside one VM.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};
  mkSite = text: pkgs.writeTextDir "index.html" text;

  # A minimal Box: the real platform (boxd + nginx-backed sites), with NM/avahi
  # dropped so the VM boots fast and deterministically — not what this exercises.
  box = page: hostName: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = hostName;
    networking.networkmanager.enable = lib.mkForce false;
    services.avahi.enable = lib.mkForce false;
    services.the-box.sites.hello.root = "${mkSite page}";
    # The test VM boots via direct kernel, not a disk bootloader, so
    # switch-to-configuration must not try to (re)install GRUB.
    boot.loader.grub.enable = lib.mkForce false;
  };
in
pkgs.testers.runNixOSTest {
  name = "box-os-switch";

  nodes.machine = { lib, ... }: {
    imports = [ (box "<h1>A-CONTENT</h1>" "box-a") ];
    # System B, as a specialisation: same Box, different identity + content.
    specialisation.boxB.configuration = {
      networking.hostName = lib.mkForce "box-b";
      services.the-box.sites.hello.root = lib.mkForce "${mkSite "<h1>B-CONTENT</h1>"}";
    };
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("nginx.service")
    machine.wait_until_succeeds("curl -sf http://localhost | grep A-CONTENT", timeout=60)
    machine.wait_until_succeeds("systemctl is-active boxd", timeout=60)

    a = machine.succeed("readlink -f /run/current-system").strip()
    b = machine.succeed("readlink -f /run/current-system/specialisation/boxB").strip()

    # Seed the booted system as the baseline generation. On a real Box the
    # system profile already carries the running generation; a fresh test VM
    # boots via an init= handoff without one, so establish it explicitly —
    # otherwise there's nothing for --rollback to return to.
    machine.succeed(f"nix-env -p /nix/var/nix/profiles/system --set {a}")

    # --- OS-tier switch to system B (== ostier::activate) ---
    # The nginx-served content is the functional proof the system flipped to B;
    # (hostname is only applied at boot, so it isn't a switch-time signal).
    machine.succeed(f"nix-env -p /nix/var/nix/profiles/system --set {b}")
    machine.succeed(f"{b}/bin/switch-to-configuration switch")
    machine.wait_until_succeeds("curl -sf http://localhost | grep B-CONTENT", timeout=60)
    machine.fail("curl -sf http://localhost | grep A-CONTENT")
    # ostier's health gate: boxd must survive the switch.
    machine.wait_until_succeeds("systemctl is-active boxd", timeout=60)

    # --- OS-tier rollback to the previous system generation (== ostier::rollback) ---
    machine.succeed("nix-env -p /nix/var/nix/profiles/system --rollback")
    machine.succeed("$(readlink -f /nix/var/nix/profiles/system)/bin/switch-to-configuration switch")
    machine.wait_until_succeeds("curl -sf http://localhost | grep A-CONTENT", timeout=60)
    machine.fail("curl -sf http://localhost | grep B-CONTENT")
    machine.wait_until_succeeds("systemctl is-active boxd", timeout=60)

    print("OS-tier switch A->B and rollback B->A verified on a live system")
  '';
}
