# Evaluates both modules with minimal configs and asserts their observable properties.
{
  pkgs,
  lib,
  nixpkgs,
  home-manager,
  hmModule,
  nixosModule,
}:
let
  roots = {
    docs = {
      path = "/home/alice/Docs";
      tier = "public";
      kind = "local";
    };
    nas = {
      path = "/mnt/nas";
      kind = "network";
    };
    other.path = "/home/alice/other"; # auto: counted as network
  };
  # 16 + 3 workers + 1 local + 2 network + 8
  expectedTasks = 30;

  llm = extra: {
    inherit roots;
    limits.workers = 3;
    exclude.paths = [ "/home/alice/Docs/tmp" ];
  }
  // extra;

  nixos =
    instance:
    (nixpkgs.lib.nixosSystem {
      inherit (pkgs.stdenv.hostPlatform) system;
      modules = [
        nixosModule
        {
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "none";
            fsType = "tmpfs";
          };
          system.stateVersion = "25.11";
          services.llm-index.instances.alice = instance;
        }
      ];
    }).config;

  hm =
    llmConfig:
    (home-manager.lib.homeManagerConfiguration {
      inherit pkgs;
      modules = [
        hmModule
        {
          home = {
            username = "alice";
            homeDirectory = "/home/alice";
            stateVersion = "25.11";
          };
          services.llm-index = {
            enable = true;
          }
          // llmConfig;
        }
      ];
    }).config;

  # Which assertions fire: the modules' own assertion function (nix/config.nix) over an
  # evaluated option set. Home Manager throws when its `assertions` are read, so this is
  # the way to see the messages.
  shared = import ../config.nix { inherit lib; };
  fired =
    extra:
    let
      cfg =
        (lib.evalModules {
          modules = [
            { options.c = shared.instanceOptions { stateDir = "/state"; }; }
            { c = llm extra; }
          ];
        }).config.c;
    in
    map (a: a.message) (lib.filter (a: !a.assertion) (shared.assertions "p" cfg));
  esc = builtins.fromJSON ''"\u001b"'';
  del = builtins.fromJSON ''"\u007f"'';
  only = sub: msgs: lib.length msgs == 1 && lib.hasInfix sub (lib.head msgs);

  builds = drv: (builtins.tryEval drv.drvPath).success;
  hActivation = h.home.activationPackage;
  nxToplevel = nx.system.build.toplevel;

  nx = nixos (llm { });
  nxUnit = nx.systemd.services.llm-index-alice.serviceConfig;
  h = hm (llm { });
  hUnit = h.systemd.user.services.llm-index.Service;

  forbidden = [
    "ProtectSystem"
    "ProtectHome"
    "PrivateTmp"
    "ReadWritePaths"
  ];
  hardened =
    unit:
    unit.NoNewPrivileges
    && unit.RestrictAddressFamilies == "AF_UNIX"
    && unit.OOMPolicy == "continue"
    && unit.MemoryMax == "1024M"
    && unit.WorkingDirectory == "/"
    && unit.Restart == "on-failure"
    && lib.hasInfix "index --self-test --config" unit.ExecStartPre
    && lib.all (k: !(unit ? ${k})) forbidden;

  checks = {
    "HM registers llm-index-public only" = lib.attrNames h.programs.mcp.servers == [ "llm-index-public" ];
    "HM public server passes --config explicitly" =
      lib.take 5 h.programs.mcp.servers.llm-index-public.args == [
        "mcp"
        "--tier"
        "public"
        "--config"
        "/home/alice/.config/llm-index/config.toml"
      ];
    "HM registerPublic = false registers nothing" =
      (hm (llm { registerPublic = false; })).programs.mcp.servers == { };
    "HM TasksMax formula" = hUnit.TasksMax == expectedTasks;
    "HM unit hardening" = hardened hUnit;
    "HM config file is generated" = h.xdg.configFile ? "llm-index/config.toml";
    "NixOS TasksMax formula" = nxUnit.TasksMax == expectedTasks;
    "NixOS unit hardening" = hardened nxUnit;
    "NixOS unit user, state dir, no network" =
      nxUnit.User == "alice"
      && nxUnit.StateDirectory == "llm-index/alice"
      && nxUnit.PrivateNetwork
      && lib.hasInfix "/etc/llm-index/alice.toml" nxUnit.ExecStart;
    "NixOS config in /etc" = nx.environment.etc ? "llm-index/alice.toml";
    "NixOS has no registerPublic" = !(nx.services.llm-index.instances.alice ? registerPublic);

    # The module assertions make the whole configuration unbuildable.
    "valid HM and NixOS configs build" = builds hActivation && builds nxToplevel;
    "no assertion fires for the valid config" = fired { } == [ ];
    "workers = 1 fires the workers assertion only" =
      only "workers must be at least 2" (fired { limits.workers = 1; });
    "exclusion outside every root fires the exclusion assertion only" =
      only "at or under a root path" (fired { exclude.paths = [ "/elsewhere" ]; });
    "relative root fires the absolute-path assertion only" =
      only "absolute and normalized" (fired { roots = roots // { rel.path = "rel"; }; });
    "non-normal roots fire the normalization assertion" = lib.all (
      p: only "absolute and normalized" (fired { roots = roots // { x.path = p; }; })
    ) [ "/a/./b" "/a/../b" "/a//b" "/a/b/" ];
    "duplicate root path fires the duplicate assertion only" =
      only "duplicate root path" (fired { roots = roots // { dup.path = "/mnt/nas"; }; });
    "empty root name fires the name assertion only" =
      only "names must be nonempty" (fired { roots = roots // { "".path = "/home/alice/e"; }; });
    "non-normal exclusion fires the exclusion normalization assertion" =
      only "absolute and normalized" (fired { exclude.paths = [ "/home/alice/Docs/./tmp" ]; });
    "control character in a root fires the control-character assertion only" = lib.all (
      c: only "control characters" (fired { roots = roots // { x.path = "/a${c}b"; }; })
    ) [ "\n" "\t" esc del ];
    "control character in an exclusion fires the control-character assertion only" =
      only "control characters" (fired { exclude.paths = [ "/home/alice/Docs/t\nmp" ]; });
    "control character in stateDir fires the control-character assertion only" =
      only "control characters" (fired { stateDir = "/state\n"; });
    "bad interval fires the interval assertion only" = lib.all (
      i: only "interval must match" (fired { roots = roots // { docs = roots.docs // { interval = i; }; }; })
    ) [ "15" "5x" "m" "0m" "1.5h" ];
    "bad backoffMax fires the backoff assertion only" = lib.all (
      b: only "backoffMax must match" (fired { limits.backoffMax = b; })
    ) [ "60" "1w" "" "0s" ];
    "MemoryHigh is 75% of memoryMaxMb" =
      hUnit.MemoryHigh == "768M" && nxUnit.MemoryHigh == "768M"
      && (hm (llm { limits.memoryMaxMb = 2000; })).systemd.user.services.llm-index.Service.MemoryHigh == "1500M";
    "HM workers = 1 fails" = !builds (hm (llm { limits.workers = 1; })).home.activationPackage;
    "HM exclusion outside every root fails" =
      !builds (hm (llm { exclude.paths = [ "/elsewhere" ]; })).home.activationPackage;
    "HM relative root fails" = !builds (hm (llm { roots.rel.path = "rel"; })).home.activationPackage;
    "HM duplicate root path fails" =
      !builds (hm (llm { roots.dup.path = "/mnt/nas"; })).home.activationPackage;
    "HM bad interval fails" =
      !builds (hm (llm { roots.docs.interval = "5x"; })).home.activationPackage;
    "NixOS bad backoffMax fails" =
      !builds (nixos (llm { limits.backoffMax = "1w"; })).system.build.toplevel;
    "NixOS workers = 1 fails" = !builds (nixos (llm { limits.workers = 1; })).system.build.toplevel;
    "NixOS exclusion outside every root fails" =
      !builds (nixos (llm { exclude.paths = [ "/elsewhere" ]; })).system.build.toplevel;
  };
  bad = lib.attrNames (lib.filterAttrs (_: ok: !ok) checks);
in
assert lib.assertMsg (bad == [ ]) "module checks failed: ${lib.concatStringsSep "; " bad}";
pkgs.runCommand "llm-index-modules" { } ''
  echo ${lib.escapeShellArg (lib.concatStringsSep "\n" (lib.attrNames checks))} > $out
''
