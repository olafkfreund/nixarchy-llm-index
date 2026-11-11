---
status: draft
issue: 4
author: olafkfreund
---

# Intent: onboarding, installation and reference documentation

## Problem

v0.1 is merged, but a new user cannot get from zero to a working search
with the current docs:

- `README.md` lists the modules and options but has no quick start, no
  prerequisites check, and no way to confirm the index is running and
  searchable (`llm-index status`, a first query).
- Nothing explains how to connect the MCP server to a specific agent
  client (Claude Code, Codex, Gemini CLI or another MCP client), or that the
  chat model is independent of the index's own built-in embedding model.
- There is no CLI reference (`index`, `mcp`, `status`, `--self-test`), no
  description of how the index works (coordinator, sandboxed workers, two
  tiers, hybrid ranking), no security model page, no troubleshooting, and no
  contributor guide (dev shell, tests, VM checks, benchmark).
- Some README facts are already stale (`MemoryHigh` is now 75% of
  `memoryMaxMb`, not a fixed `768M`).

## Proposed outcome

- `README.md` is a short landing page: what it is, requirements, a quick
  start for Home Manager and for NixOS, connecting an agent, verifying it
  works, and links into `docs/`.
- `docs/` holds focused pages: installation and configuration (full option
  reference), agent clients (per-client MCP setup), CLI reference,
  architecture, security model, troubleshooting, and development.
- Every command and option in the docs matches the code at the time of
  writing (checked against `src/config.rs`, `src/main.rs`, `src/mcp.rs` and
  `nix/*.nix`).

## Affected users and systems

New users of the flake, agent clients that consume MCP servers, the owner
enabling it on p620 (nixos_config#2858). Docs only: no code, module or
behaviour change.

## Constraints

- Docs only; no change to `src/`, `nix/` or the checks.
- Do not document features that do not exist (no GPU, OCR, `.pptx`, remote
  embedding backends, non-x86_64 platforms).
- Never suggest registering the private-tier server with a cloud or
  untrusted client; keep the existing warning.
- Client config snippets must use each client's documented format; where a
  format cannot be verified, say so rather than guess.
- Keep `docs/feasibility.md` and `docs/benchmark.md` as they are.

## Open questions

- Should the per-client guide cover only clients the owner uses (Claude
  Code, Codex, agy/Gemini CLI), or also name other MCP clients generically?
  Proposed: the three named plus a generic "any stdio MCP client" section.
