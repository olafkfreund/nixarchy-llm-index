---
status: draft
issue: 12
author: olafkfreund
---

# Intent: workers are not dumpable, and the last #10 gaps are closed

## Problem

1. **Every worker crash reaches systemd-coredump.** Extract and scan
   workers set `RLIMIT_CORE=0` (`src/worker/sandbox.rs` `start`), so no core
   file is written. But with a piped core pattern (systemd-coredump) the
   kernel still runs the helper, which logs `terminated abnormally without
   generating a coredump` and sends a desktop notification. Workers crash
   on purpose in the test suite (out of memory under the 4 GiB
   address-space limit, the parser crash tests), so one `cargo test` run
   makes several notifications; on 2026-11-13 about 60 of them looked like
   the service was crashing. A hostile file does the same in production.
   A worker is also open to `ptrace` from any process running as the
   owner while it holds document text in memory.
2. **The examples do not say a root path must be real.** README and
   `docs/installation.md` show `~/Source` as a root path. On p620 that
   path is a symlink, and a root path with a symlink component is refused
   (`config_error: root path contains a symlink`). The rule is only in the
   option table.
3. **One #10 test gap.** `scan()` sends no identity when the root's mount
   id cannot be read (`src/worker/scan.rs`). Only the helper it uses is
   tested.

## Proposed outcome

- A worker that crashes produces no systemd-coredump entry and no
  notification; `cargo test` runs leave the journal free of
  `terminated abnormally` lines for `llm-index`. The coordinator still
  records the file as `worker died`, as today.
- Workers cannot be `ptrace`d by other processes of the same user. The
  self-test checks it and fails if a worker is dumpable.
- The examples say the root path must be the real path, and how to find
  it (`realpath`).
- Item 3 either gets a `scan()`-level test or is documented as untestable
  without a test-only seam, with the reason.

## Affected users and systems

- Every install; p620 is the only deployment. Only workers change; the
  coordinator, `mcp` and `status` stay dumpable so a real crash there is
  still reported.
- nixos_config: a flake input bump to deploy it.

## Constraints

- The sandbox may only get stricter.
- `oom_score_adj` must still be written and read back (a non-dumpable
  process cannot write its own `/proc/self` files), and `/proc/self/fd`
  paths must keep working (watcher, self-test socket).
- No new dependency; `rustix` already has `set_dumpable_behavior`.

## Open questions

- A crashing worker is no longer visible in `coredumpctl`. The reason is
  still in the index (`worker died`) and in the service journal. Proposed:
  accept; a parser crash is an expected, contained event here.
