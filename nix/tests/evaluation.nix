{
  nixpkgs,
  system,
  pkgs,
  module,
}:
let
  inherit (pkgs) lib;
  evaluate =
    settings:
    (nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        module
        {
          system.stateVersion = "26.05";
          services.weinav-forge.package = pkgs.hello;
        }
        settings
      ];
    }).config;
  valid =
    config:
    builtins.all (a: a.assertion || !(lib.hasPrefix "weinav-forge:" a.message)) config.assertions;
  disabled = evaluate { };
  enabled = evaluate {
    services.weinav-forge = {
      enable = true;
      instances.watch.flavor = "huawei-plus";
      nginx.enable = true;
      nginx.virtualHost = "example.test";
    };
    services.nginx.virtualHosts."example.test".locations."/other".return = "204";
  };
  custom = evaluate {
    services.weinav-forge = {
      enable = true;
      cacheDirectory = "/srv/gnss-cache";
      stateDirectory = "/srv/gnss-state";
      outputDirectory = "/srv/gnss-public";
      instances.first = {
        flavor = "huawei";
        systems = [ "gps" ];
        fitRms = 0.5;
      };
      instances.second = {
        flavor = "open";
        allowDegraded = true;
      };
    };
  };
  invalidName = evaluate {
    services.weinav-forge.enable = true;
    services.weinav-forge.instances."../escape".flavor = "huawei";
  };
  invalidOpen = evaluate {
    services.weinav-forge.enable = true;
    services.weinav-forge.instances.watch.flavor = "open";
  };
  invalidPaths = evaluate {
    services.weinav-forge = {
      enable = true;
      outputDirectory = "/var/lib/weinav-forge/public";
      instances.watch.flavor = "huawei";
    };
  };
  noInstances = evaluate { services.weinav-forge.enable = true; };
  wrongType = builtins.tryEval (
    builtins.deepSeq
      (evaluate {
        services.weinav-forge = {
          enable = true;
          instances.watch.flavor = "invalid";
        };
      }).services.weinav-forge.instances
      true
  );
in
assert !(disabled.systemd.services ? weinav-forge-fetch);
assert !(disabled.systemd.services ? weinav-forge-build);
assert valid enabled;
assert valid custom;
assert enabled.systemd.timers.weinav-forge-fetch.timerConfig.OnCalendar == "*:0/10";
assert
  !valid (evaluate {
    services.weinav-forge = {
      enable = true;
      cacheDirectory = "/var/lib/./weinav-forge";
      instances.watch.flavor = "huawei";
    };
  });
assert !valid invalidName;
assert !valid invalidOpen;
assert !valid invalidPaths;
assert !valid noInstances;
assert !wrongType.success;
assert enabled.services.weinav-forge.package == pkgs.hello;
assert enabled.services.nginx.virtualHosts."example.test".locations."/other".return == "204";
assert
  enabled.systemd.services.weinav-forge-fetch.unitConfig.OnSuccess == "weinav-forge-build.service";
assert enabled.systemd.services.weinav-forge-build.serviceConfig.PrivateNetwork;
assert enabled.systemd.services.weinav-forge-fetch.serviceConfig.DynamicUser;
assert enabled.systemd.services.weinav-forge-build.serviceConfig.DynamicUser;
assert !(enabled.users.users ? weinav-fetch);
assert !(enabled.users.users ? weinav-build);
assert !(enabled.users.users ? weinav-forge-fetch);
assert !(enabled.users.users ? weinav-forge-build);
assert
  enabled.systemd.services.weinav-forge-fetch.serviceConfig.User
  != enabled.systemd.services.weinav-forge-build.serviceConfig.User;
assert enabled.systemd.timers.weinav-forge-fetch.timerConfig.RandomizedDelaySec == "30s";
assert enabled.systemd.services.weinav-forge-build.serviceConfig.IPAddressDeny == "any";
assert
  custom.systemd.services.weinav-forge-build.serviceConfig.BindReadOnlyPaths == [ "/srv/gnss-cache" ];
pkgs.runCommand "weinav-forge-module-evaluation" { } ''
  touch "$out"
''
