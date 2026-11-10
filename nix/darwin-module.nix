{
  config,
  lib,
  ...
}:
let
  cfg = config.services.fast-nix-gc;
  ocfg = config.services.fast-nix-optimise;
in
{
  imports = [ ./service-options.nix ];

  options.services.fast-nix-gc.startCalendarInterval = lib.mkOption {
    type = with lib.types; listOf (attrsOf int);
    default = [
      {
        Hour = 3;
        Minute = 15;
      }
    ];
    example = [
      {
        Hour = 3;
        Minute = 15;
      }
    ];
    description = ''
      When to run garbage collection, as launchd
      {manpage}`launchd.plist(5)` StartCalendarInterval entries.
    '';
  };

  options.services.fast-nix-optimise.startCalendarInterval = lib.mkOption {
    type = with lib.types; listOf (attrsOf int);
    default = [
      {
        Hour = 4;
        Minute = 15;
      }
    ];
    example = [
      {
        Hour = 4;
        Minute = 15;
      }
    ];
    description = ''
      When to run, as launchd {manpage}`launchd.plist(5)`
      StartCalendarInterval entries.
    '';
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = cfg.automatic -> config.nix.enable;
          message = "services.fast-nix-gc.automatic requires nix.enable";
        }
      ];

      warnings = lib.optional (cfg.automatic && config.nix.gc.automatic) ''
        Both services.fast-nix-gc.automatic and nix.gc.automatic are enabled.
        Disable nix.gc.automatic to avoid running two garbage collectors.
      '';

      launchd.daemons.fast-nix-gc = {
        # nix-darwin waits for the Nix store before running command.
        command = lib.escapeShellArgs cfg.argv;
        # `nix config show` needs nix on PATH even with an external installer.
        path = lib.optional config.nix.enable config.nix.package ++ [
          "/nix/var/nix/profiles/default/bin"
          "/usr/local/bin"
          "/usr/bin"
          "/bin"
        ];
        serviceConfig = {
          RunAtLoad = false;
          StartCalendarInterval = lib.mkIf cfg.automatic cfg.startCalendarInterval;
        };
      };
    })

    (lib.mkIf ocfg.enable {
      assertions = [
        {
          assertion = ocfg.automatic -> config.nix.enable;
          message = "services.fast-nix-optimise.automatic requires nix.enable";
        }
      ];

      warnings = lib.optional (ocfg.automatic && config.nix.optimise.automatic) ''
        Both services.fast-nix-optimise.automatic and nix.optimise.automatic are
        enabled. Disable nix.optimise.automatic to avoid running both.
      '';

      launchd.daemons.fast-nix-optimise = {
        command = lib.escapeShellArgs ocfg.argv;
        serviceConfig = {
          RunAtLoad = false;
          StartCalendarInterval = lib.mkIf ocfg.automatic ocfg.startCalendarInterval;
        };
      };
    })
  ];
}
