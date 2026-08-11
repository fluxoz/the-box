# Live proof of destroy-and-recreate across two real Boxes.
#
# box1 has config + content + a secret encrypted to its own SSH host key, and
# pushes the lot (config + encrypted .age, never plaintext) to a git repo on a
# third node. box2 — a *fresh* Box with a different SSH host key — runs the real
# `boxd restore`: it clones the repo and re-keys every secret from the operator's
# key to its own host key. Afterwards box2 decrypts the secret with the exact
# command agenix runs at boot (`age -d -i /etc/ssh/ssh_host_ed25519_key`), the
# operator still can, and the restored config + content are present.
#
# This is the part the Rust integration test can't reach: that the re-key
# handoff works with genuine SSH host keys, over a genuine network git remote,
# and that the result is decryptable by the box's own boot-time identity.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  # A fixed operator identity (an SSH key, as a person or agent would use). Its
  # public half authorizes both boxes as an age recipient; its private half is
  # what box2 uses to re-key the restored secrets.
  operator = pkgs.runCommand "operator-key" { nativeBuildInputs = [ pkgs.openssh ]; } ''
    mkdir -p $out
    ssh-keygen -t ed25519 -N "" -C operator -f $out/key >/dev/null
  '';

  # A Box: the real platform, with the operator authorized as a recipient. NM /
  # avahi dropped for a fast, deterministic boot.
  box = hostName: { config, ... }: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = hostName;
    networking.networkmanager.enable = lib.mkForce false;
    services.avahi.enable = lib.mkForce false;
    boot.loader.grub.enable = lib.mkForce false;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    environment.etc."box/authorized_keys".source = "${operator}/key.pub";
    environment.systemPackages = [ config.services.the-box.package pkgs.age pkgs.git pkgs.openssh ];
  };
in
pkgs.testers.runNixOSTest {
  name = "box-restore";

  nodes.box1 = box "box1";
  nodes.box2 = box "box2";

  # The user's git host (stands in for GitHub): a bare repo served over the git
  # protocol with push enabled. Not a Box; just git.
  nodes.hub = { pkgs, ... }: {
    networking.firewall.enable = false;
    environment.systemPackages = [ pkgs.git ];
  };

  testScript = ''
    start_all()
    for m in (box1, box2, hub):
        m.wait_for_unit("multi-user.target")

    # --- hub: a bare repo, served with receive-pack (push) enabled. ---
    hub.succeed("git init -q --bare /srv/repo.git")
    hub.succeed("git -C /srv/repo.git config --bool daemon.receivepack true")
    hub.succeed(
        "systemd-run --unit=gitd git daemon --reuseaddr --base-path=/srv "
        "--export-all --enable=receive-pack /srv"
    )
    hub.wait_for_open_port(9418)

    # Both boxes have generated their SSH host keys (agenix's identity).
    for m in (box1, box2):
        m.wait_until_succeeds("test -f /etc/ssh/ssh_host_ed25519_key.pub", timeout=60)

    # --- box1: real config + content + a secret keyed to its own host key. ---
    box1.succeed("install -d -m 700 /var/lib/boxd/secrets")
    box1.succeed("install -d -m 755 /var/lib/boxd/sources/site/www")
    box1.succeed("echo '<h1>alive</h1>' > /var/lib/boxd/sources/site/www/index.html")
    box1.succeed(
        "printf 'services = []\\n' > /var/lib/boxd/box.toml"
    )
    # Encrypt to [box1 host key + operator] — exactly what agecrypt::encrypt does.
    box1.succeed(
        "printf 'TOKEN=s3cr3t' | age "
        "-R /etc/ssh/ssh_host_ed25519_key.pub -R /etc/box/authorized_keys "
        "-o /var/lib/boxd/secrets/demo-env.age"
    )
    # The .age is ciphertext, not the plaintext token.
    box1.fail("grep -q s3cr3t /var/lib/boxd/secrets/demo-env.age")

    # An OPERATIONAL secret boxd generates itself: the restic backup password.
    # It's encrypted at rest (op/*.age) and must survive destroy-and-recreate,
    # or a recreated box can't read its own backups.
    backup_key = box1.succeed("boxd backup init").strip()
    box1.succeed("test -f /var/lib/boxd/secrets/op/backup-password.age")
    # Encrypted at rest — the plaintext key is not in the .age.
    box1.fail(f"grep -q {backup_key} /var/lib/boxd/secrets/op/backup-password.age")
    # The box identity that decrypts it stays on the box (never pushed).
    box1.succeed("test -f /var/lib/boxd/secrets/boxd-identity.key")

    # Push config + encrypted secrets to the user's repo, via real boxd.
    box1.succeed("boxd config remote git://hub/repo.git")
    box1.succeed("boxd config push")

    # The remote received the config and both .age secrets, and NOT the box
    # identity key (it must never leave the box).
    hub.succeed("git -C /srv/repo.git cat-file -e main:box.toml")
    hub.succeed("git -C /srv/repo.git cat-file -e main:secrets/demo-env.age")
    hub.succeed("git -C /srv/repo.git cat-file -e main:secrets/op/backup-password.age")
    hub.fail("git -C /srv/repo.git cat-file -e main:secrets/boxd-identity.key")

    # --- box2: a fresh box recreates itself from the repo. ---
    box2.succeed(
        "boxd --backend local restore git://hub/repo.git "
        "--identity ${operator}/key"
    )

    # Config and content came back.
    box2.succeed("test -f /var/lib/boxd/box.toml")
    box2.succeed("grep -q alive /var/lib/boxd/sources/site/www/index.html")

    # The crux: the restored secret was re-keyed to box2, so box2 decrypts it
    # with the *exact* command agenix runs at boot — proving the whole design
    # works with genuine SSH host keys.
    got = box2.succeed(
        "age -d -i /etc/ssh/ssh_host_ed25519_key /var/lib/boxd/secrets/demo-env.age"
    ).strip()
    assert got == "TOKEN=s3cr3t", f"box2 host-key decrypt returned {got!r}"

    # The operator remains a recipient (can still manage / migrate again).
    box2.succeed(
        "age -d -i ${operator}/key /var/lib/boxd/secrets/demo-env.age | grep -qx TOKEN=s3cr3t"
    )

    # The operational secret survived: box2 generated its OWN box identity, and
    # restore re-keyed the backup password to it — so box2 reads the very same
    # backup key its predecessor generated, and can therefore restore its
    # backups. This is the recreate-critical property.
    box2.succeed("test -f /var/lib/boxd/secrets/boxd-identity.key")
    got_key = box2.succeed("boxd secret print backup-password").strip()
    assert got_key == backup_key, (
        f"box2 backup key {got_key!r} != box1 {backup_key!r}"
    )

    print("destroy-and-recreate verified: config + content restored, the service "
          "secret re-keyed to box2's SSH host key, and the box-generated backup "
          "password recovered on box2 — nothing operational left in plaintext")
  '';
}
