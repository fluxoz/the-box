# Runtime proof of the container template: a real OCI image runs via podman,
# nginx routes its domain to it, and the container's port is not reachable from
# the LAN. The image is built locally (dockerTools) so the sandboxed test needs
# no registry pull.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};
  serverPy = pkgs.writeText "server.py" ''
    from http.server import BaseHTTPRequestHandler, HTTPServer
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200); self.end_headers()
            self.wfile.write(b"hello from the container")
        def log_message(self, *a): pass
    HTTPServer(("0.0.0.0", 80), H).serve_forever()
  '';
  appImage = pkgs.dockerTools.buildLayeredImage {
    name = "demo-app";
    tag = "latest";
    contents = [ pkgs.python3 ];
    config = {
      Cmd = [ "${pkgs.python3}/bin/python3" "${serverPy}" ];
      ExposedPorts = { "80/tcp" = { }; };
    };
  };
in
pkgs.testers.runNixOSTest {
  name = "box-container";

  nodes = {
    box = { ... }: {
      imports = [ self.nixosModules.platform ];
      networking.hostName = "box";
      networking.networkmanager.enable = lib.mkForce false;
      virtualisation.memorySize = 2048;
      virtualisation.diskSize = 4096;
      services.the-box.containers.demo = {
        image = "demo-app:latest";
        imageFile = appImage; # loaded into podman, no pull
        port = 8000;
        containerPort = 80;
        domain = null; # default vhost
      };
    };
    client = { ... }: {
      networking.networkmanager.enable = lib.mkForce false;
    };
  };

  testScript = ''
    start_all()
    box.wait_for_unit("podman-demo.service", timeout=180)
    box.wait_for_unit("nginx.service")
    box.wait_for_open_port(80)

    # nginx reverse-proxies the domain to the container on its loopback port.
    box.wait_until_succeeds(
        "curl -sf http://localhost/ | grep -i 'hello from the container'", timeout=180
    )

    # From another machine: nginx (80) is reachable; the container's mapped port
    # (8000) is loopback-only and firewalled off.
    client.wait_for_unit("multi-user.target")
    client.wait_until_succeeds(
        "curl -sf http://box/ | grep -i 'hello from the container'", timeout=60
    )
    client.fail("curl -s -m 5 http://box:8000/")

    print("container runs via podman, nginx proxies it, port closed to the LAN")
  '';
}
