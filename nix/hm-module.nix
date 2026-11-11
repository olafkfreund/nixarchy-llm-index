{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.llm-index;
  shared = import ./config.nix { inherit lib; };
  configPath = "${config.xdg.configHome}/llm-index/config.toml";
  exe = lib.getExe cfg.package;
  server = shared.mcpServers { inherit exe configPath; };
in
{
  options.services.llm-index = {
    enable = lib.mkEnableOption "the llm-index document and code index";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The llm-index package.";
    };
    registerPublic = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Register the public MCP server in programs.mcp.servers. The private server is never registered.";
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
        description = "`{ command; args; }` of the private MCP server; register it yourself, per agent.";
      };
    };
    agentInstructions = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
      default = builtins.readFile ../share/AGENTS.snippet.md;
      description = "Text for an agent's AGENTS.md / CLAUDE.md describing the tools.";
    };
  }
  // shared.instanceOptions { stateDir = "${config.xdg.stateHome}/llm-index"; };

  config = lib.mkIf cfg.enable {
    assertions = shared.assertions "services.llm-index" cfg;

    xdg.configFile."llm-index/config.toml".source = (pkgs.formats.toml { }).generate "llm-index.toml" (
      shared.configAttrs cfg
    );

    systemd.user.services.llm-index = {
      Unit.Description = "llm-index indexer";
      Service = shared.serviceConfig { inherit cfg exe configPath; };
      Install.WantedBy = [ "default.target" ];
    };

    programs.mcp = lib.mkIf cfg.registerPublic {
      enable = lib.mkDefault true;
      servers.llm-index-public = cfg.mcpServers.public;
    };
  };
}
