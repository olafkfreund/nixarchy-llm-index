---
status: draft
issue: 12
author: olafkfreund
---

# Intent: extract workers are not dumpable, and the last #10 gaps are closed

## Problem

1. **Every extract-worker crash reaches systemd-coredump.** Workers set
   `RLIMIT_CORE=0` in `src/worker/sandbox.rs` `start`, but with a piped
   core pattern the kernel ignores that limit and still runs the helper.
   Whether a core is stored is then the collector's choice. On p620
   systemd-coredump logs `terminated abnormally without generating a
   coredump` and a desktop notification follows. Extract workers crash on
   purpose in the test suite (out of memory under the 4 GiB address-space
   limit, the parser crash tests), so one `cargo test` run makes several
   notifications; on 2026-11-13 about 60 of them looked like the service
   was crashing. A hostile file does the same in production.
   An extract worker also holds document text in memory, and today an
   unprivileged process of the same user may be allowed to `ptrace` it,
   subject to Yama.
2. **The examples do not say a root path must be real.** README and
   `docs/installation.md` show `~/Source` as a root path. On p620 that path
   is a symlink, and a root path with a symlink component is refused
   (`config_error: root path contains a symlink`). The rule is only in the
   option table.
3. **One #10 test gap.** `scan()` (`src/worker/scan.rs`) has two ways to
   report no identity: the initial `root_mount_id` error (an early return
   with both identities `None`), and a mountinfo entry whose mount id is
   not the root fd's (`on_fd_mount`). Only the helper is tested; no test
   drives `scan()` itself.

## Proposed outcome

- After its sandbox is set up, a crashing extract worker never invokes the
  core-dump helper: no systemd-coredump entry and no notification. A
  `cargo test` run leaves no `terminated abnormally` lines for
  `llm-index` extract workers in the journal. The coordinator still
  records the file as `worker died`, as today.
- Unprivileged processes of the same user can no longer attach to an
  extract worker with `ptrace` (privileged ones with `CAP_SYS_PTRACE`
  still can). The self-test checks that the extract worker is not dumpable
  and fails otherwise.
- Scan, watch and validate workers, the coordinator, `mcp` and `status`
  stay dumpable: they hold paths, not document text, and a crash there is
  a real bug that should stay visible in `coredumpctl`.
- The examples say the root path must be the real path, and show how to
  find it (`realpath`).
- `scan()` gets a test that drives it with a mountinfo whose entry is not
  on the root fd's mount and checks that no identity is sent. The initial
  `root_mount_id` error is documented as not reachable in a test without a
  test-only seam: `statx` on a valid directory fd does not fail.

## Affected users and systems

- Every install; p620 is the only deployment. Only the extract role
  changes behaviour.
- nixos_config: a flake input bump to deploy it.

## Constraints

- The sandbox may only get stricter.
- The `oom_score_adj` write must happen before dumpability is turned off
  (afterwards `/proc/self/oom_score_adj` is owned by root and the write is
  refused; reading it still works). Same-process `/proc/self/fd` paths keep
  working. Dumpability resets on `execve`, so it is set inside the worker
  after its own exec; extract workers do not exec anything.
- No new dependency; `rustix` already has `set_dumpable_behavior`.
- Existing tests that inspect a child's `/proc/<pid>/fd` from the parent
  (the watcher test) must keep working; they use watch workers, which do
  not change.

## Open questions

- A crashing extract worker is no longer listed by `coredumpctl`. The file
  is still recorded as `worker died` in the index and the service journal.
  Proposed: accept; a parser crash on a hostile file is an expected,
  contained event.
