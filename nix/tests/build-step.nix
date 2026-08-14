# The sandboxed build step on a real Box OS: a repository that is not a file
# tree until a build runs, built by rootless podman AS THE BOXD SYSTEM USER,
# from the builder image shipped in the closure. This is the part no cargo
# test can see, and it is exactly the part PLAN §1 says fails silently when it
# fails: subuid ranges, the newuidmap wrappers, XDG_RUNTIME_DIR for a unit
# with no session, cgroup delegation, and podman finding all of it from
# boxd's environment rather than a login shell's.
#
# The VM has no network, which is a feature twice over: it proves the image
# loads from the closure (no registry), and the build phase is defined to run
# with --network=none anyway. The install phase runs a command that needs no
# registry, so the whole loop is provable offline.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};
in
pkgs.testers.runNixOSTest {
  name = "box-build-step";

  nodes.box = { ... }: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = "box";
    networking.networkmanager.enable = lib.mkForce false;
    services.avahi.enable = lib.mkForce false;
    boot.loader.grub.enable = lib.mkForce false;
    virtualisation.memorySize = 3072;
    virtualisation.diskSize = 8192;

    # The test script prepares the upstream repo from a root shell; git is on
    # boxd's unit PATH but not the system's.
    environment.systemPackages = [ pkgs.git ];

    # Generations pin to the platform's nixpkgs; a runCommand's inputs must be
    # in the VM's store or the build tries the (absent) network.
    system.extraDependencies = [
      nixpkgs
      pkgs.stdenvNoCC
      (pkgs.runCommand "box-generation-inputs" { } "mkdir $out").inputDerivation
    ];
  };

  testScript = ''
    import json

    box.wait_for_unit("multi-user.target")
    box.wait_until_succeeds("systemctl is-active boxd", timeout=90)
    box.wait_for_open_port(2693)

    # An upstream repository the boxd user can fetch over file:// — owned by
    # boxd, because git refuses to serve a repo owned by someone else. Its
    # site does not exist until a build writes it.
    box.succeed(
        "mkdir -p /tmp/up && "
        "git -C /tmp/up init -q -b main && "
        "echo '<h1>built by the box</h1>' > /tmp/up/page.src && "
        "echo keep > /tmp/up/README && "
        "git -C /tmp/up add -A && "
        "git -C /tmp/up -c user.email=t@t -c user.name=t commit -qm one && "
        "chown -R boxd:boxd /tmp/up"
    )

    token = box.succeed("boxd auth mint --label test 2>/dev/null | tail -1").strip()

    def mcp(tool, arguments):
        body = json.dumps({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        out = box.succeed(
            "curl -sf -X POST -H 'Authorization: Bearer %s' "
            "-H 'Content-Type: application/json' -d '%s' http://127.0.0.1:2693/mcp"
            % (token, body)
        )
        return json.loads(out)

    # The service exists first (any deploy creates it); the repo link with its
    # build step is then written the way a restored config arrives — directly
    # in box.toml. (link_repo needs a connected forge; this VM has no network.)
    r = mcp("deploy_static_site", {"name": "site", "index_html": "placeholder"})
    assert r["result"]["isError"] == False, r

    # The build command uses node from the shipped image — proving the image
    # is real, not just present — and depends on the install phase's output,
    # proving the phases run in order.
    box.succeed("""cat >> /var/lib/boxd/box.toml <<'EOF'

    [services.repo]
    forge = "github"
    repo = "local/site"
    clone_url = "file:///tmp/up"
    branch = "main"

    [services.repo.build]
    command = "node -e \\"const fs=require('fs');fs.mkdirSync('dist',{recursive:true});fs.writeFileSync('dist/index.html',fs.readFileSync('staged.src'))\\""
    install = "cp page.src staged.src"
    output_dir = "dist"
    EOF""")

    r = mcp("sync_repo", {"name": "site"})
    print(json.dumps(r, indent=2))
    assert r["result"]["isError"] == False, r
    assert "deployed" in r["result"]["content"][0]["text"], r

    # The built site is what serves — the sandbox ran node, wrote dist/, and
    # the deploy published it.
    box.succeed("curl -sf http://127.0.0.1:2693/sites/site/ | grep 'built by the box'")

    # The image came from the closure, loaded into the BOXD user's rootless
    # storage — not root's, and not a registry.
    images = box.succeed(
        "runuser -u boxd -- env HOME=/var/lib/boxd XDG_RUNTIME_DIR=/run/boxd "
        "podman --cgroup-manager=cgroupfs --events-backend=file images"
    )
    assert "box-builder" in images, images
    box.fail("podman images | grep box-builder")  # root's storage stays empty

    # The build log exists and tells the story of both phases.
    buildlog = box.succeed("cat /var/lib/boxd/repos/site.build-log")
    assert "install:" in buildlog and "build:" in buildlog, buildlog

    # A commit that breaks the build: the sync fails with the log's tail, and
    # the site keeps serving what last built.
    # (safe.directory: the repo now belongs to boxd, and root's git refuses a
    # repo owned by someone else.)
    box.succeed(
        "git -c safe.directory=/tmp/up -C /tmp/up rm -q page.src && "
        "git -c safe.directory=/tmp/up -C /tmp/up -c user.email=t@t -c user.name=t commit -qm break && "
        "chown -R boxd:boxd /tmp/up"
    )
    r = mcp("sync_repo", {"name": "site"})
    assert r["result"]["isError"] == True, r
    assert "build log" in r["result"]["content"][0]["text"], r
    box.succeed("curl -sf http://127.0.0.1:2693/sites/site/ | grep 'built by the box'")
  '';
}
