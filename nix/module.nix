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
  };

  config = lib.mkIf cfg.enable {
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
  };
}
