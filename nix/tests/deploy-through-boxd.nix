# Deploy a container the way a user actually does — through boxd's API — on a
# real Box, and check that what boxd then says about it is true.
#
# Every other container test wires `services.the-box.containers.*` by hand, which
# is why a whole class of breakage survived: the path from "an agent calls
# deploy" to "a podman unit exists" was never exercised. It was broken in four
# separate places at once — the generation flake referenced a file tree that is
# never written for a container (so the build failed at eval on any Box with
# Nix), a failed build left the service wedged in box.toml, nothing dispatched
# structural changes to the OS tier, and the OS-tier module was rendered with
# `port = 0` and no secrets.
#
# What this can prove in a sandbox: the fast path builds, the OS-tier repo is
# rendered correctly, boxd reports the service honestly, and the root unit that
# applies it exists. Building a whole NixOS system from the generated repo needs
# the platform flake from the network, so that stays a live-box concern.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};
in
pkgs.testers.runNixOSTest {
  name = "box-deploy-through-boxd";

  nodes.box = { ... }: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = "box";
    networking.networkmanager.enable = lib.mkForce false;
    services.avahi.enable = lib.mkForce false;
    boot.loader.grub.enable = lib.mkForce false;
    virtualisation.memorySize = 3072;
    virtualisation.diskSize = 8192;

    # The vhost an OS-tier apply would generate for the site deployed below.
    # Declared here because building a whole system inside the VM needs the
    # platform flake from the network; what matters for this test is that the
    # vhost's root is the live generation, which is the platform's default.
    services.the-box.sites.blog.domain = "blog.example.com";

    # The platform pins generations to its own nixpkgs, so building one needs no
    # network — that pinning is part of what this test checks. The VM still
    # needs the build inputs of a `runCommand` in its store, or it would try to
    # build stdenv from the bootstrap tools with no substituter to fetch from.
    system.extraDependencies = [
      nixpkgs
      pkgs.stdenvNoCC
      (pkgs.runCommand "box-generation-inputs" { } "mkdir $out").inputDerivation
    ];
  };

  testScript = ''
    box.wait_for_unit("multi-user.target")
    box.wait_until_succeeds("systemctl is-active boxd", timeout=90)
    box.wait_for_open_port(2693)

    # An agent's credential, minted the way `boxd provision` does.
    token = box.succeed("boxd auth mint --label test 2>/dev/null | tail -1").strip()
    auth = f'-H "Authorization: Bearer {token}"'

    def api(method, path, data=None):
        d = f"-d '{data}'" if data else ""
        return box.succeed(
            f"curl -sf -X {method} {auth} -H 'Content-Type: application/json' {d} "
            f"http://127.0.0.1:2693{path}"
        )

    # --- deploy a container through the API ---------------------------------
    # This VM has no network, which is the point: a Box must be able to build a
    # generation without reaching channels.nixos.org for the flake registry.
    # Before the fix this failed at eval anyway, because the generation flake
    # copied services/db/www, which a container never materializes.
    out = api("POST", "/api/v1/services",
              '{"name":"db","template":"container",'
              '"params":{"image":"postgres:16","expose":"internal"}}')
    print("deploy said:", out)
    assert '"generation"' in out, f"container deploy did not produce a generation: {out}"

    # A second deploy must still work: a failed build used to leave the broken
    # service in box.toml and poison every later deploy.
    api("POST", "/api/v1/services",
        '{"name":"blog","template":"static-site","params":{"index_html":"<h1>hi</h1>"}}')
    services = api("GET", "/api/v1/services")
    assert '"db"' in services and '"blog"' in services, services

    # --- boxd must not claim a container is running when it isn't -----------
    # Nothing has applied the OS tier in this test, so podman has no unit.
    box.fail("systemctl cat podman-db.service")
    import json
    listed = json.loads(services)
    db = next(s for s in listed if s["name"] == "db")
    blog = next(s for s in listed if s["name"] == "blog")
    assert db["state"] != "active", f"a container with no podman unit is not active: {db}"
    assert "/sites/db/" not in (db["url"] or ""), f"a container is not served from /sites: {db}"
    # A static site IS live as soon as the generation is built — boxd serves it.
    assert blog["state"] == "active", f"static site should be active: {blog}"
    box.succeed("curl -sf http://127.0.0.1:2693/sites/blog/ | grep hi")

    # --- the OS-tier repo must be renderable and correct --------------------
    # This is what the root apply builds. `port = 0` here is the bug that made
    # nginx proxy to 127.0.0.1:0 and gave databases no password.
    box.succeed("boxd host-gen --host-id box --out /tmp/osrepo")
    module = box.succeed("cat /tmp/osrepo/nodes/hosts/box/services/db.nix")
    print(module)
    assert "port = 0;" not in module, f"OS-tier module has no real port: {module}"
    assert 'image = "postgres:16";' in module, module

    # --- the root apply unit exists and is what boxd asks systemd to start ---
    unit = box.succeed("systemctl cat boxd-os-apply.service")
    print(unit)
    assert "os-apply" in unit, unit
    # Neither this unit nor the channel updater may be restarted by the switch
    # they are performing — that kills the updater before it can health-check
    # and roll back.
    for u in ["boxd-os-apply.service", "boxd-channel-update.service"]:
        text = box.succeed(f"systemctl cat {u}")
        assert "X-RestartIfChanged=false" in text, f"{u} must not restart on switch:\n{text}"

    # --- one copy of the content, served by the real web server -------------
    # nginx serves a static site out of the CURRENT generation, through boxd's
    # profile symlink, so the two planes cannot serve different bytes and a
    # content-only change does not need a system rebuild to appear. (nginx is
    # configured here by hand, exactly as the OS tier would after an apply.)
    box.succeed("systemctl restart nginx")
    box.wait_for_open_port(80)
    box.wait_until_succeeds("curl -sf -H 'Host: blog.example.com' http://127.0.0.1/ | grep hi", timeout=60)

    # Change only the content. This stays on boxd's fast path — no system
    # rebuild, no nginx reload — and the public plane must still be current.
    api("POST", "/api/v1/services",
        '{"name":"blog","template":"static-site","params":{"index_html":"<h1>edited</h1>"}}')
    box.wait_until_succeeds(
        "curl -sf -H 'Host: blog.example.com' http://127.0.0.1/ | grep edited", timeout=60
    )
    box.fail("curl -sf -H 'Host: blog.example.com' http://127.0.0.1/ | grep -w hi")

    # And a rollback is just as immediate on the public plane.
    box.succeed("boxd rollback 2 >&2")
    box.wait_until_succeeds(
        "curl -sf -H 'Host: blog.example.com' http://127.0.0.1/ | grep -w hi", timeout=60
    )

    print("deploy through boxd: builds, reports honestly, renders a correct OS module,")
    print("and nginx serves the live generation (one copy, both planes)")
  '';
}
