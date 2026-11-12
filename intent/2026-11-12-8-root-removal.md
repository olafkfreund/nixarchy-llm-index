---
status: draft
issue: 8
author: olafkfreund
---

# Intent: the indexer survives a root being removed or renamed

## Problem

Found in the real-life trial on p620 (2026-11-12):

1. **The coordinator dies and cannot start again.** After a root is removed
   or renamed in the config, `llm-index index` exits at start with
   `unable to open database file` (exit status 2) on every start. SQLite
   needs a temporary file to delete the old root's rows (113k chunks in the
   trial). It tries to create that file in `/var/tmp`. The coordinator's
   Landlock rule (`src/index/mod.rs` `restrict_coordinator`) allows writes
   only under `state_dir` (and `/proc`), so the open fails with `EACCES`.
   Evidence: an strace of the failing start shows
   `openat("/var/tmp/etilqs_…", O_CREAT…) = -1 EACCES`, and the same start
   succeeds with `SQLITE_TMPDIR=<state_dir>`. The Home Manager
   and NixOS units have the same problem: neither sets a temp directory.
   SQLite can also need a temp file for other large statements, so the
   crash is not limited to removing a root.
2. **Disk space is not reclaimed.** After the old root's rows are deleted,
   `public.db` stays at its old size plus the new data (1.4 GB in the trial).
   Nothing runs `VACUUM`.
3. **The docs do not cover a changed root path.** Changing a root's `path`
   while keeping its name shows `offline` / `identity mismatch`. This is
   deliberate: the changed path clears the recorded identity, and
   `src/index/shares.rs` `verdict` adopts a new one only for a root with no
   indexed files or with `share` set, so a missing mount cannot wipe a root.
   `docs/troubleshooting.md` explains that state only as "the wrong share is
   mounted". The fix, giving the root a new name, is not written down.

## Proposed outcome

- After a root is removed or renamed, the service starts and stays up.
  Today a root change makes the coordinator exit on purpose so that systemd
  restarts it; a clean restart meets this outcome. Old rows are gone and the
  new root is indexed, with no environment variable to set.
- Every temp file SQLite makes in the coordinator lives under `state_dir`.
  The coordinator still cannot write anywhere else.
- After a config change deletes rows, the space is given back: once MCP
  readers release their snapshots and the WAL is checkpointed, the database
  files are smaller. Compacting is best effort. If it fails (for example,
  not enough free disk), the deletions stay committed, the error is logged
  and indexing goes on. It is tried again at the next config change.
- The troubleshooting page explains `identity mismatch` after a path change
  and how to fix it.
- A regression test runs the real coordinator, with its Landlock rule, as a
  subprocess. Removing a root there forces SQLite to write a temp file. The
  test fails on today's code and passes after the fix.

## Affected users and systems

Every install, on both the Home Manager and NixOS routes, the first time a
root is removed or renamed. Only the coordinator (`index`) is affected:
`mcp` and `status` open the databases read-only. Today only the trial on
p620 is running; nothing deployed yet.

## Constraints

- Do not widen the Landlock rules: temp files go under `state_dir`.
- No new dependencies and no schema change.
- Keep the path-change protection. The docs say to rename the root or set
  `share`, and still warn that the mismatch can also mean the wrong mount.
- `VACUUM` must not run on every start, or after the purge that follows
  each complete scan. It runs only after a config change (at start or
  reload) actually deleted rows.
- Do not keep temp files in memory: the 1 GB memory cap is smaller than the
  databases.
- MCP reads keep working while it compacts, with no journal-mode change.
- Existing tests and `nix flake check` stay green.

## Open questions

None. The owner pre-approved all changes needed (session, 2026-11-12).
