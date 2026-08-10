# Live proof of the installer LAN beacon: a machine sitting in the pre-install
# wizard advertises itself over mDNS as an unclaimed Box installer, and an agent
# on the same segment discovers it, reads its public identity without a PIN, and
# is held out of the destructive endpoints until it presents the setup PIN — the
# LAN-native analog of SSH for bare metal (no screen, no keyboard on the target).
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};
  boxd = self.packages.${system}.boxd;
  avahi = pkgs.avahi;
in
pkgs.testers.runNixOSTest {
  name = "box-installer-beacon";

  nodes = {
    # Stands in for a machine booted into the installer's blank-boot path: avahi
    # publishing enabled (userServices lets the beacon register), the wizard on
    # 2693, and the mDNS advert — exactly what run_wizards does in the image.
    machine = { ... }: {
      networking.hostName = "box-installer";
      networking.networkmanager.enable = lib.mkForce false;
      networking.firewall.allowedTCPPorts = [ 2693 ];
      services.avahi = {
        enable = true;
        publish = {
          enable = true;
          addresses = true;
          userServices = true;
        };
      };
    };

    # The operator's agent: browses mDNS and reads the installer's identity.
    agent = { ... }: {
      networking.networkmanager.enable = lib.mkForce false;
      services.avahi = {
        enable = true;
        nssmdns4 = true;
      };
      environment.systemPackages = [ avahi pkgs.curl ];
    };
  };

  testScript = ''
    start_all()
    for m in (machine, agent):
        m.wait_for_unit("avahi-daemon.service")

    # Bring up the pre-install wizard with a setup PIN (as the installer does),
    # then beacon it on the LAN — both as transient units so they persist past
    # the launching shell.
    machine.succeed("mkdir -p /tmp/w")
    machine.succeed(
        "systemd-run --unit=box-wizard ${boxd}/bin/boxd install-wizard "
        "--listen 0.0.0.0:2693 --orders-out /tmp/w/orders.json "
        "--disko-out /tmp/w/disko.nix --commit-flag /tmp/w/commit "
        "--progress /tmp/w/prog --done /tmp/w/done --pin 424242"
    )
    machine.wait_until_succeeds("curl -sf http://localhost:2693/api/hello", timeout=60)
    machine.succeed(
        "systemd-run --unit=box-beacon ${avahi}/bin/avahi-publish-service "
        "'box-setup-test · The Box' _thebox-setup._tcp 2693 "
        "vendor=thebox role=installer state=unclaimed pin=required"
    )

    # The agent discovers the unclaimed installer over mDNS and resolves its
    # address straight from the browse record (no name-resolution dependency).
    agent.wait_until_succeeds(
        "avahi-browse -rpt _thebox-setup._tcp | grep -qi box-setup", timeout=90
    )
    addr = agent.succeed(
        "avahi-browse -rpt _thebox-setup._tcp | grep '^=' | grep IPv4 | grep -i box-setup "
        "| head -1 | cut -d';' -f8"
    ).strip()
    print(f"agent discovered the installer at {addr}")

    # Public identity (no PIN): it's a Box installer, unclaimed, PIN required.
    agent.succeed(f"curl -sf http://{addr}:2693/api/hello | grep -i '\"thebox\":\"installer\"'")
    agent.succeed(f"curl -sf http://{addr}:2693/api/hello | grep -i unclaimed")
    agent.succeed(f"curl -sf http://{addr}:2693/api/hello | grep -i '\"pin_required\":true'")

    # The destructive endpoints stay behind the PIN.
    code = agent.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' http://{addr}:2693/api/probe"
    ).strip()
    assert code == "403", f"probe without PIN should be 403, got {code}"
    agent.succeed(f"curl -sf -H 'x-setup-pin: 424242' http://{addr}:2693/api/probe >/dev/null")

    print("agent discovered an unclaimed installer over mDNS, read its identity, PIN gate intact")
  '';
}
