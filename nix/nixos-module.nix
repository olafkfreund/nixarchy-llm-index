{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  shared = import ./config.nix { inherit lib; };
  instances = config.services.llm-index.instances;
  etc = owner: "/etc/llm-index/${owner}.toml";
in
{
  options.services.llm-index.instances = lib.mkOption {
    default = { };
    description = "One indexer per owner; the attribute name is the user it runs as.";
    type = lib.types.attrsOf (
      lib.types.submodule (
        { name, config, ... }:
        let
          exe = lib.getExe config.package;
          server = shared.mcpServers {
            inherit exe;
            configPath = etc name;
          };
        in
        {
          options = {
            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "The llm-index package.";
            };
            mcpServers = {
              public = lib.mkOption {
                readOnly = true;
                default = server "public";
                description = "`{ command; args; }` of the public MCP server.";
              };
              private = lib.mkOption {
                readOnly = true;
                default = server "private";
                description = "`{ command; args; }` of the private MCP server.";
              };
            };
            agentInstructions = lib.mkOption {
              type = lib.types.str;
              readOnly = true;
              default = builtins.readFile ../share/AGENTS.snippet.md;
              description = "Text for an agent's AGENTS.md / CLAUDE.md describing the tools.";
            };
          }
          // shared.instanceOptions { stateDir = "/var/lib/llm-index/${name}"; };
        }
      )
    );
  };

  config = {
    assertions = lib.concatLists (
      lib.mapAttrsToList (
        owner: cfg:
        shared.assertions "services.llm-index.instances.${owner}" cfg
        ++ [
          {
            assertion = lib.hasPrefix "/var/lib/" cfg.stateDir;
            message = "services.llm-index.instances.${owner}.stateDir must be under /var/lib/ (it is the unit's StateDirectory)";
          }
        ]
      ) instances
    );

    environment.etc = lib.mapAttrs' (
      owner: cfg:
      lib.nameValuePair "llm-index/${owner}.toml" {
        source = (pkgs.formats.toml { }).generate "llm-index-${owner}.toml" (shared.configAttrs cfg);
      }
    ) instances;

    systemd.services = lib.mapAttrs' (
      owner: cfg:
      lib.nameValuePair "llm-index-${owner}" {
        description = "llm-index indexer for ${owner}";
        wantedBy = [ "multi-user.target" ];
        restartTriggers = [ config.environment.etc."llm-index/${owner}.toml".source ];
        serviceConfig =
          shared.serviceConfig {
            inherit cfg;
            exe = lib.getExe cfg.package;
            configPath = etc owner;
          }
          // {
            User = owner;
            StateDirectory = lib.removePrefix "/var/lib/" cfg.stateDir;
            StateDirectoryMode = "0700";
            PrivateNetwork = true;
          };
      }
    ) instances;
  };
}
