# Options and helpers shared by the Home Manager and NixOS modules.
{ lib }:
rec {
  instanceOptions =
    { stateDir }:
    {
      stateDir = lib.mkOption {
        type = lib.types.str;
        default = stateDir;
        description = "State directory (mode 0700) holding public.db and private.db.";
      };
      roots = lib.mkOption {
        default = { };
        description = "Indexed roots by name.";
        type = lib.types.attrsOf (
          lib.types.submodule {
            options = {
              path = lib.mkOption {
                type = lib.types.str;
                description = "Absolute root path with no symlink component.";
              };
              tier = lib.mkOption {
                type = lib.types.enum [
                  "public"
                  "private"
                ];
                default = "private";
                description = "Only public roots are visible to the public MCP server.";
              };
              kind = lib.mkOption {
                type = lib.types.enum [
                  "auto"
                  "local"
                  "network"
                ];
                default = "auto";
                description = "Storage kind; auto is detected at run time and counted as network for TasksMax.";
              };
              interval = lib.mkOption {
                type = lib.types.str;
                default = "15m";
                description = "Rescan interval.";
              };
              share = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                example = "nas:/export";
                description = "Share identity for roots on a network mount.";
              };
            };
          }
        );
      };
      exclude = {
        paths = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          description = "Excluded paths; each must be at or under a root.";
        };
        liftBuiltin = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
          description = "Built-in denylist patterns lifted for private roots.";
        };
      };
      limits = {
        maxFileMb = lib.mkOption {
          type = lib.types.ints.positive;
          default = 50;
        };
        maxTextBytes = lib.mkOption {
          type = lib.types.ints.positive;
          default = 33554432;
        };
        maxCells = lib.mkOption {
          type = lib.types.ints.positive;
          default = 2000000;
        };
        memoryMaxMb = lib.mkOption {
          type = lib.types.ints.positive;
          default = 1024;
        };
        workers = lib.mkOption {
          type = lib.types.int;
          default = 2;
          description = "Worker processes; at least 2.";
        };
        backoffMax = lib.mkOption {
          type = lib.types.str;
          default = "60m";
        };
      };
    };

  # The attrset serialised to config.toml.
  configAttrs = cfg: {
    state_dir = cfg.stateDir;
    root = lib.mapAttrsToList (
      name: r:
      {
        inherit name;
        inherit (r) path tier kind interval;
      }
      // lib.optionalAttrs (r.share != null) { inherit (r) share; }
    ) cfg.roots;
    exclude = {
      inherit (cfg.exclude) paths;
      lift_builtin = cfg.exclude.liftBuiltin;
    };
    limits = {
      max_file_mb = cfg.limits.maxFileMb;
      max_text_bytes = cfg.limits.maxTextBytes;
      max_cells = cfg.limits.maxCells;
      memory_max_mb = cfg.limits.memoryMaxMb;
      inherit (cfg.limits) workers;
      backoff_max = cfg.limits.backoffMax;
    };
  };

  # 16 + workers + local roots + network roots + 8; `auto` roots count as network.
  tasksMax =
    cfg:
    let
      kinds = map (r: r.kind) (lib.attrValues cfg.roots);
      local = lib.count (k: k == "local") kinds;
    in
    16 + cfg.limits.workers + local + (lib.length kinds - local) + 8;

  assertions =
    prefix: cfg:
    let
      roots = lib.attrValues cfg.roots;
      under = p: lib.any (r: p == r.path || lib.hasPrefix (r.path + "/") p) roots;
    in
    [
      {
        assertion = cfg.limits.workers >= 2;
        message = "${prefix}.limits.workers must be at least 2";
      }
      {
        assertion = lib.all (r: lib.hasPrefix "/" r.path) roots;
        message = "${prefix}.roots: every path must be absolute";
      }
      {
        assertion = lib.all under cfg.exclude.paths;
        message = "${prefix}.exclude.paths: every exclusion must be at or under a root path";
      }
    ];

  # Hardening and resource limits common to both units. No ProtectSystem, ProtectHome,
  # PrivateTmp or ReadWritePaths: the workers' own Landlock sandbox does the confinement.
  serviceConfig =
    {
      cfg,
      exe,
      configPath,
    }:
    {
      ExecStartPre = "${exe} index --self-test --config ${configPath}";
      ExecStart = "${exe} index --config ${configPath}";
      Restart = "on-failure";
      WorkingDirectory = "/";
      Nice = 19;
      IOSchedulingClass = "idle";
      CPUWeight = 20;
      MemoryHigh = "768M";
      MemoryMax = "${toString cfg.limits.memoryMaxMb}M";
      OOMPolicy = "continue";
      TasksMax = tasksMax cfg;
      NoNewPrivileges = true;
      RestrictAddressFamilies = "AF_UNIX";
      SystemCallFilter = "@system-service @sandbox";
    };

  # Read-only descriptions of the two MCP servers; the config path is passed explicitly.
  mcpServers =
    { exe, configPath }:
    tier: {
      command = exe;
      args = [
        "mcp"
        "--tier"
        tier
        "--config"
        configPath
      ];
    };
}
