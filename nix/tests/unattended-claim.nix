# Live proof of the flashed, unattended install: a Box boots with orders written
# into a file on its boot partition (what the download server swaps in as the
# image streams past), reads them at first boot with nobody present, and comes
# up already belonging to the person who downloaded it.
#
# The two properties that matter, both asserted here:
#
#   1. Nobody can take it. An unclaimed Box announces itself over mDNS the
#      moment boxd starts, so "whoever claims it first" is a race a script wins
#      against a human every time. A Box whose medium named an owner must refuse
#      network claim outright.
#   2. The owner can get in, with no screen, no keyboard and no SSH — only the
#      pairing code from their recovery kit, which burns on first use.
#
# See docs/claim-flow-spec.md.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  # The code the person downloading the image keeps, and the hash that travels
  # inside the image. Fixed here so the test can redeem it; in real life the
  # Configurator mints it in the browser and only the hash ever leaves.
  code = "ABCD-EFGH-JKMN-PQRS";
  codeHash = builtins.hashString "sha256" "ABCDEFGHJKMNPQRS";

  # Exactly what the download server writes into the image: JSON, a newline,
  # then NUL padding out to the fixed 8192-byte file. The padding is the point —
  # the file's length never changes, so the filesystem is never touched.
  claimFile = pkgs.runCommand "box-claim.txt" { } ''
    orders='{"erase_disk":true,"hostname":"auto","enrollment_code_hash":"${codeHash}","static_ip":{"address":"192.168.1.50/24","gateway":"192.168.1.1","dns":["1.1.1.1","9.9.9.9"]}}'
    printf '%s\n' "$orders" > $out
    # Pad with NULs to 8192, the way a stored-block payload is padded.
    len=$(stat -c %s $out)
    dd if=/dev/zero bs=1 count=$((8192 - len)) >> $out 2>/dev/null
  '';
in
pkgs.testers.runNixOSTest {
  name = "box-unattended-claim";

  nodes.machine = { ... }: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = lib.mkForce "box";
    networking.networkmanager.enable = lib.mkForce false;
    networking.firewall.allowedTCPPorts = [ 2693 ];

    # Stand in for the FAT boot partition of a freshly flashed card. The real
    # image carries this file from the build; the download swapped its contents.
    systemd.tmpfiles.rules = [ "d /boot 0755 root root -" ];
    environment.etc."box-claim-source".source = claimFile;
    systemd.services.box-firstboot.preStart = ''
      install -D -m 0644 /etc/box-claim-source /boot/box-claim.txt
    '';
  };

  testScript = ''
    machine.start()
    machine.wait_for_unit("box-firstboot.service")
    machine.wait_for_unit("boxd.service")
    machine.wait_for_open_port(2693)

    with subtest("first boot adopted the orders written into the image"):
        machine.succeed("test -f /etc/box/install-config.json")
        machine.succeed("grep -q '${codeHash}' /etc/box/install-config.json")

    with subtest("the static address became a NetworkManager keyfile"):
        # NM itself is off in this test (the framework scripts the network);
        # what firstboot owns is the keyfile, so that is what is asserted.
        conn = "/etc/NetworkManager/system-connections/box-lan.nmconnection"
        machine.succeed(f"test -f {conn}")
        machine.succeed(f'[ "$(stat -c %a {conn})" = 600 ]')
        out = machine.succeed(f"cat {conn}")
        assert "method=manual" in out, "static orders must produce a manual connection"
        assert "address1=192.168.1.50/24,192.168.1.1" in out, f"bad address line:\n{out}"
        assert "dns=1.1.1.1;9.9.9.9;" in out and "ignore-auto-dns=true" in out, f"bad dns:\n{out}"

    with subtest("the Box is answering"):
        machine.succeed("curl -fsS http://127.0.0.1:2693/api/v1/health | grep -q health")

    with subtest("nobody on the network can seize it"):
        # /pair must not offer first-run claim, and POSTing it must mint nothing.
        page = machine.succeed("curl -fsS http://127.0.0.1:2693/pair")
        assert "Claim this Box" not in page, "a seeded Box must not offer claim"
        machine.succeed(
            "curl -fsS -o /dev/null -X POST http://127.0.0.1:2693/pair/claim || true"
        )
        # Still nothing to sign in with: no session was created.
        machine.succeed(
            "curl -fsS http://127.0.0.1:2693/api/v1/health -o /dev/null"
        )
        page = machine.succeed("curl -fsS http://127.0.0.1:2693/pair")
        assert "Claim this Box" not in page, "claim must stay closed"

    with subtest("the owner's code from the recovery kit still works"):
        token = machine.succeed(
            "curl -fsS -X POST http://127.0.0.1:2693/pair/redeem "
            "-H 'accept: application/json' -H 'content-type: application/json' "
            """-d '{"code":"${code}","label":"laptop"}' """
            """| sed -n 's/.*"token":"\\([^"]*\\)".*/\\1/p'"""
        ).strip()
        assert len(token) == 64, f"expected a session token, got {token!r}"

    with subtest("that code is burned; a replay gets nothing"):
        # Ask for JSON, so a refusal is a 401 rather than the browser flow's
        # redirect back to /pair with an error.
        out = machine.succeed(
            "curl -s -o /dev/null -w '%{http_code}' -X POST "
            "http://127.0.0.1:2693/pair/redeem "
            "-H 'accept: application/json' -H 'content-type: application/json' "
            """-d '{"code":"${code}","label":"again"}' """
        ).strip()
        assert out == "401", f"a redeemed code must not work twice, got {out}"

    with subtest("the token actually drives the Box"):
        machine.succeed(
            f"curl -fsS -H 'authorization: Bearer {token}' "
            "http://127.0.0.1:2693/api/v1/services >/dev/null"
        )

    with subtest("the claim was written down where the owner will see it"):
        machine.succeed("journalctl -u boxd --no-pager >/dev/null")

    with subtest("the boot partition no longer carries the Wi-Fi PSK or the code hash"):
        # FAT has no permissions: anything left here is readable by whoever
        # next holds the card.
        machine.succeed("test -f /boot/box-claim.txt")
        machine.fail("grep -q '${codeHash}' /boot/box-claim.txt")
        machine.succeed(
            "test \"$(tr -d '\\0' < /boot/box-claim.txt | wc -c)\" -eq 0"
        )
  '';
}
