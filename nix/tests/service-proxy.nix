# Runtime proof of the reverse-proxied-app model: a service runs on a loopback
# port, nginx routes its domain to it, and the app's port is NOT reachable from
# the LAN — only nginx (80) is. This is the firewall/proxy contract the port
# model promises, exercised on a booted system.
{ self, nixpkgs, system }:
let
  lib = nixpkgs.lib;
  pkgs = nixpkgs.legacyPackages.${system};
  # A tiny app that binds 127.0.0.1:$PORT (the platform sets PORT) and answers.
  serverPy = pkgs.writeText "app.py" ''
    import os
    from http.server import BaseHTTPRequestHandler, HTTPServer
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            self.send_response(200); self.end_headers()
            self.wfile.write(b"hello from the box app")
        def log_message(self, *a): pass
    HTTPServer(("127.0.0.1", int(os.environ["PORT"])), H).serve_forever()
  '';
in
pkgs.testers.runNixOSTest {
  name = "box-service-proxy";

  nodes = {
    # A Box running one reverse-proxied app on port 8000, as boxd's generated
    # module would configure it.
    box = { ... }: {
      imports = [ self.nixosModules.platform ];
      networking.hostName = "box";
      networking.networkmanager.enable = lib.mkForce false;
      services.the-box.apps.demo = {
        command = "${pkgs.python3}/bin/python3 ${serverPy}";
        port = 8000;
        domain = null; # default vhost
      };
    };
    # Another machine on the same segment, to test what the LAN can reach.
    client = { ... }: {
      networking.networkmanager.enable = lib.mkForce false;
    };
  };

  testScript = ''
    start_all()
    box.wait_for_unit("box-app-demo.service")
    box.wait_for_unit("nginx.service")
    box.wait_for_open_port(80)

    # nginx reverse-proxies (the default vhost) to the app on loopback.
    box.wait_until_succeeds(
        "curl -sf http://localhost/ | grep -i 'hello from the box app'", timeout=60
    )

    # The app binds loopback only, never the LAN interface.
    box.succeed("ss -ltn | grep 127.0.0.1:8000")
    box.fail("ss -ltn | grep -E '0.0.0.0:8000|\\*:8000'")

    # From another machine: nginx (80) is reachable; the app's port (8000) is
    # firewalled off — proxied services never expose their port.
    client.wait_for_unit("multi-user.target")
    client.wait_until_succeeds(
        "curl -sf http://box/ | grep -i 'hello from the box app'", timeout=60
    )
    client.fail("curl -s -m 5 http://box:8000/")

    print("app on loopback, nginx proxies it, app port closed to the LAN")
  '';
}
