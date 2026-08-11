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

    apps = lib.mkOption {
      default = { };
      description = ''
        Reverse-proxied apps: each reverse-proxied-app service becomes a systemd
        service running `command` on 127.0.0.1:`port`, with nginx routing its
        domain to it. The port is internal and the firewall stays closed; the
        platform assigns it (see boxd's port allocator). Attribute name is the
        service name.
      '';
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          command = lib.mkOption {
            type = lib.types.str;
            description = "Command that starts the app; it must listen on 127.0.0.1:$PORT.";
          };
          port = lib.mkOption {
            type = lib.types.port;
            description = "Internal loopback port the app listens on (platform-assigned).";
          };
          domain = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Public hostname; when null the app is the default vhost.";
          };
        };
      });
    };

    containers = lib.mkOption {
      default = { };
      description = ''
        OCI/Docker containers run via podman, reverse-proxied by domain. Each
        container service maps 127.0.0.1:`port` (platform-assigned) to the
        image's `containerPort`, and nginx routes its domain there. The port is
        internal and the firewall stays closed. Attribute name is the service
        name.
      '';
      type = lib.types.attrsOf (lib.types.submodule {
        options = {
          image = lib.mkOption {
            type = lib.types.str;
            description = "OCI image reference, e.g. nginx:1.27.";
          };
          imageFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = "Optional local image tarball to load instead of pulling (air-gapped/curated images).";
          };
          port = lib.mkOption {
            type = lib.types.port;
            description = "Host loopback port nginx proxies to (platform-assigned).";
          };
          containerPort = lib.mkOption {
            type = lib.types.port;
            default = 80;
            description = "Port the image listens on inside the container.";
          };
          mode = lib.mkOption {
            type = lib.types.enum [ "proxied" "internal" "exposed" ];
            default = "proxied";
            description = ''
              How the container is reached: "proxied" (nginx routes its domain,
              firewall closed), "internal" (loopback only, for a database other
              services use), or "exposed" (its port is opened on the firewall).
            '';
          };
          cmd = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Command/args to run in the container (overrides the image default).";
          };
          domain = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Public hostname; when null the container is the default vhost.";
          };
          environment = lib.mkOption {
            type = lib.types.attrsOf lib.types.str;
            default = { };
            description = "Environment variables passed to the container.";
          };
          volumes = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = ''Volume mounts, "host:container" (host side backed up when absolute).'';
          };
        };
      });
    };

    platform = {
      release = lib.mkOption {
        type = lib.types.str;
        default = "dev";
        description = ''
          Human-readable label for this platform release. It rides along in the
          closure, so a box can see which platform generation it is running and
          confirm a channel update actually took effect.
        '';
      };
      substituters = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = ''
          Extra binary caches the platform trusts. This is what lets a small
          box download the prebuilt platform closure on a channel update instead
          of compiling it. Empty until the real cache is stood up.
        '';
      };
      trustedPublicKeys = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Public keys corresponding to platform.substituters.";
      };
    };

    autoUpdate = {
      enable = lib.mkEnableOption "periodic platform channel updates (build, switch, health-checked rollback)";
      interval = lib.mkOption {
        type = lib.types.str;
        default = "daily";
        description = "systemd OnCalendar expression for the update check.";
      };
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
        # boxd shells out to nix (generation builds), cloudflared (BYO tunnel),
        # and avahi-browse + curl (LAN fleet discovery).
        path = [ pkgs.nix pkgs.cloudflared pkgs.avahi pkgs.curl pkgs.systemd ];
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

      # Scheduled backups: an hourly heartbeat that runs `backup run --if-due`,
      # which no-ops unless backup is enabled and the configured interval (in
      # box.toml) has elapsed — so cadence is controlled by config, not a rebuild.
      systemd.services.boxd-backup = {
        description = "The Box scheduled backup";
        after = [ "network-online.target" "boxd.service" ];
        wants = [ "network-online.target" ];
        path = [ pkgs.restic pkgs.openssh ];
        environment.HOME = cfg.dataDir;
        serviceConfig = {
          Type = "oneshot";
          User = "boxd";
          Group = "boxd";
          ExecStart = "${lib.getExe cfg.package} --data-dir ${cfg.dataDir} backup run --if-due";
        };
      };
      systemd.timers.boxd-backup = {
        description = "Hourly Box backup heartbeat (runs when a backup is due)";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = "hourly";
          Persistent = true;
          RandomizedDelaySec = "20m";
        };
      };

      # Platform channel update as a ROOT oneshot (switching the system profile
      # needs root; boxd runs unprivileged). `boxd channel update` checks the
      # channel and, on a new release, rebuilds + switches + health-checks +
      # rolls back if unhealthy. Always defined so the dashboard's "Update now"
      # can trigger it; the autoUpdate timer below is what makes it periodic.
      systemd.services.boxd-channel-update = {
        description = "The Box platform channel update";
        path = [ pkgs.nix pkgs.git ];
        environment.HOME = "/root";
        serviceConfig = {
          Type = "oneshot";
          User = "root";
          ExecStart = "${lib.getExe cfg.package} --data-dir ${cfg.dataDir} channel update";
        };
      };

      # Let the unprivileged boxd user START (only) that one update unit, so the
      # dashboard's "Update now" can trigger the root reconcile without granting
      # boxd broad control over the system.
      security.polkit.extraConfig = ''
        polkit.addRule(function(action, subject) {
          if (action.id == "org.freedesktop.systemd1.manage-units" &&
              action.lookup("unit") == "boxd-channel-update.service" &&
              subject.user == "boxd") {
            return polkit.Result.YES;
          }
        });
      '';

      # A machine-readable record of what the OS tier declares, for boxd/GUI
      # introspection of the composed layer stack.
      environment.etc."box/sites.json".text = builtins.toJSON (
        lib.mapAttrs (_: site: { inherit (site) domain; root = "${site.root}"; }) cfg.sites
      );

      # Which platform release this closure is: an observable marker so a
      # channel update can be confirmed to have landed.
      environment.etc."box/platform.json".text = builtins.toJSON {
        release = cfg.platform.release;
      };

      # The boxd CLI is on PATH and defaults to the box's data dir, so an
      # operator who SSHes in can just run `boxd auth enroll`, `boxd status`,
      # etc. — no wrapper, no --data-dir. (Auth writes chown to the data dir
      # owner so a code minted by root is readable by the boxd service.)
      # restic backs the `boxd backup` client-side-encrypted backups.
      environment.systemPackages = [ cfg.package pkgs.restic ];
      environment.variables.BOXD_DATA_DIR = "${cfg.dataDir}";

      # Advertise this Box on the LAN so peers can discover it (the fleet view).
      # Identity/health are read from /api/v1/health, so the record itself is a
      # static "a Box is here on this port" — no per-machine values baked in.
      services.avahi.extraServiceFiles.the-box = pkgs.writeText "the-box.service" ''
        <?xml version="1.0" standalone='no'?>
        <!DOCTYPE service-group SYSTEM "avahi-service.dtd">
        <service-group>
          <name replace-wildcards="yes">%h · The Box</name>
          <service>
            <type>_thebox._tcp</type>
            <port>${lib.last (lib.splitString ":" cfg.listen)}</port>
            <txt-record>vendor=thebox</txt-record>
          </service>
        </service-group>
      '';
    }

    (lib.mkIf (cfg.platform.substituters != [ ]) {
      # List options merge across modules, so these append to the defaults.
      nix.settings.substituters = cfg.platform.substituters;
      nix.settings.trusted-public-keys = cfg.platform.trustedPublicKeys;
    })

    (lib.mkIf cfg.autoUpdate.enable {
      # The self-reconcile timer: periodically run the (always-defined) update
      # service — check the channel, and on a new release rebuild, switch, and
      # roll back if it doesn't come up healthy.
      systemd.timers.boxd-channel-update = {
        description = "Periodic Box platform channel update";
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnCalendar = cfg.autoUpdate.interval;
          Persistent = true;
          RandomizedDelaySec = "1h";
        };
      };
    })

    (lib.mkIf (cfg.sites != { } || cfg.apps != { }) {
      services.nginx = {
        enable = true;
        # File sites serve their directory; apps reverse-proxy to their loopback
        # port. Both route by domain (or as the default vhost when no domain).
        virtualHosts =
          (lib.mapAttrs (name: site: {
            serverName = if site.domain != null then site.domain else name;
            default = site.domain == null;
            root = site.root;
            locations."/".index = "index.html";
          }) cfg.sites)
          // (lib.mapAttrs (name: app: {
            serverName = if app.domain != null then app.domain else name;
            default = app.domain == null;
            locations."/".proxyPass = "http://127.0.0.1:${toString app.port}";
          }) cfg.apps);
      };

      # Each app runs as its own sandboxed service on loopback. The firewall is
      # NOT opened for the app's port — only nginx (80) reaches it.
      systemd.services = lib.mapAttrs' (name: app:
        lib.nameValuePair "box-app-${name}" {
          description = "The Box app: ${name}";
          wantedBy = [ "multi-user.target" ];
          after = [ "network.target" ];
          environment.PORT = toString app.port;
          serviceConfig = {
            ExecStart = app.command;
            DynamicUser = true;
            Restart = "on-failure";
            RestartSec = 2;
            StateDirectory = "box-app-${name}";
            # Sandbox: no privilege, no host filesystem writes beyond its state.
            NoNewPrivileges = true;
            ProtectSystem = "strict";
            ProtectHome = true;
            PrivateTmp = true;
          };
        }) cfg.apps;

      # nginx is the only thing exposed for web services; app ports stay loopback.
      networking.firewall.allowedTCPPorts = [ 80 ];
    })

    (lib.mkIf (cfg.containers != { }) (
      let
        vals = lib.attrValues cfg.containers;
        anyProxied = lib.any (c: c.mode == "proxied") vals;
        exposedPorts = map (c: c.port) (lib.filter (c: c.mode == "exposed") vals);
      in
      {
        virtualisation.podman.enable = true;
        virtualisation.oci-containers.backend = "podman";
        virtualisation.oci-containers.containers = lib.mapAttrs (_: c: {
          inherit (c) image environment volumes cmd;
          imageFile = lib.mkIf (c.imageFile != null) c.imageFile;
          # proxied/internal stay on loopback (nginx is the only front door);
          # exposed binds all interfaces so the opened firewall port reaches it.
          ports = [
            "${lib.optionalString (c.mode != "exposed") "127.0.0.1:"}${toString c.port}:${toString c.containerPort}"
          ];
        }) cfg.containers;

        # Only proxied containers get an nginx vhost.
        services.nginx = lib.mkIf anyProxied {
          enable = true;
          virtualHosts = lib.mapAttrs (name: c: {
            serverName = if c.domain != null then c.domain else name;
            default = c.domain == null;
            locations."/".proxyPass = "http://127.0.0.1:${toString c.port}";
          }) (lib.filterAttrs (_: c: c.mode == "proxied") cfg.containers);
        };

        # nginx (80) when anything is proxied; plus each exposed container's port.
        # Internal containers open nothing.
        networking.firewall.allowedTCPPorts = (lib.optional anyProxied 80) ++ exposedPorts;
      }
    ))
  ]);
}
