# llm-index

A local, sandboxed index of your documents and code for LLM agents. It runs as
a background service on NixOS or Home Manager, keeps itself up to date, and
answers over MCP, so any agent that speaks MCP can search your files by
words, by meaning, or both.

- **Local.** No network listener, no remote embedding service, no GPU. Search
  by meaning uses a small static model that ships in the package.
- **Sandboxed.** Parsers run in Landlock-confined worker processes that can
  only read their own root.
- **Two tiers.** Folders are `public` or `private` (default). The public MCP
  server opens a separate database and never sees private files.
- **Read-only.** The agent gets four tools: `search`, `get`, `recent`, `status`.

## What it is not

- Not a cloud service, and not a chat model. Your agent's model is separate.
- Not an OCR tool. Scanned pages and images are not indexed; `.pptx` is not
  supported.
- Not a backup or sync tool. It reads your files and never changes them.
- Not for other platforms: `x86_64-linux` only, with Nix flakes.

## Requirements

- `x86_64-linux`, Nix with flakes, Home Manager or NixOS.
- A kernel with Landlock (the NixOS default). Without it the service refuses
  to start.

## Quick start: Home Manager

Add the flake input (see [installation](docs/installation.md#add-the-flake)),
then:

```nix
{ inputs, config, ... }:
{
  imports = [ inputs.llm-index.homeManagerModules.default ];

  services.llm-index = {
    enable = true;
    roots = {
      code = { path = "${config.home.homeDirectory}/Source"; tier = "public"; };
      documents.path = "${config.home.homeDirectory}/Documents"; # private
    };
  };

  # The module does not put llm-index on PATH.
  home.packages = [ config.services.llm-index.package ];
}
```

Apply it with your usual Home Manager switch.

## Quick start: NixOS

```nix
{ inputs, config, ... }:
{
  imports = [ inputs.llm-index.nixosModules.default ];

  # The module does not create the account.
  users.users.alice = {
    isNormalUser = true;
    packages = [ config.services.llm-index.instances.alice.package ];
  };

  services.llm-index.instances.alice.roots = {
    code = { path = "/home/alice/Source"; tier = "public"; };
    documents.path = "/home/alice/Documents";
  };
}
```

Run `nixos-rebuild switch`. On NixOS the config is
`/etc/llm-index/alice.toml`, so every command needs `--config` with that path.
All options: [installation](docs/installation.md).

## Check that it works

```sh
# Home Manager
systemctl --user status llm-index
llm-index status

# NixOS
systemctl status llm-index-alice
llm-index --config /etc/llm-index/alice.toml status
```

`status` lists each root with its state and counts. Wait until `queued` is 0
for a first search; the first scan takes a while on large folders.

## Connect an agent

Claude Code:

```sh
claude mcp add -s user llm-index-public -- llm-index mcp --tier public
```

On NixOS add `--config /etc/llm-index/alice.toml` after `--tier public`.
Then ask the agent to find something in a public root, for example "search my
code for where the config file is parsed". The public server never sees
private roots such as Documents above. `claude mcp list` shows the server.

Codex, Gemini CLI and generic clients, the agent instruction snippet, and the
private tier are in [docs/agents.md](docs/agents.md). Any model works through
an MCP client. Register the private server only with clients that run a local
model: tool results go to the model.

## Performance

Targets on the reference laptop (4 cores, 8 GB): initial index of 10,000 files
within 2 h, indexer memory at most 1 GB, `mcp` at most 100 MB, 100 local edits
searchable within 5 min. Measured on p620 (smoke run, not the laptop): 93 s,
229 MB, 34.7 MB, 3.3 s. The reference-laptop run is still to do. Details:
[benchmark](docs/benchmark.md).

## Documentation

| Page | Contents |
| --- | --- |
| [Installation](docs/installation.md) | Routes, options, network roots, upgrading, uninstalling |
| [Connect an agent](docs/agents.md) | Claude Code, Codex, Gemini CLI, the agent snippet, local models |
| [Reference](docs/reference.md) | CLI, MCP tools, supported file types and limits |
| [Architecture](docs/architecture.md) | Components, indexing flow, ranking |
| [Security](docs/security.md) | Tiers, sandbox, denylist, what is not protected |
| [Troubleshooting](docs/troubleshooting.md) | Symptoms, causes, fixes |
| [Development](docs/development.md) | Dev shell, tests, checks, repository layout |
| [Feasibility](docs/feasibility.md) | Pins and choices checked before building |
| [Benchmark](docs/benchmark.md) | Method and results |

## Licence

MIT. See [LICENSE](LICENSE).
