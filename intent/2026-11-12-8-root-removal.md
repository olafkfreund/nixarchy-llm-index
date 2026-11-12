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
   only under `state_dir`, so the open fails with `EACCES`. The Home Manager
   and NixOS units have the same problem: neither sets a temp directory.
   SQLite can also need a temp file for other large statements, so the
   crash is not limited to removing a root.
2. **Disk space is not reclaimed.** After the old root's rows are deleted,
   `public.db` stays at its old size plus the new data (1.4 GB in the trial).
   Nothing runs `VACUUM`.
3. **The docs do not cover a changed root path.** Changing a root's `path`
   while keeping its name shows `offline` / `identity mismatch`.
   `docs/troubleshooting.md` explains that state only as "the wrong share is
   mounted". The fix, giving the root a new name, is not written down.

## Proposed outcome

- Removing or renaming a root, or editing the config while the service
  runs, leaves the service running. Old rows are gone and the new root is
  indexed, with no environment variable to set.
- Every temp file SQLite makes in the coordinator lives under `state_dir`.
  The coordinator still cannot write anywhere else.
- After a config change deletes rows, the database file shrinks.
- The troubleshooting page explains `identity mismatch` after a path change
  and how to fix it.
- A regression test fails on today's code and passes after the fix.

## Affected users and systems

Every install, on both the Home Manager and NixOS routes, the first time a
root is removed or renamed. Only the coordinator (`index`) is affected:
`mcp` and `status` open the databases read-only. Today only the trial on
p620 is running; nothing deployed yet.

## Constraints

- Do not widen the Landlock rules: temp files go under `state_dir`.
- No new dependencies and no schema change.
- `VACUUM` must not run on every start, only after rows were actually
  deleted by a config change.
- Existing tests and `nix flake check` stay green.

## Open questions

None. The owner pre-approved all changes needed (session, 2026-11-12).
