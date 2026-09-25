{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    mkPackageOption
    types
    ;
  cfg = config.services.weinav-forge;
  instances = lib.filterAttrs (_: instance: instance.enable) cfg.instances;
  names = builtins.attrNames instances;
  cacheGroup = "weinav-forge-cache";
  buildGroup = "weinav-forge-work";
  publicGroup = "weinav-forge-public";
  fetchUser = "weinav-fetch";
  buildUser = "weinav-build";
  runtime = "/run/weinav-forge";
  executable = "${cfg.package}/bin/weinav-forge";
  quote = lib.escapeShellArg;
  commonArgs =
    instance:
    [
      "--flavor"
      instance.flavor
      "--systems"
      (lib.concatStringsSep "," instance.systems)
      "--cache"
      cfg.cacheDirectory
    ]
    ++ lib.optional (!instance.agnss) "--no-agnss";
  flavors = lib.unique (map (name: instances.${name}.flavor) names);
  fetchCommands = lib.concatMapStringsSep "\n" (
    flavor:
    let
      selected = builtins.filter (i: i.flavor == flavor) (builtins.attrValues instances);
      union = {
        inherit flavor;
        systems = lib.unique (lib.concatMap (i: i.systems) selected);
        agnss = builtins.any (i: i.agnss) selected;
      };
      sources = lib.concatLists (
        lib.mapAttrsToList (
          role: urls:
          lib.concatMap (url: [
            "--url"
            "${role}=${url}"
          ]) urls
        ) cfg.fetch.sourceUrls
      );
    in
    "${executable} fetch ${lib.escapeShellArgs (commonArgs union ++ sources)}"
  ) flavors;
  cacheCleanup = ''
    find ${quote "${cfg.cacheDirectory}/urls"} -maxdepth 1 -type f -name '*.json' \
      -mmin +${toString (cfg.retention.cacheMaxAgeHours * 60)} -delete
    referenced=$(mktemp ${quote "${cfg.cacheDirectory}/references.XXXXXX"})
    trap 'rm -f -- "$referenced"' EXIT
    find ${quote cfg.cacheDirectory} -maxdepth 1 -type f -name '*.json' -print0 \
      | xargs -0 -r jq -r '.sources[].sha256' > "$referenced"
    find ${quote "${cfg.cacheDirectory}/urls"} -maxdepth 1 -type f -name '*.json' -print0 \
      | xargs -0 -r jq -r '.sha256' >> "$referenced"
    sort -u -o "$referenced" "$referenced"
    while IFS= read -r -d $'\0' object; do
      digest=''${object##*/}
      if ! grep -Fxq -- "$digest" "$referenced"; then
        rm -f -- "$object"
      fi
    done < <(find ${quote "${cfg.cacheDirectory}/objects"} -maxdepth 1 -type f \
      -mmin +${toString (cfg.retention.cacheMaxAgeHours * 60)} -print0)
  '';
  fetchScript = pkgs.writeShellApplication {
    name = "weinav-forge-fetch";
    runtimeInputs = with pkgs; [
      coreutils
      findutils
      gnugrep
      jq
      util-linux
    ];
    text = ''
      exec 9>${runtime}/lock
      flock -x 9
      ${fetchCommands}
      ${cacheCleanup}
    '';
  };
  compressFile = file: ''
    ${lib.optionalString cfg.compression.gzip.enable ''
      ${cfg.compression.gzip.package}/bin/pigz -n -${toString cfg.compression.gzip.level} \
        -p ${toString cfg.compression.threads} -c "${file}" > "${file}.gz"
      ${cfg.compression.gzip.package}/bin/pigz -d -c "${file}.gz" > "$stage/gzip-roundtrip"
      cmp "${file}" "$stage/gzip-roundtrip"
      if [ "$(stat -c %s "${file}.gz")" -ge "$(stat -c %s "${file}")" ]; then
        rm -- "${file}.gz"
      fi
    ''}
    ${lib.optionalString cfg.compression.brotli.enable ''
      ${cfg.compression.brotli.package}/bin/brotli -q ${toString cfg.compression.brotli.quality} \
        -c "${file}" > "${file}.br"
      ${cfg.compression.brotli.package}/bin/brotli -d -c "${file}.br" > "$stage/brotli-roundtrip"
      cmp "${file}" "$stage/brotli-roundtrip"
      if [ "$(stat -c %s "${file}.br")" -ge "$(stat -c %s "${file}")" ]; then
        rm -- "${file}.br"
      fi
    ''}
  '';
  buildOne =
    name: instance:
    let
      destination = "${cfg.outputDirectory}/${name}";
      args =
        commonArgs instance
        ++ [
          "--fit-rms"
          (toString instance.fitRms)
          "--report"
          "${cfg.stateDirectory}/reports/${name}.json"
        ]
        ++ lib.optional instance.allowDegraded "--allow-degraded";
    in
    ''
      (
        set -e
        destination=${quote destination}
        stage=$(mktemp -u ${quote "${cfg.stateDirectory}/staging/${name}.XXXXXXXXXXXX"})
        mkdir -m 0770 "$stage"
        publication=""
        pointer=""
        trap 'rm -rf -- "$stage"; if [ -n "$publication" ]; then rm -rf -- "$publication"; fi; if [ -n "$pointer" ]; then rm -f -- "$pointer"; fi' EXIT
        ${executable} process ${lib.escapeShellArgs args} --output "$stage"
        candidate=$(mktemp -u "$destination/.generations/.stage.XXXXXXXXXXXX")
        mkdir -m 0770 "$candidate"
        publication=$candidate
        cat "$stage/ephemeris.zip" > "$publication/ephemeris.zip"
        ${compressFile "$publication/ephemeris.zip"}
        printf '%s\n' ${
          quote (builtins.toJSON { inherit (instance) flavor systems agnss; })
        } > "$publication/variant.json"
        stamp=$(unzip -p "$publication/ephemeris.zip" time)
        [[ "$stamp" =~ ^[0-9]{13}$ ]]
        age=$(( $(date +%s%3N) - stamp ))
        if [ "$age" -lt 0 ] || [ "$age" -ge 1800000 ]; then
          echo "Refuse ${name}: ZIP timestamp is outside the publication window" >&2
          exit 1
        fi
        chmod 00770 "$publication"
        chmod 0640 "$publication"/*
        generation="generation-$(date +%s)-''${publication##*.}"
        sync -f "$publication"
        mv -T "$publication" "$destination/.generations/$generation"
        publication=""
        pointer="$destination/.current-''${generation}"
        ln -s ".generations/$generation" "$pointer"
        mv -Tf "$pointer" "$destination/current"
        pointer=""
        sync -f "$destination"
        echo "Published ${name}: $generation, original timestamp $stamp"
      ) &
      worker=$!
      if ! wait "$worker"; then
        failed=1
        echo "Update failed for ${name}; keep its last published generation" >&2
      fi
    '';
  buildManifest = ''
    (
      set -e
      destination=${quote "${cfg.outputDirectory}/.manifest"}
      stage=$(mktemp -u ${quote "${cfg.stateDirectory}/staging/manifest.XXXXXXXXXXXX"})
      mkdir -m 0770 "$stage"
      publication=""
      pointer=""
      trap 'rm -rf -- "$stage"; if [ -n "$publication" ]; then rm -rf -- "$publication"; fi; if [ -n "$pointer" ]; then rm -f -- "$pointer"; fi' EXIT
      : > "$stage/variants.jsonl"
      ${lib.concatMapStringsSep "\n" (name: ''
        archive_root=$(readlink -f ${quote "${cfg.outputDirectory}/${name}/current"} || true)
        if [ -f "$archive_root/ephemeris.zip" ]; then
          generation=''${archive_root##*/}
          [[ "$generation" =~ ^generation-[0-9]+-[A-Za-z0-9]+$ ]]
          profile='{}'
          if [ -f "$archive_root/variant.json" ]; then
            profile=$(cat "$archive_root/variant.json")
          fi
          digest=$(sha256sum "$archive_root/ephemeris.zip")
          digest=''${digest%% *}
          stamp=$(unzip -p "$archive_root/ephemeris.zip" time)
          jq -cn --arg name ${quote name} --arg generation "$generation" \
            --arg url "${cfg.nginx.location}${name}/generations/$generation/ephemeris.zip" \
            --arg latest_url ${quote "${cfg.nginx.location}${name}/ephemeris.zip"} \
            --arg sha256 "$digest" --argjson size "$(stat -c %s "$archive_root/ephemeris.zip")" \
            --argjson timestamp_ms "$stamp" --argjson profile "$profile" \
            '$profile + {name:$name,generation:$generation,url:$url,latest_url:$latest_url,sha256:$sha256,size:$size,timestamp_ms:$timestamp_ms}' \
            >> "$stage/variants.jsonl"
        fi
      '') names}
      jq -s '{version:1,variants:.}' "$stage/variants.jsonl" > "$stage/manifest.json"
      candidate=$(mktemp -u "$destination/.generations/.stage.XXXXXXXXXXXX")
      mkdir -m 0770 "$candidate"
      publication=$candidate
      cat "$stage/manifest.json" > "$publication/manifest.json"
      ${compressFile "$publication/manifest.json"}
      chmod 00770 "$publication"
      chmod 0640 "$publication"/*
      generation="generation-$(date +%s)-''${publication##*.}"
      sync -f "$publication"
      mv -T "$publication" "$destination/.generations/$generation"
      publication=""
      pointer="$destination/.current-''${generation}"
      ln -s ".generations/$generation" "$pointer"
      mv -Tf "$pointer" "$destination/current"
      pointer=""
      sync -f "$destination"
      echo "Published discovery manifest: $generation"
    ) &
    worker=$!
    if ! wait "$worker"; then
      failed=1
      echo "Manifest update failed; keep the last published manifest" >&2
    fi
  '';
  generationCleanup =
    name:
    let
      destination = "${cfg.outputDirectory}/${name}";
    in
    ''
      destination=${quote destination}
      current=$(readlink "$destination/current" || true)
      kept=0
      while IFS= read -r generation; do
        kept=$((kept + 1))
        if [ "$kept" -le ${toString cfg.retention.generations} ] || [ ".generations/$generation" = "$current" ]; then
          continue
        fi
        if [ -f ${quote "${cfg.outputDirectory}/manifest.json"} ] && \
          jq -e --arg name ${quote name} --arg generation "$generation" \
            'any(.variants[]; .name == $name and .generation == $generation)' \
            ${quote "${cfg.outputDirectory}/manifest.json"} >/dev/null; then
          continue
        fi
        if [ -n "$(find "$destination/.generations/$generation" -maxdepth 0 \
          -mmin +${toString (cfg.retention.minimumGenerationAgeHours * 60)} -print)" ]; then
          rm -rf -- "$destination/.generations/$generation"
        fi
      done < <(find "$destination/.generations" -mindepth 1 -maxdepth 1 -type d -name 'generation-*' -printf '%f\n' | sort -r)
      if [ -f "$destination/current/ephemeris.zip" ]; then
        stamp=$(unzip -p "$destination/current/ephemeris.zip" time)
        if [[ "$stamp" =~ ^[0-9]{13}$ ]] && [ "$(( $(date +%s%3N) - stamp ))" -ge 1800000 ]; then
          echo "${name}: published output is expired; keep timestamp $stamp until a complete replacement is ready" >&2
        fi
      fi
    '';
  buildScript = pkgs.writeShellApplication {
    name = "weinav-forge-build";
    runtimeInputs = with pkgs; [
      coreutils
      diffutils
      findutils
      jq
      unzip
      util-linux
    ];
    text = ''
      exec 9>${runtime}/lock
      flock -x 9
      mkdir -p ${quote "${cfg.stateDirectory}/staging"} ${quote "${cfg.stateDirectory}/reports"}
      find ${quote "${cfg.stateDirectory}/staging"} -mindepth 1 -maxdepth 1 -type d -exec rm -rf -- {} +
      ${lib.concatMapStringsSep "\n" (name: ''
        mkdir -p ${quote "${cfg.outputDirectory}/${name}/.generations"}
        find ${quote "${cfg.outputDirectory}/${name}/.generations"} -mindepth 1 -maxdepth 1 -type d -name '.stage.*' -exec rm -rf -- {} +
        find ${quote "${cfg.outputDirectory}/${name}"} -maxdepth 1 -type l -name '.current-*' -delete
        while IFS= read -r -d $'\0' sidecar; do
          original=''${sidecar%.*}
          if [ -f "$original" ] && [ "$(stat -c %s "$sidecar")" -ge "$(stat -c %s "$original")" ]; then
            rm -- "$sidecar"
          fi
        done < <(find ${quote "${cfg.outputDirectory}/${name}/.generations"} -type f \
          \( -name '*.gz' -o -name '*.br' \) -print0)
      '') (names ++ [ ".manifest" ])}
      failed=0
      ${lib.concatStringsSep "\n" (lib.mapAttrsToList buildOne instances)}
      ${buildManifest}
      ${lib.concatMapStringsSep "\n" generationCleanup (names ++ [ ".manifest" ])}
      exit "$failed"
    '';
  };
  sandbox = {
    Type = "oneshot";
    DynamicUser = true;
    UMask = "0007";
    NoNewPrivileges = true;
    CapabilityBoundingSet = "";
    AmbientCapabilities = "";
    ProtectSystem = "strict";
    ProtectHome = true;
    PrivateTmp = true;
    PrivateDevices = true;
    PrivateIPC = true;
    ProtectKernelTunables = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    ProtectControlGroups = true;
    ProtectClock = true;
    ProtectHostname = true;
    ProtectProc = "invisible";
    ProcSubset = "pid";
    RestrictNamespaces = true;
    RestrictRealtime = true;
    RestrictSUIDSGID = true;
    LockPersonality = true;
    MemoryDenyWriteExecute = true;
    SystemCallArchitectures = "native";
    SystemCallFilter = [ "@system-service" ];
    SystemCallErrorNumber = "EPERM";
    RemoveIPC = true;
    KeyringMode = "private";
    TasksMax = cfg.limits.tasksMax;
    MemoryMax = cfg.limits.memoryMax;
    CPUQuota = cfg.limits.cpuQuota;
    Nice = 10;
    NoExecPaths = paths ++ [ runtime ];
  };
  paths = [
    cfg.cacheDirectory
    cfg.stateDirectory
    cfg.outputDirectory
  ];
  pathValid =
    path:
    builtins.match "/[A-Za-z0-9_+./-]+" path != null
    && !lib.hasSuffix "/" path
    && builtins.all (
      part:
      !(lib.elem part [
        ""
        "."
        ".."
      ])
    ) (lib.tail (lib.splitString "/" path));
  serveConfig = mime: ''
    default_type ${mime};
    types { }
    gzip off;
    brotli off;
    gzip_static ${if cfg.compression.gzip.enable then "on" else "off"};
    brotli_static ${if cfg.compression.brotli.enable then "on" else "off"};
    gzip_vary on;
    add_header Vary Accept-Encoding always;
    add_header Cache-Control "no-store" always;
    open_file_cache off;
    autoindex off;
    limit_except GET { deny all; }
  '';
  currentLocations =
    key: uri: directory: file: mime:
    let
      suffix = builtins.substring 0 12 (builtins.hashString "sha256" key);
      variable = "weinav_${suffix}";
      internal = "/_weinav_internal_${suffix}";
    in
    [
      {
        name = "= ${cfg.nginx.location}${uri}";
        value.extraConfig = ''
          root ${cfg.outputDirectory}/${directory}/current;
          set ${"$"}${variable} $realpath_root;
          rewrite ^ ${internal} last;
        '';
      }
      {
        name = "= ${internal}";
        value.extraConfig = ''
          internal;
          alias ${"$"}${variable}/${file};
          ${serveConfig mime}
        '';
      }
    ];
  locations = lib.listToAttrs (
    currentLocations "manifest" "manifest.json" ".manifest" "manifest.json" "application/json"
    ++ lib.concatMap (
      name:
      let
        capture = "weinav_generation_${builtins.substring 0 12 (builtins.hashString "sha256" name)}";
      in
      currentLocations "variant:${name}" "${name}/ephemeris.zip" name "ephemeris.zip" "application/zip"
      ++ [
        {
          name = "~ ^${cfg.nginx.location}${name}/generations/(?<${capture}>generation-[0-9]+-[A-Za-z0-9]+)/ephemeris[.]zip$";
          value.extraConfig = ''
            alias ${cfg.outputDirectory}/${name}/.generations/${"$"}${capture}/ephemeris.zip;
            ${serveConfig "application/zip"}
          '';
        }
      ]
    ) names
  );
in
{
  options.services.weinav-forge = {
    enable = mkEnableOption "scheduled GNSS archive builds";
    package = mkPackageOption pkgs "weinav-forge" { default = null; };
    cacheDirectory = mkOption {
      type = types.str;
      default = "/var/cache/weinav-forge";
      description = "Directory for source objects and manifests. Only the fetch service can write here.";
    };
    stateDirectory = mkOption {
      type = types.str;
      default = "/var/lib/weinav-forge";
      description = "Private directory for build reports and staging files.";
    };
    outputDirectory = mkOption {
      type = types.str;
      default = "/var/lib/weinav-forge-public";
      description = "Directory for immutable published generations and current links.";
    };
    instances = mkOption {
      default = { };
      description = "Archive products, with one stable URL per instance name.";
      type = types.attrsOf (
        types.submodule {
          options = {
            enable = mkOption {
              type = types.bool;
              default = true;
              description = "Build this instance.";
            };
            flavor = mkOption {
              type = types.enum [
                "huawei"
                "huawei-plus"
                "open-plus"
                "open"
              ];
              description = "Explicit source policy for this instance.";
            };
            systems = mkOption {
              type = types.listOf (
                types.enum [
                  "gps"
                  "glonass"
                  "galileo"
                  "bds"
                  "qzs"
                ]
              );
              default = [
                "gps"
                "glonass"
                "galileo"
                "bds"
                "qzs"
              ];
              description = "Constellations to include. GPS is required.";
            };
            agnss = mkOption {
              type = types.bool;
              default = true;
              description = "Include AGNSS RTCM data.";
            };
            fitRms = mkOption {
              type = types.addCheck types.number (value: value > 0);
              default = 1;
              description = "Maximum orbit fit RMS in metres.";
            };
            allowDegraded = mkOption {
              type = types.bool;
              default = false;
              description = "Permit the open flavor's declared gaps and partial EXTRA data.";
            };
          };
        }
      );
    };
    fetch = {
      timerConfig = mkOption {
        type = types.attrsOf utils.systemdUtils.unitOptions.unitOption;
        default = {
          OnCalendar = "*:0/10";
          Persistent = true;
          RandomizedDelaySec = "30s";
        };
        example = {
          OnCalendar = "hourly";
          Persistent = true;
        };
        description = "Systemd timer settings for source updates.";
      };
      timeout = mkOption {
        type = types.str;
        default = "15min";
        description = "Maximum fetch service run time.";
      };
      sourceUrls = mkOption {
        type = types.attrsOf (types.listOf types.str);
        default = { };
        description = "Optional source URL fallback chains, indexed by CLI source role.";
      };
    };
    build.timeout = mkOption {
      type = types.str;
      default = "15min";
      description = "Maximum build and publication service run time.";
    };
    compression = {
      threads = mkOption {
        type = types.ints.positive;
        default = 1;
        description = "Maximum pigz compression threads. Brotli uses one thread.";
      };
      gzip = {
        enable = mkOption {
          type = types.bool;
          default = true;
          description = "Publish gzip sidecars when they are smaller than the original files.";
        };
        level = mkOption {
          type = types.ints.between 1 9;
          default = 6;
          description = "Pigz compression level.";
        };
        package = mkPackageOption pkgs "pigz" { };
      };
      brotli = {
        enable = mkOption {
          type = types.bool;
          default = true;
          description = "Publish Brotli sidecars when they are smaller than the original files.";
        };
        quality = mkOption {
          type = types.ints.between 0 11;
          default = 6;
          description = "Brotli compression quality.";
        };
        package = mkPackageOption pkgs "brotli" { };
      };
    };
    limits = {
      memoryMax = mkOption {
        type = types.str;
        default = "1G";
        description = "Memory limit for each service.";
      };
      cpuQuota = mkOption {
        type = types.str;
        default = "100%";
        description = "CPU time limit for each service.";
      };
      tasksMax = mkOption {
        type = types.ints.positive;
        default = 64;
        description = "Task limit for each service.";
      };
    };
    retention = {
      generations = mkOption {
        type = types.ints.positive;
        default = 3;
        description = "Minimum number of recent generations to retain per instance.";
      };
      minimumGenerationAgeHours = mkOption {
        type = types.ints.positive;
        default = 24;
        description = "Minimum age before old unpublished generations can be removed. The current generation is always retained.";
      };
      cacheMaxAgeHours = mkOption {
        type = types.ints.positive;
        default = 168;
        description = "Age limit for unreferenced source objects and obsolete URL metadata. Committed manifests keep their source objects.";
      };
    };
    nginx = {
      enable = mkEnableOption "nginx locations for published archives";
      virtualHost = mkOption {
        type = types.str;
        default = "localhost";
        example = "gnss.example.org";
        description = "Nginx virtual host to create or extend. Configure TLS in the nginx module.";
      };
      location = mkOption {
        type = types.str;
        default = "/agnss/";
        description = "URL prefix, with leading and trailing slashes.";
      };
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = names != [ ];
        message = "weinav-forge: enable at least one instance.";
      }
      {
        assertion = builtins.all (name: builtins.match "[A-Za-z0-9][A-Za-z0-9_-]*" name != null) names;
        message = "weinav-forge: instance names must contain letters, digits, underscores or hyphens.";
      }
      {
        assertion = builtins.all pathValid paths;
        message = "weinav-forge: use absolute directory paths without spaces, traversal or trailing slashes.";
      }
      {
        assertion =
          builtins.all (a: builtins.all (b: a == b || !(lib.hasPrefix "${a}/" b)) paths) paths
          && builtins.length (lib.unique paths) == 3;
        message = "weinav-forge: cache, state and output directories must be separate and must not contain each other.";
      }
      {
        assertion = builtins.all (
          i: lib.elem "gps" i.systems && builtins.length i.systems == builtins.length (lib.unique i.systems)
        ) (builtins.attrValues instances);
        message = "weinav-forge: each instance requires GPS and a unique constellation list.";
      }
      {
        assertion = builtins.all (i: i.flavor != "open" || i.allowDegraded) (builtins.attrValues instances);
        message = "weinav-forge: open instances require allowDegraded.";
      }
      {
        assertion = builtins.match "/([A-Za-z0-9_-]+/)*" cfg.nginx.location != null;
        message = "weinav-forge: nginx.location must be a path prefix with leading and trailing slashes.";
      }
    ];
    users = {
      groups = {
        ${cacheGroup} = { };
        ${buildGroup} = { };
        ${publicGroup} = { };
      };
      users = {
        ${config.services.nginx.user}.extraGroups = lib.mkIf cfg.nginx.enable [ publicGroup ];
      };
    };
    systemd = {
      tmpfiles.rules = [
        "d ${cfg.cacheDirectory} 2770 root ${cacheGroup} -"
        "Z ${cfg.cacheDirectory} ~2770 root ${cacheGroup} -"
        "A+ ${cfg.cacheDirectory} - - - - d:u::rwx,d:g::rwx,d:o::---"
        "d ${cfg.stateDirectory} 2770 root ${buildGroup} -"
        "Z ${cfg.stateDirectory} ~2770 root ${buildGroup} -"
        "A+ ${cfg.stateDirectory} - - - - d:u::rwx,d:g::rwx,d:o::---"
        "d ${cfg.outputDirectory} 2770 root ${buildGroup} -"
        "Z ${cfg.outputDirectory} ~2770 root ${buildGroup} -"
        "A+ ${cfg.outputDirectory} - - - - g::rwx,g:${publicGroup}:r-x,d:u::rwx,d:g::rwx,d:g:${publicGroup}:r-x,d:o::---"
        "L ${cfg.outputDirectory}/manifest.json - - - - .manifest/current/manifest.json"
        "L ${cfg.outputDirectory}/manifest.json.gz - - - - .manifest/current/manifest.json.gz"
        "L ${cfg.outputDirectory}/manifest.json.br - - - - .manifest/current/manifest.json.br"
        "d ${runtime} 0750 root ${cacheGroup} -"
        "f ${runtime}/lock 0660 root ${cacheGroup} -"
      ];
      timers.weinav-forge-fetch = {
        description = "Update GNSS source data";
        wantedBy = [ "timers.target" ];
        timerConfig = cfg.fetch.timerConfig // {
          Unit = "weinav-forge-fetch.service";
        };
      };
      services = {
        weinav-forge-fetch = {
          description = "Fetch GNSS source data";
          wants = [ "network-online.target" ];
          after = [
            "network-online.target"
            "systemd-tmpfiles-setup.service"
          ];
          unitConfig.OnSuccess = "weinav-forge-build.service";
          environment.SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
          serviceConfig = sandbox // {
            User = fetchUser;
            Group = cacheGroup;
            ExecStart = lib.getExe fetchScript;
            TimeoutStartSec = cfg.fetch.timeout;
            RestrictAddressFamilies = [
              "AF_UNIX"
              "AF_INET"
              "AF_INET6"
            ];
            ReadWritePaths = [
              cfg.cacheDirectory
              runtime
            ];
            InaccessiblePaths = [
              cfg.stateDirectory
              cfg.outputDirectory
            ];
          };
        };
        weinav-forge-build = {
          description = "Build, compress and publish GNSS archives";
          after = [ "systemd-tmpfiles-setup.service" ];
          serviceConfig = sandbox // {
            User = buildUser;
            Group = buildGroup;
            SupplementaryGroups = [ cacheGroup ];
            ExecStart = lib.getExe buildScript;
            TimeoutStartSec = cfg.build.timeout;
            PrivateNetwork = true;
            IPAddressDeny = "any";
            RestrictAddressFamilies = [ "AF_UNIX" ];
            SocketBindDeny = "any";
            SystemCallFilter = sandbox.SystemCallFilter ++ [ "~@network-io" ];
            TemporaryFileSystem = [ "/run:ro" ];
            BindPaths = [ runtime ];
            BindReadOnlyPaths = [ cfg.cacheDirectory ];
            ReadWritePaths = [
              cfg.stateDirectory
              cfg.outputDirectory
              runtime
            ];
          };
        };
      };
    };
    services.nginx = mkIf cfg.nginx.enable {
      enable = true;
      additionalModules = [ pkgs.nginxModules.brotli ];
      virtualHosts.${cfg.nginx.virtualHost}.locations = locations // {
        "${cfg.nginx.location}".return = "404";
      };
    };
  };
}
