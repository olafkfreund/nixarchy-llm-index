---
status: draft
issue: 12
intent: intent/2026-11-13-12-worker-dumpable.md
---

# Spec: extract workers are not dumpable, and the last #10 gaps are closed

## Design

### 1. Extract workers turn off dumpability

`src/worker/sandbox.rs` `start`: right after the existing
`setrlimit(Resource::Core, 0)` (which already comes after the
`oom_score_adj` write and read-back), when `role` is `extract` (the
self-test's extract probe also arrives as `extract`: `worker::run` strips
the `selftest-` prefix before calling `selftest`), call
`rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)?`.
Every later step of `start` (open the root with `openat2`, Landlock) and
the extract loop use only same-process `/proc/self` paths or none, which
stay allowed.

Why only extract: extract workers parse hostile files and hold document
text, and they are the ones the test suite crashes on purpose. Scan,
watch and validate workers hold paths only; the watcher test reads a watch
child's `/proc/<pid>/fd` from the parent, which a non-dumpable child would
refuse.

### 2. The self-test checks it

`src/worker/mod.rs` `selftest`: after `sandbox::start`, read
`rustix::process::dumpable_behavior()`. Inside `selftest` the role is
already stripped, so compare against `extract` and `scan`: for `extract`
it must be `NotDumpable`, else the self-test fails with `extract worker
is dumpable`; for `scan` it must be `Dumpable`, else it fails with `scan
worker is not dumpable` (the role split is deliberate). `rustix` 1.1.6,
already locked, has `set_dumpable_behavior` and `dumpable_behavior`
(`src/process/prctl.rs`). `self_test_in` already runs both probes. Print `dumpable=<bool>` with the other probe lines. The
`index --self-test` output (`src/index/mod.rs` `self_test_in`, which
prints the child's stdout) then shows it.

### 3. Tests

- `src/worker/mod.rs` tests: a child (the existing `LLM_INDEX_SANDBOX_CHILD`
  pattern) runs `sandbox::start` with role `extract` and reports
  `dumpable_behavior()`; the parent asserts `NotDumpable`. The same with
  role `scan` asserts `Dumpable`. Fails on 332b56c for the extract case.
- `tests/selftest.rs`: also asserts a `dumpable=false` line (from the
  extract probe) and a `dumpable=true` line (from the scan probe).
- `src/worker/scan.rs` tests: drive `scan()` itself with a `Ready` built
  by hand (its fields are public): a real temp directory as the root fd,
  and a `mountinfo` file whose entry for the root path carries a mount id
  that is not the root fd's (assert the fixture parses to `Some` identity
  first, so the test really exercises the mismatch). Capture the messages
  by running `scan()` in a child process (the `current_exe` pattern with
  `--exact`, as the sandbox tests do; `Output` writes to stdout and stays
  unchanged) and parse its stdout; assert the
  `ScanDone` has `complete: false` and both identities `None`. Fails if
  `on_fd_mount` is removed from `scan()`.
- The initial `root_mount_id` error is not tested: `statx(fd, "",
  AT_EMPTY_PATH, STATX_MNT_ID)` on a local directory fd does not fail
  without kernel or FUSE fault injection (it can fail on NFS or FUSE with
  `ESTALE`/`EIO`, which is why the branch exists), and an invalid
  `OwnedFd` is undefined behaviour. A one-line comment at that branch says
  so.

### 4. Docs

`README.md` (both examples, lines ~43 and ~69) and `docs/installation.md`
(examples ~56 and ~95): one sentence after each example block, the first
time per file: the root path must be the real path, no symlink anywhere
in it; check with `realpath ~/Source` and use what it prints.
`docs/security.md` sandbox section: extract workers are not dumpable, so
their crashes are not passed to the core-dump helper and same-user
unprivileged processes cannot `ptrace` them.

## Alternatives rejected

- **All worker roles non-dumpable.** Breaks the watcher test's
  cross-process `/proc/<pid>/fd` reads and hides real crashes in scan and
  watch workers, which hold no document text.
- **Turn off dumpability only in tests (an environment variable).** A
  test-only switch in production code, and production would keep invoking
  the helper on every hostile file.
- **A seam to make `root_mount_id` fail in a test.** Test-only code in the
  scan path for a branch a supported kernel never takes.

## Risks

- A real extract-worker crash is no longer in `coredumpctl`; the file
  still shows `worker died` in the index and the journal. Accepted in the
  intent.
- If `set_dumpable_behavior` fails, `start` fails and the worker exits;
  the coordinator records the error as for any start failure. `prctl` with
  `PR_SET_DUMPABLE` does not fail for a valid argument.
- Host: p620 only.

## Verification

- `cargo test --all-targets`, `cargo clippy --all-targets -- -D
  warnings`, `cargo fmt --check` in the dev shell.
- `nix flake check` (VM tests run the real self-test and parser crash
  tests).
- Run `cargo test` once and count `journalctl --since` lines for
  `llm[-_]index.*terminated abnormally` (test binaries are named
  `llm_index-<hash>`): none from extract workers; any left
  are traced to their role and listed in the PR.
- p620 after deploy: the self-test journal lines include `dumpable=false`,
  both roots end `ok`, and a public search from Claude, Codex and agy
  still returns results.
