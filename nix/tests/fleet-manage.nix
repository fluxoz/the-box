# Live proof of fleet write-authz: an operator (a separate device, not a Box)
# manages Boxes REMOTELY over the network, and management is gated by a session
# per Box — "management answers to you." Boxes trust the operator, never each
# other, so a session is scoped to one Box and revocation cuts access. Coarse
# health stays public (discovery != authorization).
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  # A real Box (platform: boxd on 0.0.0.0:2693, firewall opens it). NetworkManager
  # dropped so the test net comes up deterministically.
  box = hostName: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = hostName;
    networking.networkmanager.enable = lib.mkForce false;
  };
in
pkgs.testers.runNixOSTest {
  name = "box-fleet-manage";

  nodes.boxA = box "box-a";
  nodes.boxB = box "box-b";
  # The operator's device — not a Box, just something with curl.
  nodes.laptop = { pkgs, ... }: {
    networking.hostName = "laptop";
    environment.systemPackages = [ pkgs.curl ];
  };

  testScript = ''
    start_all()

    for m in (boxA, boxB):
        m.wait_for_unit("boxd.service")
        m.wait_until_succeeds("curl -sf http://localhost:2693/api/v1/health", timeout=120)
    laptop.wait_for_unit("multi-user.target")

    D = "--data-dir /var/lib/boxd"

    # 1. Coarse health is PUBLIC: the operator reads it on any Box, unauthenticated.
    laptop.wait_until_succeeds("curl -sf http://box-b:2693/api/v1/health | grep -i box-b", timeout=120)

    # 2. Management is GATED: no session -> 401. (All of /api/v1 beyond health goes
    #    through the same middleware, so this is the write-authz gate.)
    code = laptop.succeed("curl -s -o /dev/null -w '%{http_code}' http://box-b:2693/api/v1/status").strip()
    assert code == "401", f"expected 401 unauthenticated, got {code}"

    # 3. Operator pairs with box-b (recovery-kit / CLI path mints a session token).
    token = boxB.succeed(f"boxd {D} auth mint --label laptop").strip()
    assert len(token) >= 32, f"unexpected token: {token!r}"

    # 4. With the session, the operator manages box-b REMOTELY (over the LAN, not
    #    loopback) — the same protected endpoints now return 200, and real
    #    management data (the services list) is served.
    code = laptop.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -H 'Authorization: Bearer {token}' http://box-b:2693/api/v1/status"
    ).strip()
    assert code == "200", f"expected 200 authenticated, got {code}"
    laptop.succeed(f"curl -sf -H 'Authorization: Bearer {token}' http://box-b:2693/api/v1/services")

    # 5. Boxes trust YOU, not each other: box-b's token is worthless on box-a.
    code = laptop.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -H 'Authorization: Bearer {token}' http://box-a:2693/api/v1/status"
    ).strip()
    assert code == "401", f"expected box-a to reject box-b's token, got {code}"

    # 6. Revocation cuts management off: revoke the session, the token is dead.
    sid = boxB.succeed(f"boxd {D} auth list").strip().split("\t")[0]
    boxB.succeed(f"boxd {D} auth revoke {sid}")
    code = laptop.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' -H 'Authorization: Bearer {token}' http://box-b:2693/api/v1/status"
    ).strip()
    assert code == "401", f"expected 401 after revoke, got {code}"

    print("fleet write-authz: remote management gated per-Box, isolated, revocable")
  '';
}
