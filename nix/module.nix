{ config, lib, pkgs, ... }:
let
  cfg = config.services.the-box;

  # The trusted builder image for repository build steps (see boxd/src/build.rs).
  # Built from this very nixpkgs and shipped IN the platform closure — never
  # pulled from a registry — so it is pinned like everything else and updates
  # through the release channel. The user's repository is only ever data on a
  # bind mount handed to this image; there is no `podman build` on anything a
  # user wrote, because attacker-controlled build contexts are what trigger
  # container escapes.
  builderImage = pkgs.dockerTools.buildLayeredImage {
    name = "box-builder";
    tag = "latest"; # boxd retags by store hash so channel updates reload it
    contents = [
      # /bin/sh and /usr/bin/env exist: node's child_process and every npm
      # script shebang assume them.
      pkgs.dockerTools.binSh
      pkgs.dockerTools.usrBinEnv
      pkgs.dockerTools.caCertificates
      pkgs.bashInteractive
      pkgs.coreutils
      pkgs.gnugrep
      pkgs.gnused
      pkgs.gnutar
      pkgs.gzip
      pkgs.findutils
      pkgs.nodejs
      pkgs.yarn
      pkgs.pnpm
      pkgs.git
    ];
    config = {
      Env = [
        "PATH=/usr/bin:/bin"
        # The install phase talks TLS to registries; give every tool the same
        # trust root. (Node ships its own bundle; npm and git read these.)
        "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
        "GIT_SSL_CAINFO=/etc/ssl/certs/ca-bundle.crt"
      ];
      WorkingDir = "/work";
    };
  };

  # Only what the operator actually published.
  published = lib.filterAttrs (_: s: s.public);

  # Exactly ONE default server on :80, however many services lack a domain.
  # Every domain-less vhost used to claim `default_server`, and nginx treats
  # two claimants as a fatal configuration error — so the second domain-less
  # service you deployed took every site on the Box offline, and the switch
  # still reported success. Found on a real Box, not in review. The first
  # domain-less site wins (alphabetically), then apps, then containers; the
  # prefixes keep a site and an app with the same name from colliding.
  domainless = attrs: lib.attrNames (lib.filterAttrs (_: v: v.domain == null) attrs);
  defaultOwner =
    let candidates =
      (map (n: "site:${n}") (domainless cfg.sites))
      ++ (map (n: "app:${n}") (domainless cfg.apps))
      ++ (map (n: "container:${n}")
        (domainless (lib.filterAttrs (_: c: c.mode == "proxied") cfg.containers)));
    in if candidates == [ ] then null else lib.head candidates;

  # What a service's virtual host serves, independent of which plane it is on.
  siteVhost = name: site: {
    serverName = if site.domain != null then site.domain else name;
    # The live generation by default: <data>/profiles/box is the symlink boxd
    # swaps atomically on every deploy and rollback, so nginx and boxd serve
    # one set of files, at one speed.
    root =
      if site.root != null then site.root
      else "${cfg.dataDir}/profiles/box/services/${name}/www";
    locations."/".index = "index.html";
    domain = site.domain;
  };

  appVhost = app: {
    serverName = if app.domain != null then app.domain else null;
    locations."/".proxyPass = "http://127.0.0.1:${toString app.port}";
    domain = app.domain;
  };

  proxyVhost = name: c: {
    serverName = if c.domain != null then c.domain else name;
    locations."/".proxyPass = "http://127.0.0.1:${toString c.port}";
    domain = c.domain;
  };

  # Your own network: everything, on :80, exactly as before. `key` is this
  # vhost's "<kind>:<name>", judged against defaultOwner above.
  lanVhost = key: v: (removeAttrs v [ "domain" ]) // {
    serverName = if v.serverName != null then v.serverName else "_";
    default = key == defaultOwner;
  };

  # The internet, via the tunnel: loopback-only, and a service must have a
  # domain to be addressable here at all.
  publicVhost = v: (removeAttrs v [ "domain" ]) // {
    listen = [{ addr = "127.0.0.1"; port = cfg.publicListenPort; }];
    # No `default`: an unrecognized Host on the public plane matches nothing and
    # gets nginx's rejection, rather than falling into whichever service
    # happened to be first.
  };
in
{
  options.services.the-box = {
    enable = lib.mkEnableOption "The Box daemon (boxd)";

    package = lib.mkOption {
      type = lib.types.package;
      description = "The boxd package to run.";
    };

    publicListenPort = lib.mkOption {
      type = lib.types.port;
      default = 2694;
      description = ''
        Loopback port serving ONLY the services marked public — what a tunnel
        connects to. Separate from :80 so that pointing a tunnel at the Box
        cannot publish something the operator did not publish, and separate
        from the console's port so a tunnel never fronts the console.
      '';
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
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = ''
              Directory of files to serve. Leave null (what boxd's generated
              modules do) to serve the CURRENT generation's copy directly,
              through the profile symlink — so an edit that only changes content
              is live the moment boxd swaps the generation, with no system
              rebuild, and nginx and boxd serve the same bytes rather than two
              copies that drift apart. Set it to serve a fixed path instead.
            '';
          };
          domain = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Public hostname; when null the site is the default vhost.";
          };
          public = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = ''
              Whether this may be reached from the internet. Only services with
              this set are served on the public listener the tunnel connects to
              (see `publicListenPort`); everything else stays on port 80, which
              is your own network.
            '';
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
          public = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Whether this may be reached from the internet (see sites.<name>.public).";
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
          secretEnvFile = lib.mkOption {
            type = lib.types.nullOr lib.types.path;
            default = null;
            description = ''
              An age-encrypted (.age) env file of secret KEY=value lines. agenix
              decrypts it at runtime to /run/agenix and the container reads it,
              so secret values never enter box.toml or the Nix store.
            '';
          };
          domain = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Public hostname; when null the container is the default vhost.";
          };
          public = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Whether this may be reached from the internet (see sites.<name>.public).";
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
        # Rootless podman (the build sandbox) needs subordinate id ranges to
        # map user namespaces from. A range no normal user reaches.
        subUidRanges = [{ startUid = 300000; count = 65536; }];
        subGidRanges = [{ startGid = 300000; count = 65536; }];
      };
      users.groups.boxd = { };

      # The build sandbox's runtime. Always on (not just when containers are
      # deployed): a Box that cannot build is a Box that refuses half the
      # repositories people actually have.
      virtualisation.podman.enable = true;

      systemd.services.boxd = {
        description = "The Box daemon";
        wantedBy = [ "multi-user.target" ];
        after = [ "network.target" ];
        # Everything boxd shells out to. Keep this in step with the
        # `Command::new` calls in boxd/src: nix (generation builds), cloudflared
        # (BYO tunnel), avahi-browse + curl (LAN fleet discovery), git (config
        # repo history + push), tailscale (Box Connect mesh), restic + openssh
        # (backups, including sftp repos), age/age-keygen (secrets).
        path = [
          pkgs.nix
          pkgs.cloudflared
          pkgs.avahi
          pkgs.curl
          pkgs.systemd
          pkgs.age
          pkgs.git
          pkgs.tailscale
          pkgs.restic
          pkgs.openssh
          # The build sandbox: rootless podman, plus the setuid newuidmap /
          # newgidmap wrappers it maps user namespaces with ("/run/wrappers"
          # resolves to /run/wrappers/bin).
          pkgs.podman
          "/run/wrappers"
        ];
        environment = {
          # nix (invoked by boxd for generation builds) needs a writable cache
          # under $HOME; the boxd system user's default home is /var/empty.
          HOME = cfg.dataDir;
          # The platform service catalog (presets), shipped in the closure. boxd
          # merges it with the box's own user catalog under the data dir.
          BOX_CATALOG_DIR = "${../catalog}";
          # The nixpkgs this system was built from. Generations pin themselves
          # to it instead of resolving `flake:nixpkgs` through the mutable
          # registry, which needs the network (so an offline Box could not
          # deploy at all) and could otherwise drift to a different nixpkgs than
          # the one whose closure is already on this disk.
          BOX_NIXPKGS = "${pkgs.path}";
          # The trusted builder image for repository build steps, as a tarball
          # in this closure. Its presence is what tells boxd this machine can
          # run builds at all (BuildExec::detect).
          BOX_BUILDER_IMAGE = "${builderImage}";
          # Rootless podman keeps its runtime state under XDG_RUNTIME_DIR;
          # without one it guesses, and a system unit has no session to guess
          # from. RuntimeDirectory=boxd provides /run/boxd below.
          XDG_RUNTIME_DIR = "/run/boxd";
        };
        serviceConfig = {
          ExecStart = "${lib.getExe cfg.package} --data-dir ${cfg.dataDir} serve --listen ${cfg.listen}";
          User = "boxd";
          Group = "boxd";
          StateDirectory = "boxd";
          # /run/boxd, tmpfs: where a secret that arrives in a request (the
          # operator identity a restore re-keys with) is written for the length
          # of the operation, so it never touches a disk.
          RuntimeDirectory = "boxd";
          RuntimeDirectoryMode = "0700";
          # Hand this unit its own cgroup subtree. Without delegation, the
          # build sandbox's --memory/--pids limits are DECORATIVE for a
          # rootless podman run from a system unit — they apply nothing and
          # report nothing, silently (the exact trap PLAN §1 warns about).
          Delegate = "cpu cpuset io memory pids";
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
        # age: boxd decrypts the (encrypted-at-rest) backup password on demand
        # for restic via RESTIC_PASSWORD_COMMAND.
        path = [ pkgs.restic pkgs.openssh pkgs.age ];
        environment.HOME = "/root";
        serviceConfig = {
          Type = "oneshot";
          # Root, because the backup set is manifest-derived and includes state
          # only root can read (/etc/box/install-config.json is 0600 root, and a
          # container's volumes are usually root-owned). As boxd this failed on
          # every due run: restic exited non-zero, so the freshness marker was
          # never written and retention never ran. boxd only ever sees the
          # marker, which `backup::run` hands back to the data dir's owner.
          User = "root";
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
        path = [ pkgs.nix pkgs.git pkgs.systemd ];
        environment.HOME = "/root";
        # The update switches the system configuration, and switch-to-configuration
        # restarts changed units. Without this, systemd restarts THIS unit
        # mid-update and kills the updater before it can health-check the new
        # generation and roll back — the failure mode the rollback exists for.
        restartIfChanged = false;
        stopIfChanged = false;
        serviceConfig = {
          Type = "oneshot";
          User = "root";
          ExecStart = "${lib.getExe cfg.package} --data-dir ${cfg.dataDir} channel update";
        };
      };

      # Apply the CURRENT config to the running system, without taking a new
      # platform release. This is the slow half of the two-speed reconciler: a
      # structural deploy (a new container or app, a changed domain) builds its
      # generation on boxd's fast path, and needs this to actually run.
      #
      # Before this existed, a container deployed from the console or by an
      # agent sat in the config until some unrelated platform release happened
      # to come along.
      systemd.services.boxd-os-apply = {
        description = "The Box: make the system match the config";
        path = [ pkgs.nix pkgs.git pkgs.systemd ];
        environment.HOME = "/root";
        # Same reason as the channel update: this unit's own switch must not
        # restart it out from under the health check and rollback.
        restartIfChanged = false;
        stopIfChanged = false;
        serviceConfig = {
          Type = "oneshot";
          User = "root";
          ExecStart = "${lib.getExe cfg.package} --data-dir ${cfg.dataDir} os-apply";
        };
      };

      # Let the unprivileged boxd user START (only) these two units, so a deploy
      # or the dashboard's "Update now" can trigger the root reconcile without
      # granting boxd broad control over the system.
      security.polkit.extraConfig = ''
        polkit.addRule(function(action, subject) {
          if (action.id == "org.freedesktop.systemd1.manage-units" &&
              (action.lookup("unit") == "boxd-channel-update.service" ||
               action.lookup("unit") == "boxd-os-apply.service") &&
              subject.user == "boxd") {
            return polkit.Result.YES;
          }
        });
      '';

      # A machine-readable record of what the OS tier declares, for boxd/GUI
      # introspection of the composed layer stack.
      environment.etc."box/sites.json".text = builtins.toJSON (
        lib.mapAttrs (name: site: {
          inherit (site) domain;
          root =
            if site.root != null then "${site.root}"
            else "${cfg.dataDir}/profiles/box/services/${name}/www";
        }) cfg.sites
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
        # TWO PLANES, and which one a request arrives on is the whole security
        # boundary:
        #
        #   :80                  your own network. Every service, as before.
        #   127.0.0.1:<public>   the internet, via the tunnel. ONLY services
        #                        marked public — it is not even possible to
        #                        reach the others here, so "let people outside
        #                        your home reach it" is enforced by which
        #                        listener exists rather than by a check that
        #                        something has to remember to run.
        #
        # The public listener is loopback-only: nothing reaches it except
        # cloudflared on this machine, and the firewall never opens it.
        virtualHosts =
          (lib.mapAttrs (name: site: lanVhost "site:${name}" (siteVhost name site)) cfg.sites)
          // (lib.mapAttrs (name: app: lanVhost "app:${name}" (appVhost app)) cfg.apps)
          // (lib.mapAttrs' (name: site: lib.nameValuePair "${name}-public"
            (publicVhost (siteVhost name site))) (published cfg.sites))
          // (lib.mapAttrs' (name: app: lib.nameValuePair "${name}-public"
            (publicVhost (appVhost app))) (published cfg.apps));
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
        secretName = name: "box-container-${name}-env";
      in
      {
        virtualisation.podman.enable = true;
        virtualisation.oci-containers.backend = "podman";
        virtualisation.oci-containers.containers = lib.mapAttrs (name: c: {
          inherit (c) image environment volumes cmd;
          imageFile = lib.mkIf (c.imageFile != null) c.imageFile;
          # Secret env comes from the agenix-decrypted file, never box.toml.
          environmentFiles =
            lib.optional (c.secretEnvFile != null) config.age.secrets.${secretName name}.path;
          # proxied/internal stay on loopback (nginx is the only front door);
          # exposed binds all interfaces so the opened firewall port reaches it.
          ports = [
            "${lib.optionalString (c.mode != "exposed") "127.0.0.1:"}${toString c.port}:${toString c.containerPort}"
          ];
        }) cfg.containers;

        # Each container's secret env file is an agenix secret, decrypted at
        # runtime to /run/agenix (root, 0400) where the container service reads it.
        age.secrets = lib.mapAttrs'
          (name: c: lib.nameValuePair (secretName name) { file = c.secretEnvFile; })
          (lib.filterAttrs (_: c: c.secretEnvFile != null) cfg.containers);

        # Only proxied containers get an nginx vhost, and only published ones
        # appear on the plane the tunnel reaches (see the sites/apps block).
        services.nginx = lib.mkIf anyProxied {
          enable = true;
          virtualHosts =
            let proxied = lib.filterAttrs (_: c: c.mode == "proxied") cfg.containers;
            in
            (lib.mapAttrs (name: c: lanVhost "container:${name}" (proxyVhost name c)) proxied)
            // (lib.mapAttrs' (name: c: lib.nameValuePair "${name}-public"
              (publicVhost (proxyVhost name c))) (published proxied));
        };

        # nginx (80) when anything is proxied; plus each exposed container's port.
        # Internal containers open nothing.
        networking.firewall.allowedTCPPorts = (lib.optional anyProxied 80) ++ exposedPorts;
      }
    ))
  ]);
}
