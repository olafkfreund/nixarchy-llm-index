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
     - Test (a) also asserts that `page_count` dropped.
     - (b) Insert about 100 files into `pub` and 5 small files into a
       second root `pub2`. Reload without `pub2`, which leaves a few free
       pages, below the threshold. Assert `freelist_count > 0` and that it
       is unchanged after reload, which proves compaction was skipped.

   → verify by `cargo test --lib store`.

   Traps:
   - `Limit` comes from `rusqlite::limits::Limit`, as in `src/db.rs`.
   - Do not call `compact` from the scan path (around line 568).
   - Keep the existing `purge` signature.
4. `tests/e2e.rs`: new `#[test] fn removing_a_root_restarts_cleanly_and_compacts()`.
   Use the existing harness style (temp base dir with `process::id()`,
   `Store::open` polling, `count`, `query`/`search`).
   - **Both roots must be large.** `VACUUM` rebuilds the surviving data, so
     the surviving root `a` must hold more than SQLite's 2 MB temp page
     cache, and the removed root `b` must be large enough to push free
     pages over a quarter of the file.
   - Generate:
     - `a`: `alpha.md`, containing "marmalade", plus about 400 files
       `a/n{i}.md`;
     - `b`: about 400 files `b/n{i}.md`.
     Each file is about 10 KB of varied text, with `format!` using `i` and
     the line number in every line.
   - **Measure first.** Time run 1 locally with the model enabled. If it is
     over about 60 s, lower both counts, keeping `a` large enough that
     `public.db` holds at least 8 MB after `b` is removed (check with
     `page_count * page_size`). Record the counts and timing in this plan.
     Time limits: 300 s per wait.
   - Run 1: `llm-index index` with both roots. **Readiness:** poll until
     each root has the expected number of files with at least one chunk
     (`SELECT count(DISTINCT f.id) FROM files f JOIN chunks c ON c.file_id = f.id WHERE f.root_id = ?`;
     check the real column names in `src/db.rs` SCHEMA) and "marmalade"
     finds `alpha.md`. Then record `page_count`, `page_size` and the
     `public.db` size, and kill and wait.
   - Rewrite the config with only `a`.
   - Run 2: same binary, `.env_remove("SQLITE_TMPDIR")`, stderr to
     `base/run2.log` (`Stdio::from(File::create(..))`).
   - Poll the log until it contains `store: compacted public` or
     `store: compact public:`.
   - Then assert:
     - the log contains `store: compacted public`;
     - `child.try_wait()` is `Ok(None)`;
     - no `files` row has root `b`;
     - "marmalade" still finds `alpha.md`;
     - `page_count` is below the baseline;
     - `freelist_count` is 0;
     - `page_count * page_size` is at least 8 MB, so the surviving data was
       big enough to spill.
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
   - Remove the private `docs` root from the trial config, which frees all
     of `private.db`, and add it back afterwards.
   - Confirm the unit stays active, `store: compacted private` is in the
     journal, and `private.db` shrinks.

## Tests

- `cargo test --all-features`: all pass, including the new e2e test and the
  two unit tests.
- With the fix removed, the new e2e test fails (step 6).
- `cargo clippy --all-targets --all-features -- -D warnings`: clean.
- `nix flake check`: green (the e2e test also runs under `TMPDIR=/build`).

## Rollback

Revert the merge commit. No schema or data format change. A database that
was already compacted stays valid.
