---
status: approved
issue: 4
author: olafkfreund
---

# Intent: onboarding, installation and reference documentation

## Problem

v0.1 is merged. The README has installation examples, an options table,
requirements and tier guidance, but there is no complete, verified path
from zero to a working search, and no organised reference:

- No quick start and no end-to-end check that the index is running and
  searchable. Neither module puts `llm-index` on `PATH`
  (`nix/hm-module.nix:12`, `nix/nixos-module.nix:89`), and the NixOS module
  writes `/etc/llm-index/<owner>.toml` while the CLI defaults to
  `$XDG_CONFIG_HOME/llm-index/config.toml` (`nix/nixos-module.nix:11`,
  `src/config.rs:112`), so a bare `llm-index status` does not work after
  either install.
- Nothing explains how to connect the MCP server to a specific agent client
  (Claude Code, Codex, Gemini CLI or another stdio MCP client), that HM's
  `programs.mcp.servers` registration is not the same as per-client setup,
  or that the chat model is independent of the index's own built-in,
  offline embedding model.
- No CLI reference (`index`, `mcp`, `status`, `--self-test`, `--config`) and
  no MCP tool reference (search modes and filters, response fields, stale
  marks, `get` continuation). `share/AGENTS.snippet.md:18` already omits
  `get`'s `offset`.
- No architecture page, troubleshooting page or contributor guide; the
  security material in the README is scattered.
- Stale or wrong README facts: `MemoryHigh` is not a fixed `768M` but 75%
  of `memoryMaxMb`, rounded down (`nix/config.nix:194`); titles are
  escaped but not wrapped in `<untrusted-document>` tags, only passages are
  (`README.md:229`, `src/mcp.rs:151-161`).

## Proposed outcome

- `README.md` is a short landing page: what it is, requirements, a quick
  start for Home Manager and for NixOS, connecting an agent, verifying it
  works, and links into `docs/`.
- `docs/` holds focused pages:
  - installation and configuration: choosing one route (HM user unit or
    NixOS system unit `llm-index-<owner>`) per index, full option
    reference, permissions (NixOS runs as the owner; `stateDir` must be
    under `/var/lib/`), mounts the user must provide, service and log
    commands, state-dir and sync restrictions;
  - agent clients: per-client MCP setup and the generic stdio case;
  - CLI and MCP tool reference;
  - architecture;
  - security model, including that a `public` root's paths, titles and
    passages reach every registered client and its model provider;
  - troubleshooting;
  - development: dev shell, tests, VM checks, benchmark.
- Verification commands work as written after either install route, with
  the right executable, config path and owner.
- `share/AGENTS.snippet.md` matches the current tool signatures.
- Performance statements separate measured results (p620 smoke runs) from
  targets; the reference-laptop table stays blank until measured.

## Affected users and systems

New users of the flake, agent clients that consume MCP servers, the owner
enabling it on p620 (nixos_config#2858). Docs and the agent snippet only.

## Constraints

- No change to `src/`, `nix/` or the checks. `share/AGENTS.snippet.md` is the
  only shipped file that changes.
- Do not document features that do not exist (no GPU, OCR, `.pptx`, remote
  embedding backends, non-x86_64 platforms).
- Never suggest registering the private-tier server with a cloud or
  untrusted client. Tell users to treat every document-derived field as
  untrusted.
- Client config snippets use each client's documented format; where a
  format cannot be verified, say so rather than guess.
- Keep `docs/feasibility.md` and `docs/benchmark.md` as they are; link them.

## Open questions

- Per-client guide scope: proposed Claude Code, Codex and Gemini CLI (used
  by agy), plus a generic "any stdio MCP client" section. Other clients by
  name only if their format can be verified.
