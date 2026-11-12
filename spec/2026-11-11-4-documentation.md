---
status: draft
issue: 4
intent: intent/2026-11-11-4-documentation.md
---

# Spec: onboarding, installation and reference documentation

## Design

Plain Markdown in the repo, rendered by GitHub. One landing page plus
focused pages under `docs/`, each answering one question. Facts come from
the code, not from the current README: every option, flag, tool parameter
and path is read from `src/` and `nix/` while writing.

### Files

| File | Answers | Main content |
| --- | --- | --- |
| `README.md` (rewrite, ~150 lines) | What is it, and how do I get a first search? | One-paragraph pitch; what it is not (no GPU, no network, no API key); requirements; quick start for Home Manager and for NixOS; connect one agent; verify it works; links to `docs/`; licence. |
| `docs/installation.md` | How do I install and configure it? | Choosing a route: HM user unit `llm-index.service` or NixOS system unit `llm-index-<owner>.service` (one route per index); flake input; HM and NixOS examples; putting the binary on `PATH` (`config.services.llm-index.package`); config file locations (HM `$XDG_CONFIG_HOME/llm-index/config.toml`, NixOS `/etc/llm-index/<owner>.toml` and `--config`); full option reference (moved from the README, corrected: `MemoryHigh` = 75% of `memoryMaxMb`, rounded down); permissions (NixOS runs as the owner, `stateDir` under `/var/lib/`); network roots (the user mounts the share; `kind`, `share`, `interval`, offline behaviour); state-dir and sync restrictions; Landlock requirement; service and log commands (`systemctl --user` / `systemctl`, `journalctl`); upgrading and uninstalling (what to delete). |
| `docs/agents.md` | How do I use it from my LLM client? | The model behind the client does not matter; the client must speak stdio MCP. What HM `programs.mcp.servers` registration does and does not do. Per-client setup: Claude Code, Codex, Gemini CLI (also used by agy), each with the client's documented config format and a check command; a generic stdio section (command, args, no env needed); local models: the client front-end must support MCP. Adding the agent snippet (`agentInstructions`) to `CLAUDE.md`/`AGENTS.md`/`GEMINI.md`. Private tier: local clients only, with the reason. |
| `docs/reference.md` | What exactly can I run and call? | CLI: global `--config`, `index`, `index --self-test`, `mcp --tier public\|private`, `status`; `worker` named as internal; exit codes where the code defines them. MCP tools `search`, `get`, `recent`, `status`: every parameter with type, default and limits from `src/mcp.rs`; search modes; filters; result fields; stale/offline marks; `get` continuation via `next` / `offset`; the fail-closed "index configuration is being checked" message; what is wrapped (passages) and what is only escaped (titles, symbols, headings, sheet names). Supported file types and limits. |
| `docs/architecture.md` | How does it work? | One mermaid diagram (coordinator, sandboxed workers, two SQLite DBs, two MCP servers, clients). Indexing flow (scan → extract → chunk → embed → store), watching (inotify for local, polling for network), share scheduling, ranking (FTS5 + int8 vectors, hybrid), the embedding model and why it is static and offline, crash containment. Short; links to the spec and plan for detail. |
| `docs/security.md` | What can leak, and what stops it? | Threat model in plain terms: the two tiers and their isolation; a `public` root's paths, titles and passages reach every registered client and its model provider; worker sandbox (Landlock read-only, no TCP, rlimits, systemd hardening); built-in denylist and `liftBuiltin`; symlink and mount-boundary rules; untrusted-content handling; what is not protected (residual parser aborts are contained, not prevented; anyone with your user account can read the DBs). |
| `docs/troubleshooting.md` | It does not work — now what? | Symptom → cause → fix, from real failure modes in the code: refuses to start (no Landlock, state dir on network/FUSE or in a synced folder, config assertion), self-test failure, root `config_error` / `offline` / `degraded`, "index configuration is being checked", a file not found by search (denylist, size or cell limit, unsupported type, `failed`), network share hangs (fail-closed), MCP client shows no tools. Each with the command to diagnose. |
| `docs/development.md` | How do I work on it? | Dev shell (`nix develop`, the env vars it sets), `cargo test --all-features`, clippy, `nix flake check` and what each check covers, running one VM test, the benchmark (link `docs/benchmark.md`), repo layout, the artifact workflow (intent → spec → plan) for changes. |
| `share/AGENTS.snippet.md` | (shipped to agents) | Add `offset` to `get`'s signature and say when to use it; no other change unless a signature is wrong. |

`docs/feasibility.md` and `docs/benchmark.md` stay unchanged and are
linked. Performance figures in the README and architecture page are quoted
as "measured on p620 (smoke run)" next to the targets; the reference-laptop
row stays blank.

### Client formats

Each client snippet is checked against that client's current
documentation (Context7 or the vendor docs) at writing time:

- Claude Code: `claude mcp add` with `-s user`, and the JSON form.
- Codex: `[mcp_servers.<name>]` in `~/.codex/config.toml`.
- Gemini CLI: `mcpServers` in `~/.gemini/settings.json`.

Where a detail cannot be confirmed, the page says so instead of guessing.
No other client is named with a config snippet.

### Style

Short sentences, task-first headings, every runnable command in a fenced
block with a language tag, NixOS-module commands shown with the
`--config /etc/llm-index/<owner>.toml` they need. No marketing language.

## Alternatives rejected

- **mdBook / generated docs site.** Needs a build and hosting; GitHub
  renders Markdown and that is enough for v0.1.
- **Generate the option reference from the modules (`nixosOptionsDoc`).**
  Accurate by construction, but adds Nix code and a check, which the intent
  excludes (docs only). Worth doing later.
- **Keep everything in one README.** Already 235 lines and growing;
  onboarding gets lost under the reference.
- **Document a `nix run` / `PATH` wrapper or an MCP-registering NixOS
  option.** That is a code change; the docs describe today's modules and
  name the gap.

## Risks

- Client config formats change after writing. Mitigation: each client
  section links the vendor docs and shows a check command.
- Docs drift from the code on the next change. Mitigation: the
  development page says option, flag or tool changes update
  `docs/reference.md` / `docs/installation.md` in the same PR.
- A wrong command in the quick start costs every new user. Mitigation:
  every command in README and `docs/installation.md` is run during
  verification.
- No host or service is touched; nothing to roll back beyond the docs.

## Verification

1. Option table: script lists option names from `nix/config.nix`,
   `nix/hm-module.nix`, `nix/nixos-module.nix` and diffs them against the
   names in `docs/installation.md`; no missing or extra options. Defaults
   checked by hand against the same files.
2. CLI: `llm-index --help` and each subcommand's `--help` from the built
   package match `docs/reference.md`.
3. MCP tools: the tool list and input schemas printed by
   `tests/mcp_client.py` (or an MCP `tools/list` call) match
   `docs/reference.md` and `share/AGENTS.snippet.md`.
4. Commands: the quick-start and verification commands run on p620 against
   a scratch config (`--config`, temp roots, temp state dir) and give the
   documented output; no system or user service is changed.
5. Links: every relative link and anchor in `README.md` and `docs/`
   resolves.
6. `nix flake check --no-build` still evaluates (the snippet is part of the
   package).
7. Codex review of the finished docs against the code, read-only.
