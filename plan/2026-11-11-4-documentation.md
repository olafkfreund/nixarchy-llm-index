---
status: approved
issue: 4
spec: spec/2026-11-11-4-documentation.md
coders:
  2: coder   # reference + snippet: exact transcription from src/mcp.rs and main.rs, needs care, not volume
  3: coder   # installation: options table plus module facts, Sonnet coder per model split
  4: coder   # agent clients: needs vendor-doc lookups (Context7) and judgement
  5: coder   # four explanatory pages, prose from code; same agent keeps its cache warm
  6: coder   # README last, built from the finished pages
reviewers:
  - codex    # read-only review of the finished docs against the code
  # agy omitted: out of use by owner decision (#2748); add it back here to include it
---

# Plan: onboarding, installation and reference documentation

Self-contained: a coder can execute this without the intent or spec.

## Decisions carried from the approved spec

- Docs only. No change to `src/`, `nix/`, `tests/` or the checks.
  `share/AGENTS.snippet.md` is the only shipped file that changes.
- Plain GitHub Markdown. Files: `README.md` (rewrite, ~150 lines),
  `docs/installation.md`, `docs/agents.md`, `docs/reference.md`,
  `docs/architecture.md`, `docs/security.md`, `docs/troubleshooting.md`,
  `docs/development.md`. `docs/feasibility.md` and `docs/benchmark.md` stay
  unchanged and are linked.
- Facts come from the code, never from the current README. Known corrections:
  `MemoryHigh` is 75% of `memoryMaxMb`, rounded down (`nix/config.nix:194`);
  `search` passages and `get`'s `text` are wrapped in `<untrusted-document>`
  (`src/mcp.rs:151-161`, `:474`); titles, symbols, headings and sheet names
  are escaped only.
- Facts the docs must state (verified in code):
  - Neither module puts `llm-index` on `PATH`. HM: add
    `config.services.llm-index.package` to `home.packages`. NixOS: add
    `config.services.llm-index.instances.<owner>.package` to
    `environment.systemPackages` (or the owner's `users.users.<owner>.packages`).
  - Config files: HM `$XDG_CONFIG_HOME/llm-index/config.toml` (CLI default);
    NixOS `/etc/llm-index/<owner>.toml`, so every CLI/MCP command needs
    `--config /etc/llm-index/<owner>.toml`. `--config` is a global flag.
  - Units: HM user unit `llm-index.service`; NixOS system unit
    `llm-index-<owner>.service`, runs as `<owner>`, `stateDir` under
    `/var/lib/`. One route per index.
  - HM `registerPublic` (default true) writes `programs.mcp.servers.llm-index-public`;
    that only helps clients whose HM module reads `programs.mcp`. The private
    server is never registered automatically.
  - CLI: `index`, `index --self-test`, `mcp --tier public|private`, `status`,
    global `--config`; `worker` is internal; `paths` and `eval-recall` hidden.
  - MCP tools `search`, `get` (with `offset`), `recent`, `status`; schemas
    from `tools/list`; defaults/clamps from `src/mcp.rs`.
  - A `public` root's paths, titles and passages reach every registered
    client and that client's model provider. Private tier: local-model
    clients only.
  - Chat model is independent; the embedding model is static, pinned,
    offline (model2vec potion-retrieval-32M, see `docs/feasibility.md`).
- Client snippets: Claude Code (`claude mcp add -s user …` and JSON form),
  Codex (`[mcp_servers.<name>]` in `~/.codex/config.toml`), Gemini CLI
  (`mcpServers` in `~/.gemini/settings.json`, also used by agy), plus a
  generic stdio section. Each checked against the vendor's current docs
  (Context7 first); if a detail cannot be confirmed, say so. No other client
  gets a snippet.
- Performance: quote p620 smoke-run numbers as "measured on p620 (smoke
  run)" beside the targets; reference-laptop row stays blank.
- Style: short sentences, task-first headings, every runnable command in a
  fenced block with a language tag, NixOS commands shown with their
  `--config`. No marketing language. Do not document nonexistent features
  (GPU, OCR, `.pptx`, remote embeddings, non-x86_64).

## Steps

1. **Facts file (me).** Build the package; capture into the scratchpad (not
   the repo): `llm-index --help` and each subcommand's `--help`; the MCP
   `tools/list` JSON via a small client modelled on `tests/mcp_client.py`
   (`initialize`, `notifications/initialized`, `tools/list`) against
   `llm-index mcp --tier public --config <scratch>` (reuse `Session.rpc("tools/list", {})`
   from `tests/mcp_client.py`); the option list (`nix eval` of the HM and
   NixOS module option names, descending into the `instances.<owner>` and
   `roots.<name>` submodules, with defaults). Hand the file path
   to the coder.
   → verify: files non-empty; four tools listed; submodule options present.
   Traps: `tools/list` needs no running indexer; do not touch real config.

2. **`docs/reference.md` + `share/AGENTS.snippet.md` (coder).** CLI section
   from the help output; MCP section with every parameter, type, default,
   limit (from `src/mcp.rs`), result fields, stale/offline marks, `get`
   continuation via `next`/`offset`, the fail-closed message "index
   configuration is being checked", what is wrapped vs escaped; supported
   file types and limits (from `src/extract/` and `src/config.rs`). Exit codes as defined in
   `src/main.rs` (around line 116). Snippet: add `offset` to `get` and widen the untrusted warning to every
   document-derived field (paths, titles, headings, symbols, sheet names),
   matching `src/mcp.rs:555`.
   → verify: every param in `tools/list` appears in both files.
   Traps: snippet is shipped in the package and `agentInstructions`; keep it
   short.

3. **`docs/installation.md` (coder).** Route choice; flake input; HM and
   NixOS examples; `PATH` per route; config locations; full option table for
   every option in the facts file (corrected defaults); permissions; network
   roots (user mounts the share; `kind`, `share`, `interval`, offline/
   fail-closed behaviour); state-dir and sync restrictions; Landlock;
   service and log commands; upgrading; uninstalling (units, state dir).
   → verify: option-name diff (step 7) empty.
   Traps: NixOS `stateDir` must be under `/var/lib/`; config assertions now
   reject non-normal paths, duplicate root paths, bad durations, workers < 2.

4. **`docs/agents.md` (coder).** Content per the decisions above, plus the
   agent snippet (`agentInstructions`) into `CLAUDE.md` / `AGENTS.md` /
   `GEMINI.md`, and a check command per client. Look up each vendor format
   with Context7 (`resolve-library-id` then `query-docs`); cite the source
   URL in the page.
   → verify: each snippet matches the looked-up docs; unknowns flagged.
   Traps: never show the private tier registered with a cloud client.

5. **`docs/architecture.md`, `docs/security.md`, `docs/troubleshooting.md`,
   `docs/development.md` (coder).**
   - Architecture: one mermaid diagram with coordinator (`index`), sandboxed
     workers (scan, extract, watch, validate), `public.db` and `private.db`,
     the two MCP servers and the clients. Then: indexing flow scan → extract
     → chunk → embed → store; watching (inotify for local roots, polling at
     `interval` for network/degraded); share scheduling (one live worker per
     network share, backoff up to `backoffMax`, hung share fails closed);
     ranking (SQLite FTS5 unicode61 + int8 512-dim vectors, hybrid); the
     static offline embedding model and why; crash containment (a parser
     crash fails only that file, one solo retry). Short; link #1's
     intent/spec/plan for depth.
   - Security: the two tiers and their isolation (public MCP never opens
     `private.db`); public-root content reaches clients and model
     providers; worker sandbox (Landlock read-only beneath the root and
     `/nix/store`, no TCP, RLIMIT_AS/CORE, `oom_score_adj`, systemd
     hardening); built-in denylist and `liftBuiltin` (private roots only);
     no symlink following, no crossing mounts; untrusted-content handling
     (wrap/escape, control/bidi stripping); not protected: residual parser
     aborts are contained not prevented, anyone with the user account can
     read the DBs.
   - Troubleshooting, symptom → cause → fix with a diagnose command, from
     real error strings (grep `src/`): refuses to start (no Landlock, state
     dir on network/FUSE or in a synced folder, config assertion at
     rebuild), self-test failure, root `config_error` / `offline` /
     `degraded`, "index configuration is being checked", a file not found
     (denylist, size/text/cell limit, unsupported type, `failed`), network
     share hangs, MCP client shows no tools (wrong `--config`, not on
     `PATH`).
   - Development: dev shell (`nix develop` and its env vars), `cargo test
     --all-features`, clippy, `nix flake check` with each check's purpose
     (from `flake.nix` and `nix/checks/`), running one VM test, the
     benchmark (link `docs/benchmark.md`), repo layout, the artifact
     workflow, and the rule that option/flag/tool changes update
     `docs/reference.md` / `docs/installation.md` in the same PR.
   → verify: every quoted error string greps in `src/`.
   Traps: link the intent/spec/plan of #1 for depth rather than copying.

6. **`README.md` (coder).** Landing page per the spec: pitch, what it is
   not, requirements, quick start HM and NixOS (with `PATH`), connect one
   agent (link `docs/agents.md`), verify it works, docs index, licence.
   → verify: ≤ ~150 lines; every link resolves.

7. **Verification (me).**
   - Option names: diff facts file vs `docs/installation.md`; then
     hand-check every documented default against `nix/config.nix` and the
     modules.
   - CLI help and `tools/list` vs `docs/reference.md` and the snippet; then
     hand-check MCP defaults and clamps not in the schema against
     `src/mcp.rs` (e.g. the search `limit` clamp near line 281).
   - Every command in `README.md` and `docs/installation.md` is either run
     below or listed in the PR as not executed, with the reason.
   - Scratch CLI run on p620: scratch config with one root `tier = "public"`
     (tiers default to private) over a copy of `tests/fixtures/tree`, temp
     state dir. Start `index` in the background, poll `status` with a
     timeout until queued is 0, run an MCP `search` for a known fixture
     term and expect its file, then stop `index` and delete the scratch
     dirs. No real services touched.
   - Throwaway NixOS VM test (scratch file under the scratchpad, `nix build`,
     not added to `checks`): the documented NixOS example, plus creating
     the owner account (`users.users.<owner>`; the module only sets
     `User=`); unit `llm-index-<owner>` starts; documented `systemctl`,
     `journalctl`, `llm-index --config /etc/llm-index/<owner>.toml status`
     and the `PATH` example work as the owner.
   - HM example: build its `activationPackage` with the pinned home-manager;
     inspect generated unit and config; not activated. HM `systemctl --user`
     commands are checked for unit name only, and the PR says activation
     was not tested.
   - Link and anchor check over `README.md` and `docs/`.
   - `nix flake check --no-build`.
   Announce on the agent bus before the VM run.
   → verify: all pass; fix docs for any mismatch.

8. **Review and PR (me).** Codex read-only review of the diff against the
   code; fix confirmed findings; open the PR linking intent, spec, plan and
   naming which steps the coder did.

## Tests

- Step 7 checks, all passing.
- `nix flake check --no-build` evaluates.

## Rollback

Revert the branch commits; no code, host or service is affected.
