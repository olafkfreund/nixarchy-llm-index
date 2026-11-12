---
status: draft
issue: 8
intent: intent/2026-11-12-8-root-removal.md
---

# Spec: the indexer survives a root being removed or renamed

## Design

### 1. SQLite temp files go under `state_dir`

In `src/index/mod.rs`, add one helper, `sqlite_tmpdir(state_dir)`. It sets
`SQLITE_TMPDIR` to `state_dir` with `std::env::set_var` (an `unsafe` block
in edition 2024; a `// SAFETY:` comment says the process is still single
threaded, because no thread or worker has started yet). Call it in `run`
right after `statedir::prepare` and before `restrict_coordinator`, and in the
self-test path before its `restrict_coordinator`. Both calls come before any
SQLite connection is opened. That matters: SQLite's unix VFS reads
`SQLITE_TMPDIR` once, in `sqlite3_os_init`, the first time SQLite is used.

- The value is set unconditionally and overrides any inherited
  `SQLITE_TMPDIR` or `TMPDIR`. `state_dir` is the only writable place under
  the coordinator's Landlock rule. It is mode 0700, so temp files holding
  index data also stay private. In `/var/tmp` they would sit in a directory
  everyone can write to.
- Workers inherit the variable. They do not use SQLite, so it has no
  effect there.
- SQLite opens a temp file and unlinks it at once, so nothing is left behind
  in `state_dir`.
- Landlock is unchanged.
- The units in `nix/config.nix` need no change: the binary sets the
  variable itself.

Rejected `PRAGMA temp_store = MEMORY`: `VACUUM` and a large delete can need
temp space as large as the database (1.4 GB in the trial), more than the
1 GB memory cap.

### 2. Compact after a config change deletes rows

- `DbSink::purge` (`src/index/store.rs` around line 380) changes to return
  `Res<Vec<Tier>>`: the tiers where it deleted or moved out at least one
  row. The call after a complete scan (around line 568) ignores the value,
  so compaction never follows a scan.
- `open_with` (start) and `reload` (config change) pass the returned tiers
  to a new `compact(&[Tier])`. For each tier it runs `VACUUM` and then
  `PRAGMA wal_checkpoint(TRUNCATE)`.
- **Best effort.** Any error, for example a full disk, is logged with
  `eprintln!` (`store: compact <tier>: <error>`) and ignored. The deletions
  were committed in their own transactions before, so they stay.
- `VACUUM` runs only outside a transaction, which holds at these call sites.
  `wal_checkpoint` returns `busy` while an MCP reader holds a snapshot. The
  WAL is then truncated at the next checkpoint, so the disk space comes back
  later rather than at once, which is the eventual reclamation the intent
  asks for.
- MCP readers keep reading in WAL mode while `VACUUM` writes.
- There is no journal-mode change and no `auto_vacuum` (that would need a
  schema-level `VACUUM` on every existing database).
- A failed compaction is retried at the next config change that deletes
  rows. There is no retry loop.

### 3. Docs

- `docs/troubleshooting.md`:
  - The `identity mismatch` row in the root-state table (around line 92)
    gets a second cause: the root's `path` changed while its name stayed
    and it already had indexed files. That protection is deliberate (a
    missing mount must not look like an empty root). The fix is to give the
    root a new `name` (its old rows are dropped and it is re-indexed), or to
    set `share` to the mount source. The row keeps saying it can also mean
    the wrong share is mounted.
  - The network-share section (around line 159) gets the same pointer.
  - A new row covers `unable to open database file` at start: on versions
    before this fix, the workaround is `SQLITE_TMPDIR=<state_dir>`.
- `docs/installation.md`, "State directory" section: SQLite temp files live
  there. Removing a root compacts the database once, which needs free space
  up to twice the database size.

## Alternatives rejected

- **Allow writes to `/var/tmp` or `/tmp` in Landlock.** That widens the
  sandbox and puts index data in a shared directory.
- **`temp_store = MEMORY`.** Temp space can exceed the memory cap (see
  above).
- **Set `SQLITE_TMPDIR` only in the Nix units.** A manual run, `--self-test`
  or another route would still crash. The binary owns its sandbox, so it
  should own the temp location.
- **`sqlite3_temp_directory` through FFI.** It is also `unsafe` and also
  global, and it needs a dependency on `libsqlite3-sys` symbols. The
  environment variable is the documented interface.
- **`VACUUM` after every purge, or on a timer.** That is costly on large
  databases and would break the intent's constraint.
- **`auto_vacuum = INCREMENTAL`.** Turning it on for an existing database
  needs a full `VACUUM` anyway, and it fragments the file.

## Risks

- **A long startup after removing a large root.** `VACUUM` of a 1.4 GB
  database blocks the coordinator for tens of seconds. It is logged, and it
  happens only on a config change.
- **Disk use while compacting.** The temp copy and the WAL can briefly need
  up to twice the database size in free space. On failure the database
  stays valid and keeps its size.
- **`set_var` safety** holds only while the call stays before the first
  thread. The `// SAFETY:` comment states this, and the call sits at the
  top of `run`.
- **Hosts.** Only the trial on p620 runs today. Rollback is a revert.

## Verification

1. New subprocess test in `tests/e2e.rs`,
   `removing_a_root_restarts_cleanly_and_compacts`:
   - Two public roots, `a` and `b`, with a few files each. Run
     `llm-index index` (the real coordinator with its Landlock rule) until
     both are indexed, then kill it.
   - Rewrite the config without `b` and start it again. `SQLITE_TMPDIR` and
     `TMPDIR` are removed from the child's environment, so the test does not
     depend on the caller's settings.
   - The process is still running after the rows of `b` are gone (polled
     through a read-only `Store`, with a time limit).
   - `a` is still searchable.
   - `PRAGMA freelist_count` on `public.db` is 0, which shows `VACUUM` ran.
   - The test must fail when the `sqlite_tmpdir` call is removed: VACUUM
     then needs a temp file outside `state_dir` and the coordinator exits.
     The plan step proves this by running the test once with the call
     commented out.
2. Unit test in `src/index/store.rs`:
   - `purge` returns the tier when rows are deleted, and an empty list when
     nothing changed;
   - `reload` that drops a root leaves `freelist_count` at 0;
   - a purge after a complete scan does not compact (the scan path ignores
     the result).
3. `cargo test --all-features`, `cargo clippy --all-targets --all-features
   -- -D warnings`, full `nix flake check` (the e2e test then also runs in
   the Nix build sandbox, where `TMPDIR=/build` is outside `state_dir`).
4. Real check on the p620 trial: remove a root from the trial config,
   restart without the `SQLITE_TMPDIR` workaround, and confirm the service
   stays up and `public.db` shrinks.
5. Codex review of the diff, read-only.
