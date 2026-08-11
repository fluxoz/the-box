# Runtime proof of encrypted service secrets: an agenix-encrypted env file is
# decrypted at boot to /run/agenix (root, 0400) and fed to a container, which
# returns the secret. The plaintext value never exists outside the runtime
# tmpfs — not in config, not in the Nix store. The .age is built offline.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};

  # A fixed test age identity + a secret env file encrypted to it.
  secretEnv = pkgs.runCommand "test-secret-env" { nativeBuildInputs = [ pkgs.age ]; } ''
    mkdir -p $out
    age-keygen -o $out/key.txt 2>/dev/null
    pub=$(age-keygen -y $out/key.txt)
    printf 'SECRET_TOKEN=hunter2\n' | age -r "$pub" -o $out/env.age
  '';

  serverPy = pkgs.writeText "server.py" ''
    import os
    from http.server import BaseHTTPRequestHandler, HTTPServer
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200); self.end_headers()
            self.wfile.write(os.environ.get("SECRET_TOKEN", "MISSING").encode())
        def log_message(self, *a): pass
    HTTPServer(("0.0.0.0", 80), H).serve_forever()
  '';
  appImage = pkgs.dockerTools.buildLayeredImage {
    name = "secret-app";
    tag = "latest";
    contents = [ pkgs.python3 ];
    config = {
      Cmd = [ "${pkgs.python3}/bin/python3" "${serverPy}" ];
      ExposedPorts = { "80/tcp" = { }; };
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "box-secret-env";

  nodes.box = { ... }: {
    imports = [ self.nixosModules.platform ];
    networking.hostName = "box";
    networking.networkmanager.enable = lib.mkForce false;
    virtualisation.memorySize = 2048;
    virtualisation.diskSize = 4096;
    # Decrypt with the test identity (a real box uses its SSH host key).
    age.identityPaths = lib.mkForce [ "${secretEnv}/key.txt" ];
    services.the-box.containers.demo = {
      image = "secret-app:latest";
      imageFile = appImage;
      port = 8000;
      containerPort = 80;
      mode = "proxied";
      secretEnvFile = "${secretEnv}/env.age";
    };
  };

  testScript = ''
    start_all()
    box.wait_for_unit("podman-demo.service", timeout=180)
    box.wait_for_open_port(80)

    # The container returns the secret, so it received the env decrypted at
    # runtime — never from config.
    box.wait_until_succeeds("curl -sf http://localhost/ | grep -x hunter2", timeout=180)

    # The plaintext exists only on the runtime tmpfs, root-owned 0400.
    box.succeed("test \"$(stat -c %a /run/agenix/box-container-demo-env)\" = 400")
    box.succeed("grep -q 'SECRET_TOKEN=hunter2' /run/agenix/box-container-demo-env")

    print("container got an agenix-decrypted secret env; plaintext only in /run/agenix")
  '';
}
