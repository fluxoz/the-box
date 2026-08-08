{ config, lib, pkgs, ... }:
let
  cfg = config.services.the-box;
in
{
  options.services.the-box = {
    enable = lib.mkEnableOption "The Box daemon (boxd)";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The boxd package to run.";
    };

    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:2693";
      description = "Listen address for the local dashboard, JSON API and site router.";
    };

    dataDir = lib.mkOption {
      type = lib.types.path;
      default = "/var/lib/boxd";
      description = "State directory holding the declarative config, sources and generation profiles.";
    };

    sites = lib.mkOption {
      default = { };
      description = ''
        Static sites served at the OS level. This is where box.toml compiles
        to: each static-site service becomes one entry, and the platform turns
        it into a real nginx virtual host — proving a generated module composes
        all the way into a running system service. Attribute name is the
        service name.
      '';
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          root = lib.mkOption {
            type = lib.types.path;
            description = "Directory of files to serve for this site.";
          };
          domain = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Public hostname; when null the site is the default vhost.";
          };
        };
      });
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      users.users.boxd = {
        isSystemUser = true;
        group = "boxd";
      };
      users.groups.boxd = { };

      systemd.services.boxd = {
        description = "The Box daemon";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];
        # boxd shells out to nix to build generations and to cloudflared for
        # BYO tunnel exposure.
        path = [ pkgs.nix pkgs.cloudflared ];
        # nix (invoked by boxd for generation builds) needs a writable cache
        # under $HOME; the boxd system user's default home is /var/empty.
        environment.HOME = cfg.dataDir;
        serviceConfig = {
          ExecStart = "${lib.getExe cfg.package} --data-dir ${cfg.dataDir} serve --listen ${cfg.listen}";
          User = "boxd";
          Group = "boxd";
          StateDirectory = "boxd";
          Restart = "on-failure";
          RestartSec = 2;
        };
      };

      # A machine-readable record of what the OS tier declares, for boxd/GUI
      # introspection of the composed layer stack.
      environment.etc."box/sites.json".text = builtins.toJSON (
        lib.mapAttrs (_: site: { inherit (site) domain; root = "${site.root}"; }) cfg.sites
      );
    }

    (lib.mkIf (cfg.sites != { }) {
      services.nginx = {
        enable = true;
        virtualHosts = lib.mapAttrs (name: site: {
          serverName = if site.domain != null then site.domain else name;
          default = site.domain == null;
          root = site.root;
          locations."/".index = "index.html";
        }) cfg.sites;
      };
      networking.firewall.allowedTCPPorts = [ 80 ];
    })
  ]);
}
