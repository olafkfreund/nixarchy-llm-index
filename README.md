# nixarchy-llm-index

A local, sandboxed, self-updating document and code index for LLM agents on
NixOS and Home Manager. Search documents and source code by meaning, exact
words, or both across local folders and mounted network shares over stdio MCP,
with strict separation between public and private tiers.

## Status (v0.1)

- **Platform:** `x86_64-linux` only (pure Nix flake evaluation, offline builds,
  no GPU, no external database server, no network listener).
- **Supported formats:**
  - Prose and markup: Markdown (`.md`, `.markdown`, `.mdx`), plain text
    (`.txt`, `.text`, `.rst`, `.adoc`, `.asciidoc`, `.org`, `.log`, `.cfg`,
    `.ini`, `.conf`, `README`, `LICENSE`, `CHANGELOG`, etc.), HTML (`.html`,
    `.htm`, `.xhtml`).
  - Documents: PDF text layer (`.pdf`, via `pdfium` compiled with V8 and XFA
    disabled), Word (`.docx`), OpenDocument Text (`.odt`).
  - Spreadsheets: Excel (`.xlsx`, `.xls`), OpenDocument Spreadsheet (`.ods`),
    CSV (`.csv`).
  - Source code (chunked by AST declarations via `tree-sitter`): Rust, Python,
    Go, JavaScript, TypeScript, Nix, C, C++, Java, Bash, TOML, YAML, JSON, CSS.
  - Not in v0.1: OCR, `.pptx`.

## Requirements

### Landlock LSM

`llm-index` requires Linux Landlock support in the running kernel (`security.lsm`
must include `landlock`, which is enabled by default on NixOS). Both the
coordinator (`llm-index index`) and every short-lived `worker` child enforce
Landlock rulesets before touching any indexed root, and the systemd service runs
`llm-index index --self-test` as `ExecStartPre`. If Landlock is unavailable,
`llm-index index` refuses to start with no override.

### State Directory and File Sync Caveat

The index databases (`public.db` and `private.db`, in SQLite WAL mode) live in
`stateDir` (`$XDG_STATE_HOME/llm-index` under Home Manager, or
`/var/lib/llm-index/<owner>` under NixOS) with directory mode `0700` and file
mode `0600`.

- **Never place `stateDir` on a network or FUSE filesystem or inside a synced
  folder.** `llm-index index` checks `statfs` and parent directories at startup
  and refuses to start if `stateDir` is on NFS/CIFS/FUSE or beneath a sync
  marker (`.stfolder`, `.sync`, `.dropbox`, `.nextcloudsync.log`,
  `.sync_*.db`).
- **If you sync `$HOME`** (with Syncthing, Dropbox, Nextcloud, etc.), exclude
  `$XDG_STATE_HOME/llm-index` (default `~/.local/state/llm-index`) in your sync
  client, or configure `stateDir` outside the synced tree. `llm-index` also
  writes a `CACHEDIR.TAG` file in `stateDir` and treats `stateDir` as a
  built-in indexing exclusion.

## Installation

Add `nixarchy-llm-index` to your `flake.nix` inputs:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    llm-index = {
      url = "github:olafkfreund/nixarchy-llm-index";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
}
```

### Home Manager Module (`homeManagerModules.default`)

Import `llm-index.homeManagerModules.default` in your Home Manager
configuration (loaded as a NixOS flake module and rebuilt with `nixos-rebuild`):

```nix
{ inputs, config, ... }:
{
  imports = [ inputs.llm-index.homeManagerModules.default ];

  services.llm-index = {
    enable = true;
    registerPublic = true; # registers programs.mcp.servers.llm-index-public

    roots = {
      code = {
        path = "${config.home.homeDirectory}/Source";
        tier = "public";
        kind = "local";
      };
      documents = {
        path = "${config.home.homeDirectory}/Documents";
        tier = "private";
        kind = "local";
      };
      nas-archive = {
        path = "/mnt/nas/archive";
        tier = "private";
        kind = "network";
        interval = "15m";
        share = "nas:/export/archive";
      };
    };

    exclude = {
      paths = [ "${config.home.homeDirectory}/Documents/Scratch" ];
      liftBuiltin = [ ];
    };
  };
}
```

### NixOS Module (`nixosModules.default`)

Import `llm-index.nixosModules.default` in a host configuration to run one
sandboxed system service per user (`services.llm-index.instances.<owner>`):

```nix
{ inputs, ... }:
{
  imports = [ inputs.llm-index.nixosModules.default ];

  services.llm-index.instances.alice = {
    roots = {
      projects = {
        path = "/home/alice/projects";
        tier = "public";
        kind = "local";
      };
      vault = {
        path = "/home/alice/vault";
        tier = "private";
        kind = "local";
      };
    };
  };
}
```

## Options Summary

Shared options (`services.llm-index.*` in Home Manager, or
`services.llm-index.instances.<owner>.*` in NixOS):

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `package` | package | `llm-index.packages.x86_64-linux.default` | The `llm-index` package to run. |
| `stateDir` | string | HM: `"${config.xdg.stateHome}/llm-index"`; NixOS: `"/var/lib/llm-index/<owner>"` | Mode `0700` directory holding `public.db` and `private.db`. |
| `roots.<name>.path` | string | *(required)* | Canonical absolute path with no symlink component. |
| `roots.<name>.tier` | `"public"` \| `"private"` | `"private"` | Security tier; roots default to `"private"`. |
| `roots.<name>.kind` | `"auto"` \| `"local"` \| `"network"` | `"auto"` | Storage kind (`local` uses `inotify`; `network` polls at `interval`). |
| `roots.<name>.interval` | string | `"15m"` | Poll interval for network or degraded roots. |
| `roots.<name>.share` | null or string | `null` | Expected share source (e.g. `"nas:/export"`) for network roots. |
| `exclude.paths` | list of strings | `[ ]` | Excluded absolute paths; every entry must be at or under a configured root. |
| `exclude.liftBuiltin` | list of strings | `[ ]` | Built-in denylist patterns lifted for `private` roots only. |
| `limits.maxFileMb` | positive int | `50` | Maximum input file size in MiB. |
| `limits.maxTextBytes` | positive int | `33554432` | Maximum extracted text bytes per file (32 MiB). |
| `limits.maxCells` | positive int | `2000000` | Maximum spreadsheet cells per workbook. |
| `limits.memoryMaxMb` | positive int | `1024` | Service `MemoryMax` in MiB (`MemoryHigh` is `768M`). |
| `limits.workers` | int (`>= 2`) | `2` | Maximum concurrent scan/extract worker processes. |
| `limits.backoffMax` | string | `"60m"` | Maximum exponential backoff for offline network shares. |
| `mcpServers.public` | read-only attrset | `{ command; args; }` | Command and arguments for `llm-index mcp --tier public`. |
| `mcpServers.private` | read-only attrset | `{ command; args; }` | Command and arguments for `llm-index mcp --tier private`. |
| `agentInstructions` | read-only string | contents of `share/AGENTS.snippet.md` | Prompt snippet explaining how agents should use the MCP tools. |

Home Manager additionally provides:

- `services.llm-index.enable` (`bool`, default `false`)
- `services.llm-index.registerPublic` (`bool`, default `true`): automatically
  registers `programs.mcp.servers.llm-index-public`.

## Tiers and Registering the Private Server

`llm-index` splits indexed data across two isolated SQLite databases:

- **`public` tier (`llm-index mcp --tier public`):** opens `public.db` only.
  It has no flag, code path, or SQL attachment that can open `private.db`.
  Private root names, paths, file counts, and validation error paths are never
  stored in `public.db`. If a newly added root or exclusion has not yet been
  validated against symlinks by the indexer, or has `config_error`, every
  public tool fails closed with the generic message
  `index configuration is being checked`.
- **`private` tier (`llm-index mcp --tier private`):** opens both `public.db`
  and `private.db`, searching across public and private roots.

`registerPublic = true` in the Home Manager module registers **only** the
public server in `programs.mcp.servers`. The private server is never
auto-registered globally. Register `mcpServers.private` explicitly only on a
trusted, local-only agent client:

```nix
{ config, ... }:
{
  # Example: pass the read-only private server definition to a local-only client
  programs.myLocalAgent.mcpServers.llm-index-private =
    config.services.llm-index.mcpServers.private;
}
```

Or on NixOS for user `alice`:

```nix
config.services.llm-index.instances.alice.mcpServers.private
```

## How Agents Use the Index

The package installs `$out/share/llm-index/AGENTS.snippet.md` (also exposed in
Nix as `services.llm-index.agentInstructions` and in this repository at
[`share/AGENTS.snippet.md`](share/AGENTS.snippet.md)). Include it in your
agent's `AGENTS.md` or `CLAUDE.md`:

```nix
home.file.".claude/CLAUDE.md".text = ''
  ${config.services.llm-index.agentInstructions}
'';
```

The MCP server exposes four read-only tools over stdio:

- `search(query, mode?, field?, folder?, type?, after?, before?, limit?)`
- `get(path, start?, end?, sheet?, offset?)`
- `recent(days, folder?, type?)`
- `status()`

All returned passages and titles are wrapped in `<untrusted-document path="…">`
tags with HTML-escaped `<` and `&` and stripped Unicode control/bidi/tag
characters so agents treat document text strictly as untrusted data.

## Licence

MIT.
