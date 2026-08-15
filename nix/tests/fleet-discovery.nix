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

    # The fleet view is an OPERATOR surface — it went behind auth when the
    # API hardened, and this test kept curling it bare, which is how the Tests
    # workflow sat red for weeks reading like a discovery failure while every
    # /fleet response was actually the 401 pairing signpost. Authenticate the
    # way a real operator does.
    tokenA = boxA.succeed("boxd auth mint --label test 2>/dev/null | tail -1").strip()
    tokenB = boxB.succeed("boxd auth mint --label test 2>/dev/null | tail -1").strip()

    # Diagnostics stay: a future failure should name the broken link.
    print("boxd browse:", boxA.succeed("runuser -u boxd -- avahi-browse -rptl _thebox._tcp | head -5 || true"))
    print("fleet raw:", boxA.succeed(
        f"curl -s -H 'Authorization: Bearer {tokenA}' http://localhost:2693/api/v1/fleet | head -c 600 || true"))

    # boxd's own fleet endpoint discovers the peer and reads its coarse health.
    boxA.wait_until_succeeds(
        f"curl -sf -H 'Authorization: Bearer {tokenA}' http://localhost:2693/api/v1/fleet | grep -i box-b",
        timeout=90,
    )
    boxB.wait_until_succeeds(
        f"curl -sf -H 'Authorization: Bearer {tokenB}' http://localhost:2693/api/v1/fleet | grep -i box-a",
        timeout=90,
    )

    # And the peer's coarse health is directly readable (public, no auth).
    boxA.succeed("curl -sf http://box-b.local:2693/api/v1/health | grep -i box-b")

    print("two Boxes discovered each other over mDNS and read coarse health")
  '';
}
