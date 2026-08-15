# Live proof of Box Connect: Boxes join a Headscale-coordinated WireGuard mesh
# and the tunnel actually CARRIES TRAFFIC — one Box reaches another's dashboard
# over the tailnet — plus boxd's tailnet fleet discovery finds the tag:box peer.
# This is the "VM-tier" item the unit tests can't reach: real tailscaled + real
# WireGuard + a real coordinator.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  # ACL so tag:box preauth keys are accepted (the fleet user owns the tag) and
  # peers can reach each other — the shaped-mesh shipped policy, minimised.
  acl = pkgs.writeText "acl.hujson" (builtins.toJSON {
    tagOwners."tag:box" = [ "fleet@" ];
    acls = [{ action = "accept"; src = [ "*" ]; dst = [ "*:*" ]; }];
  });

  box = hostName: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = hostName;
    networking.networkmanager.enable = lib.mkForce false;
    # tailscale's WireGuard + the test's routing get along better with loose rpf.
    networking.firewall.checkReversePath = "loose";
  };
in
pkgs.testers.runNixOSTest {
  name = "box-fleet-mesh";

  nodes.coordinator = { pkgs, ... }: {
    networking.hostName = "coordinator";
    services.headscale = {
      enable = true;
      address = "0.0.0.0";
      port = 8080;
      settings = {
        server_url = "http://coordinator:8080";
        # Embedded DERP so nodes can coordinate/relay even before a direct path.
        # urls=[] + no auto-update: DON'T fetch the public DERPMap from
        # controlplane.tailscale.com (no internet in the test → headscale would
        # crash-loop). The embedded DERP (region 999) is the only relay.
        derp = {
          urls = [ ];
          auto_update_enabled = false;
          server = {
            enabled = true; # headscale's yaml key is `enabled`, not `enable`
            region_id = 999;
            region_code = "test";
            region_name = "test";
            stun_listen_addr = "0.0.0.0:3478";
            automatically_add_embedded_derp_region = true; # -> non-empty DERPMap
          };
        };
        # Required by headscale's config assertions; unused here — the Boxes join
        # with `tailscale up --accept-dns=false`, so tailnet DNS never applies.
        dns = {
          magic_dns = false;
          base_domain = "box.test";
          nameservers.global = [ "9.9.9.9" ];
        };
        policy.path = toString acl;
      };
    };
    networking.firewall.allowedTCPPorts = [ 8080 ];
    networking.firewall.allowedUDPPorts = [ 3478 ];
    environment.systemPackages = [ pkgs.headscale ];
  };

  nodes.boxA = box "box-a";
  nodes.boxB = box "box-b";

  testScript = ''
    import json

    start_all()
    coordinator.wait_for_unit("headscale.service")
    # Wait until the gRPC socket actually serves (idempotent), then create once.
    coordinator.wait_until_succeeds("headscale users list", timeout=120)
    coordinator.succeed("headscale users create fleet")

    users = json.loads(coordinator.succeed("headscale users list -o json"))
    users = users["users"] if isinstance(users, dict) else users
    fleet_id = str([u for u in users if u["name"] == "fleet"][0]["id"])

    key = json.loads(coordinator.succeed(
        f"headscale preauthkeys create --user {fleet_id} --reusable --expiration 24h --tags tag:box -o json"
    ))["key"]

    # Every Box joins the mesh with the tagged key (as `boxd connect enroll` does).
    for m, h in ((boxA, "box-a"), (boxB, "box-b")):
        m.wait_for_unit("tailscaled.service")
        m.succeed(
            f"tailscale up --login-server http://coordinator:8080 --authkey {key} "
            f"--hostname {h} --accept-dns=false"
        )

    # The tailnet forms: each Box sees the other as a peer.
    boxA.wait_until_succeeds("tailscale status | grep -i box-b", timeout=120)
    boxB.wait_until_succeeds("tailscale status | grep -i box-a", timeout=120)

    ip_b = boxB.succeed("tailscale ip -4").strip()

    # THE PROOF: box-a reaches box-b's dashboard over the WireGuard tunnel (its
    # tailnet IP, not the LAN) — Box Connect carries real traffic.
    boxA.wait_until_succeeds(f"curl -sf http://{ip_b}:2693/api/v1/health | grep -i box-b", timeout=120)

    # And boxd's tailnet discovery plane finds the tag:box peer over the mesh.
    # (/fleet is an operator surface — authenticate like one; a bare curl gets
    # the 401 signpost, which is correct and not a discovery failure.)
    token_a = boxA.succeed("boxd auth mint --label test 2>/dev/null | tail -1").strip()
    boxA.wait_until_succeeds(
        f"curl -sf -H 'Authorization: Bearer {token_a}' http://localhost:2693/api/v1/fleet | grep -i box-b",
        timeout=120,
    )

    print("Box Connect: WireGuard tunnel carries traffic across the mesh + tailnet fleet discovery")
  '';
}
