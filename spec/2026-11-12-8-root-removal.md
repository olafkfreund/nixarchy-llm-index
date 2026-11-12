---
status: approved
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
SQLite connection is opened, any worker is spawned and the embedder is loaded.
(`unixTempFileDir()` in SQLite's unix VFS reads the variable each time it picks a
temp directory; setting it this early avoids depending on that detail.)

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

### 2. Compact when a database is mostly free pages

- New `DbSink::compact(&self)` in `src/index/store.rs`. It is called at the
  end of `open_with` (after `purge`) and of `reload` (after `purge`), and
  nowhere else, so it never runs after a scan.
- For each tier it reads `PRAGMA page_count` and `PRAGMA freelist_count`.
  It compacts only when `freelist_count * 4 > page_count`, that is when more
  than a quarter of the file is free. This one rule:
  - catches every large deletion, including the removed-root rows that
    `prune` deletes (`store.rs` around line 358), with no change to
    `prune`, `purge` or their callers;
  - stays off on a normal start, because ordinary churn reuses free pages;
  - retries by itself: if a compaction failed, the free pages are still
    there at the next start or reload.
- Compacting runs on the writer connection:
  1. `set_limit(SQLITE_LIMIT_ATTACHED, 1)`. `src/db.rs:82` sets it to 0 for
     every connection, and `VACUUM` attaches its temp database internally,
     so it fails with "too many attached databases - max 0" under 0. Only
     this function raises it, with no other SQL in between.
  2. `VACUUM`.
  3. `set_limit(SQLITE_LIMIT_ATTACHED, 0)`, always, even when `VACUUM`
     failed.
  4. `PRAGMA wal_checkpoint(TRUNCATE)`. Its result row `(busy, log,
     checkpointed)` is read. `busy = 1` is logged as deferred.
- Logging: `store: compacted <tier>: <before> -> <after> pages` on success,
  and `store: compact <tier>: <error>` on failure. Any failure is ignored.
  The deletions were committed in their own transactions earlier, so they
  stay.
- **Eventual WAL reclamation.** Writer connections get
  `PRAGMA journal_size_limit = 67108864` (64 MiB) in `src/db.rs` `open`
  (writer branch). An automatic checkpoint that resets the WAL then
  truncates it to that size. So if the `TRUNCATE` checkpoint was busy
  because an MCP reader held a snapshot, the WAL still shrinks later.
- MCP readers keep reading in WAL mode while `VACUUM` writes.
- There is no journal-mode change and no `auto_vacuum`. `VACUUM` runs
  outside any transaction, which holds at both call sites: `prune`,
  `delete_files` and `move_tier` commit before they return.

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
- **Return the changed tiers out of `prune` and `purge`.** It needs more code
  than the free-page rule, and it forgets a failed compaction.
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
   - Two public roots, `a` (a few files) and `b` (about 400 generated
     text files of about 10 KB each). That makes `public.db` several MB, well
     above SQLite's default 2 MB page cache, so `VACUUM` has to spill its
     temp database to disk.
   - Run `llm-index index` (the real coordinator with its Landlock rule)
     until both are indexed, record `page_count` of `public.db`, and kill
     it.
   - Rewrite the config without `b` and start it again, with its stderr
     sent to a file. Only `SQLITE_TMPDIR` is removed from the child's
     environment. `TMPDIR` is inherited, so under `nix flake check` the
     child sees `TMPDIR=/build`, outside `state_dir`; on a host without
     `TMPDIR` it falls back to `/var/tmp`. Both are outside the Landlock
     rule.
   - Wait, with a time limit, for `store: compacted public` or
     `store: compact public:` in that file, then assert:
     - it is `compacted`;
     - the process is still running;
     - `b`'s rows are gone;
     - `a` is still searchable;
     - `page_count` is below the recorded baseline and `freelist_count` is
       0.
   - **Proof that it guards the fix:** the plan step runs the test once with
     the `sqlite_tmpdir` call commented out and records that it fails, with
     a compact error or the coordinator exiting.
2. Unit tests in `src/index/store.rs` (no Landlock in unit tests):
   - `reload` that drops a large root leaves `freelist_count` at 0 and
     restores `SQLITE_LIMIT_ATTACHED` to 0, so `ATTACH` still fails;
   - a database below the threshold is not compacted (`page_count`
     unchanged).
3. `cargo test --all-features`, `cargo clippy --all-targets --all-features
   -- -D warnings`, full `nix flake check` (the e2e test then also runs in
   the Nix build sandbox, where `TMPDIR=/build` is outside `state_dir`).
4. Real check on the p620 trial: remove a root from the trial config,
   restart without the `SQLITE_TMPDIR` workaround, and confirm the service
   stays up and `public.db` shrinks.
5. Codex review of the diff, read-only.
