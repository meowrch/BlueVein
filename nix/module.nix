{ lib, pkgs, config, ... }:

let
  cfg = config.services.bluevein;
in
{
  options.services.bluevein = {
    enable = lib.mkEnableOption "BlueVein Bluetooth key sync service";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.callPackage ./package.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./package.nix { }";
      description = "BlueVein package to run.";
    };

    efiDevice = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/dev/disk/by-uuid/A1B2-C3D4";
      description = "Optional EFI device path passed as BLUEVEIN_EFI_DEVICE.";
    };

    extraEnvironment = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      example = {
        RUST_BACKTRACE = "1";
      };
      description = "Additional environment variables for the BlueVein service.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.bluevein = {
      description = "BlueVein Bluetooth Key Sync Service";
      documentation = [ "https://github.com/meowrch/BlueVein" ];
      wantedBy = [ "multi-user.target" ];
      after = [ "bluetooth.target" "network.target" ];

      environment =
        (lib.optionalAttrs (cfg.efiDevice != null) {
          BLUEVEIN_EFI_DEVICE = cfg.efiDevice;
        })
        // cfg.extraEnvironment;

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/bluevein";
        Restart = "on-failure";
        RestartSec = 5;
        NoNewPrivileges = true;
        PrivateTmp = true;
      };
    };
  };
}
