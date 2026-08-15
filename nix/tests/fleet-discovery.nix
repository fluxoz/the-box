# Live proof that two Boxes on a shared network discover each other over mDNS
# and can read each other's coarse health — the critical-path de-risk before
# scaling to many VMs. Two real Box nodes (the platform: boxd + avahi + the
# _thebox._tcp advert) on one L2 segment; we assert each browses the other and
# that boxd's own /api/v1/fleet lists the peer.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  # A Box: the real platform, with NetworkManager dropped so the test net (and
  # avahi on it) comes up deterministically. avahi itself is what we're testing.
  box = hostName: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = hostName;
    networking.networkmanager.enable = lib.mkForce false;
  };
in
pkgs.testers.runNixOSTest {
  name = "box-fleet-discovery";

  nodes.boxA = box "box-a";
  nodes.boxB = box "box-b";

  testScript = ''
    start_all()

    for m in (boxA, boxB):
        m.wait_for_unit("avahi-daemon.service")
        m.wait_for_unit("boxd.service")
        m.wait_until_succeeds("curl -sf http://localhost:2693/api/v1/health", timeout=90)

    # Each Box browses the LAN and finds the *other* over mDNS (-l ignores its
    # own advert, exactly as boxd's discover() does).
    boxA.wait_until_succeeds("avahi-browse -rptl _thebox._tcp | grep -i box-b", timeout=90)
    boxB.wait_until_succeeds("avahi-browse -rptl _thebox._tcp | grep -i box-a", timeout=90)

    # Diagnostics first: what the browse looks like as root, as the boxd user,
    # and what /fleet actually returns — so a failure here names the broken
    # link instead of just timing out.
    print("root browse:", boxA.succeed("avahi-browse -rptl _thebox._tcp | head -5 || true"))
    print("boxd browse:", boxA.succeed("runuser -u boxd -- avahi-browse -rptl _thebox._tcp | head -5 || true"))
    print("fleet raw:", boxA.succeed("curl -s http://localhost:2693/api/v1/fleet | head -c 600 || true"))

    # boxd's own fleet endpoint (loopback-trusted) discovers the peer and reads
    # its coarse health.
    boxA.wait_until_succeeds(
        "curl -sf http://localhost:2693/api/v1/fleet | grep -i box-b", timeout=90
    )
    boxB.wait_until_succeeds(
        "curl -sf http://localhost:2693/api/v1/fleet | grep -i box-a", timeout=90
    )

    # And the peer's coarse health is directly readable (public, no auth).
    boxA.succeed("curl -sf http://box-b.local:2693/api/v1/health | grep -i box-b")

    print("two Boxes discovered each other over mDNS and read coarse health")
  '';
}
