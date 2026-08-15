# Runtime proof of the reverse-proxied-app model: a service runs on a loopback
# port, nginx routes its domain to it, and the app's port is NOT reachable from
# the LAN — only nginx (80) is. This is the firewall/proxy contract the port
# model promises, exercised on a booted system.
#
# It also carries the duplicate-default regression: several services with no
# domain at once. Every domain-less vhost used to claim `default_server`, and
# nginx treats two claimants as a fatal configuration error — the second
# domain-less site deployed onto a real Box took every site on it offline,
# and the switch reported success. Exactly one deterministic winner now; this
# test is the shape that machine was in.
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
    # A Box with THREE domain-less claimants (two sites and an app is the
    # nginx-fatal shape), plus a domain'd app for the proxy contract.
    box = { ... }: {
      imports = [ self.nixosModules.platform ];
      networking.hostName = "box";
      networking.networkmanager.enable = lib.mkForce false;
      services.the-box.sites.alpha = {
        root = pkgs.writeTextDir "index.html" "alpha is the default";
        domain = null;
      };
      services.the-box.sites.zulu = {
        root = pkgs.writeTextDir "index.html" "zulu answers to its name";
        domain = null;
      };
      services.the-box.apps.demo = {
        command = "${pkgs.python3}/bin/python3 ${serverPy}";
        port = 8000;
        domain = "demo.lan";
      };
      services.the-box.apps.bare = {
        command = "${pkgs.python3}/bin/python3 ${serverPy}";
        port = 8001;
        domain = null; # a third domain-less claimant
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
    # The regression: with several domain-less services, nginx used to refuse
    # its own config ("a duplicate default server") and never come up at all.
    box.wait_for_unit("nginx.service")
    box.wait_for_open_port(80)

    # Exactly one deterministic default: the first domain-less site by name.
    box.wait_until_succeeds(
        "curl -sf http://localhost/ | grep 'alpha is the default'", timeout=60
    )
    # The others are still there, by their names.
    box.succeed("curl -sf -H 'Host: zulu' http://localhost/ | grep 'zulu answers'")
    box.succeed("curl -sf -H 'Host: demo.lan' http://localhost/ | grep -i 'hello from the box app'")

    # Static sites carry Cache-Control, so the tunnel's edge can act as a CDN:
    # pages revalidate fast, hashed assets are immutable. Apps set their own.
    box.succeed("curl -sfI http://localhost/ | grep -i 'cache-control: public, max-age=60'")
    box.fail("curl -sfI -H 'Host: demo.lan' http://localhost/ | grep -i 'max-age=60'")

    # The apps bind loopback only, never the LAN interface.
    box.succeed("ss -ltn | grep 127.0.0.1:8000")
    box.fail("ss -ltn | grep -E '0.0.0.0:8000|\\*:8000'")

    # From another machine: nginx (80) is reachable; the app's port (8000) is
    # firewalled off — proxied services never expose their port.
    client.wait_for_unit("multi-user.target")
    client.wait_until_succeeds(
        "curl -sf -H 'Host: demo.lan' http://box/ | grep -i 'hello from the box app'", timeout=60
    )
    client.fail("curl -s -m 5 http://box:8000/")

    print("one default among many domain-less services, nginx alive, ports closed")
  '';
}
