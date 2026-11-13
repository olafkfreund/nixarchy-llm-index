---
status: draft
issue: 12
spec: spec/2026-11-13-12-worker-dumpable.md
coders:
  1: codex   # small, security-sensitive change in the sandbox and self-test
  2: coder   # one self-contained test in scan.rs with a child-process harness
  3: agy     # mechanical docs edits in three files
reviewers:
  - codex    # independent read of the diff against this plan
  - agy      # second read; checks the docs wording too
---

# Plan: extract workers are not dumpable, and the last #10 gaps are closed

Self-contained summary of the approved spec. Repo
`/mnt/code/Source-home/GitHub/nixarchy-llm-index`, branch
`fix/12-worker-dumpable`. The three steps touch disjoint files and run in
parallel, each in its own worktree (`../llm-index-12-sN`), cherry-picked
by the lead in order 1, 2, 3.

Check for every step, inside the dev shell (it sets the pdfium and model
paths):
`nix develop -c bash -c 'CARGO_TARGET_DIR=target-sN cargo test
--all-targets && CARGO_TARGET_DIR=target-sN cargo clippy --all-targets --
-D warnings && cargo fmt --check'`.

Traps for every step: no new dependency; no test-only code paths in
production; each behaviour change has a test that fails on 332b56c.

## Steps

### 1. Extract workers non-dumpable, self-test checks it

Files: `src/worker/sandbox.rs`, `src/worker/mod.rs`, `tests/selftest.rs`.

- `sandbox::start`: directly after `setrlimit(Resource::Core, 0)` (after
  the `oom_score_adj` write and read-back, which must stay first), when
  `role == "extract"`, call
  `rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)?`.
  Only `extract`: the self-test's extract probe also arrives as `extract`
  because `worker::run` strips `selftest-`. Scan, watch and validate stay
  dumpable (the watcher test reads a watch child's `/proc/<pid>/fd`).
- `selftest` (worker/mod.rs): after `sandbox::start`, read
  `rustix::process::dumpable_behavior()?`. Role `extract`: must be
  `NotDumpable`, else error `extract worker is dumpable`. Role `scan`:
  must be `Dumpable`, else error `scan worker is not dumpable`. Print
  `dumpable=<bool>` next to the existing probe lines.
- Tests: in `src/worker/mod.rs`, a child (existing
  `LLM_INDEX_SANDBOX_CHILD` pattern) runs `sandbox::start` with role
  `extract` and prints `dumpable_behavior()`; parent asserts
  `NotDumpable`. Same with role `scan`: `Dumpable`. `tests/selftest.rs`
  also asserts a `dumpable=false` and a `dumpable=true` line.
- `docs/security.md` is step 3's.

### 2. A `scan()`-level identity test

File: `src/worker/scan.rs` only.

- A test that runs `scan()` in a child process (`current_exe` with
  `--exact` and an env var, like the sandbox tests; `Output` writes to
  stdout and stays unchanged). The child builds `Ready` by hand (fields
  are public; `scan.rs` ~756 already does): config with one root at a temp
  directory, `root: Some(0)`, the directory opened as `root_fd`, a
  `mountinfo` `File` (a temp file) whose line for the root's path parses
  to `Some` identity but carries a mount id that is not the root fd's,
  `Gitignore::empty()`, `oom_score_adj: String::new()`. Assert first, in
  the child, that the fixture parses to `Some` identity. The parent finds
  the `ScanDone` line in stdout and asserts `complete: false`,
  `identity_before: null`, `identity_after: null`.
- A one-line comment at the initial `root_mount_id` error branch: not
  tested, because `statx` on a local directory fd does not fail without
  kernel or FUSE fault injection (NFS/FUSE can give `ESTALE`/`EIO`, the
  reason the branch exists).

### 3. Docs

Files: `README.md`, `docs/installation.md`, `docs/security.md`.

- README examples (~43 and ~69) and `docs/installation.md` examples (~56
  and ~95): after the first example block in each file, one sentence: the
  root path must be the real path with no symlink anywhere in it; check
  with `realpath ~/Source` and use what it prints.
- `docs/security.md` sandbox section: extract workers are not dumpable, so
  their crashes are not passed to the core-dump helper (no systemd-coredump
  entry) and unprivileged processes of the same user cannot `ptrace` them;
  other workers stay dumpable. The self-test checks both.
- Check: `npx --yes markdownlint-cli2 README.md docs/*.md` reports 0
  issues, plus the cargo check.

## Tests

- Each step's check passes in its worktree and again on the branch after
  the cherry-picks.
- `nix flake check` on the branch.
- Run `cargo test --all-targets` once on the branch and count
  `journalctl --since <start> | grep -E 'llm[-_]index.*terminated
  abnormally'`: none from extract workers; any left are traced to their
  role and listed in the PR.
- Final reviews: Codex and agy read `git diff main..HEAD` against this
  plan.
- p620 after deploy: the self-test journal lines include `dumpable=false`
  and `dumpable=true`, both roots end `ok`, a public search from Claude,
  Codex and agy returns results.

## Rollback

Revert the merge on `main`, bump the nixos_config input back to 332b56c and
deploy p620. No state changes.
