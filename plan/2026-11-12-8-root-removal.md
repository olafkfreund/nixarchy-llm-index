---
status: draft
issue: 8
spec: spec/2026-11-12-8-root-removal.md
coders:
  1: coder  # small, but in the startup path; Sonnet with an exact recipe
  2: coder  # one pragma
  3: coder  # compaction logic and unit tests, exact recipe below
  4: coder  # subprocess e2e test following the existing harness
  5: coder  # docs rows
reviewers:
  - codex   # read-only diff review; agy is out of use by owner decision (#2748)
---

# Plan: the indexer survives a root being removed or renamed

## Summary of approved decisions

- **Temp files.** The coordinator sets `SQLITE_TMPDIR=<state_dir>` itself,
  before any SQLite use, thread or worker. It is set unconditionally and
  overrides any inherited value. Landlock and the Nix units do not change.
  `temp_store = MEMORY` is rejected (the 1 GB cap).
- **Compaction.** `DbSink::compact()` runs at the end of `open_with` and
  `reload` only. For each tier it compacts when
  `freelist_count * 4 > page_count`:
  1. raise `SQLITE_LIMIT_ATTACHED` to 1 (`VACUUM` attaches internally);
  2. `VACUUM`;
  3. reset the limit to 0, always;
  4. `PRAGMA wal_checkpoint(TRUNCATE)`, reading the `(busy, log,
     checkpointed)` row.

  Logging and errors:
  - success logs `store: compacted <tier>: <before> -> <after> pages`;
  - failure logs `store: compact <tier>: <error>` and is otherwise ignored;
  - `busy = 1` logs `store: compact <tier>: WAL truncation deferred`.
- **Eventual WAL shrink.** Writer connections set
  `PRAGMA journal_size_limit = 67108864`.
- **Docs.** `identity mismatch` after a path change: rename the root or set
  `share`; it can still mean the wrong mount. Add a row for
  `unable to open database file` (workaround `SQLITE_TMPDIR`). Note temp
  files and compaction space (up to twice the database) in "State
  directory".

## Steps

1. `src/index/mod.rs`: add, next to `restrict_coordinator` (around
   line 333):

   ```rust
   /// SQLite's temp files (large deletes, VACUUM) must land where Landlock lets us write.
   fn sqlite_tmpdir(state_dir: &Path) {
       // SAFETY: called before any thread, worker or SQLite connection exists.
       unsafe { std::env::set_var("SQLITE_TMPDIR", state_dir) };
   }
   ```

   Call `sqlite_tmpdir(&config.state_dir);` in `run` on the line after
   `statedir::prepare(&config.state_dir)?;` (around line 405). In the
   self-test path, call it on the line before its
   `restrict_coordinator(&config.state_dir)?;` (around line 489).
   → verify by `cargo build`.

   Traps:
   - Do not move `restrict_coordinator`.
   - Do not touch the worker paths.
   - Edition 2024 needs the `unsafe` block.
   - Check with grep that nothing before these lines opens a `Store` or
     spawns.

2. `src/db.rs` `open`, writer branch (around line 55): after
   `conn.pragma_update(None, "journal_mode", "WAL")?;` add
   `conn.pragma_update(None, "journal_size_limit", 67_108_864)?; // WAL shrinks back after big writes`.
   → verify by `cargo test --lib db`.

   Traps:
   - Readers stay unchanged: they are `query_only`, and the pragma does
     not matter there.

3. `src/index/store.rs`: add `pub fn compact(&self)` on `DbSink`. It
   returns nothing: errors are logged, never returned.
   - For `t in TIERS`, with `c = self.conn(t)`:
     - read `page_count` and `freelist_count` with `query_row`; on error,
       log and `continue`;
     - if `free * 4 <= pages`, `continue`;
     - `c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 1)`, then
       `let r = c.execute_batch("VACUUM");`, then always
       `c.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)`;
     - on `Err`, log `store: compact {label}: {e}` and continue;
     - otherwise run `PRAGMA wal_checkpoint(TRUNCATE)` with `query_row`,
       reading three `i64`s. If `busy == 1`, log
       `store: compact {label}: WAL truncation deferred`;
     - read `page_count` again and log
       `store: compacted {label}: {pages} -> {after} pages`.
   - Use the existing `label(t)` helper for the tier name.
   - Call `s.compact();` in `open_with` after `s.purge()?;`, and in
     `reload` replace the trailing `self.purge()` with
     `self.purge()?; self.compact(); Ok(())`.
   - Unit tests in the existing `mod tests`, using the `fx` fixture:
     - (a) Insert about 300 extracted files into root `pub` with
       `f.sink.extracted`, using passages of about 4 KB each so the
       database has many pages. Then reload with a config that drops
       `pub` but keeps another root. Assert `freelist_count == 0` and that
       `ATTACH ':memory:' AS x` fails on `f.sink.conn(Tier::Public)`.
     - (b) A fresh fixture with one small file: call `compact()` and
       assert `page_count` is unchanged.

   → verify by `cargo test --lib store`.

   Traps:
   - `Limit` comes from `rusqlite::limits::Limit`, as in `src/db.rs`.
   - Do not call `compact` from the scan path (around line 568).
   - Keep the existing `purge` signature.
4. `tests/e2e.rs`: new `#[test] fn removing_a_root_restarts_cleanly_and_compacts()`.
   Use the existing harness style (temp base dir with `process::id()`,
   `Store::open` polling, `count`, `query`/`search`).
   - Root `a` holds `alpha.md` containing "marmalade".
   - Root `b` holds 400 files `b/n{i}.md`, each about 10 KB of varied
     words. Use `format!` with `i` in every line, so the files are not
     identical.
   - Run 1: `llm-index index` with both roots. Poll until `files` holds at
     least 401 rows and `validated` has at least 1 row (time limit 180 s).
     Record `page_count` of `public.db` (read-only `Store`), then kill and
     wait.
   - Rewrite the config with only `a`.
   - Run 2: same binary, `.env_remove("SQLITE_TMPDIR")`, stderr to
     `base/run2.log` (`Stdio::from(File::create(..))`).
   - Poll the log (time limit 180 s) until it contains
     `store: compacted public` or `store: compact public:`.
   - Then assert:
     - the log contains `store: compacted public`;
     - `child.try_wait()` is `Ok(None)`;
     - no `files` row has `root_id = 'b'`;
     - `search` for "marmalade" returns `alpha.md`;
     - `page_count` is below the baseline;
     - `freelist_count` is 0.
   - Kill the child and remove the base dir.

   → verify by `cargo test --test e2e removing_a_root`.

   Traps:
   - Do not remove `TMPDIR` from the child (the Nix sandbox case needs it).
   - Copy the config format from the existing test.
5. Docs:
   - `docs/troubleshooting.md`:
     - The `identity mismatch` row (around line 92) gets the second cause
       and fix from the summary.
     - The network-share section (around line 159) gets a one-line
       pointer.
     - In "The service does not start" (around line 20), add
       `unable to open database file` at start: fixed in this version;
       on older versions, set `SQLITE_TMPDIR=<state_dir>` for the
       service.
   - `docs/installation.md` "State directory" (around line 250): one
     sentence each on temp files and on compaction space.

   → verify by reading the rendered tables; no broken table pipes.
6. Mine (not handed off): run the step 4 test once with the
   `sqlite_tmpdir` call in `run` commented out. Record that it fails, then
   restore the call. Then run:
   - `cargo test --all-features`;
   - `cargo clippy --all-targets --all-features -- -D warnings`;
   - `nix flake check`.

   After that, Codex review of the diff, the PR, and merge.
7. Mine: the real check on the p620 trial.
   - Build the merged package.
   - Restart `llm-index-trial` without `--setenv=SQLITE_TMPDIR`.
   - Remove a root from the trial config. The private `docs` root is a
     good candidate; add it back afterwards.
   - Confirm the unit stays active, `store: compacted` is in the journal,
     and the database shrinks.

## Tests

- `cargo test --all-features`: all pass, including the new e2e test and the
  two unit tests.
- With the fix removed, the new e2e test fails (step 6).
- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `nix flake check`: green (the e2e test also runs under `TMPDIR=/build`).

## Rollback

Revert the merge commit. No schema or data format change. A database that
was already compacted stays valid.
