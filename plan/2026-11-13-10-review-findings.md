---
status: approved
issue: 10
spec: spec/2026-11-13-10-review-findings.md
coders:
  1: coder   # scheduler state machine across sched.rs/shares.rs/store.rs; large and stateful
  2: codex   # get paging: small, tricky cursor logic in one function
  3: codex   # sanitize/label: small, exact character rules with tests
  4: codex   # three small tricky fixes (byte slice, watch errors, mtime saturation)
  5: coder   # config validation and the pipe reader; medium, self-contained
  6: coder   # store.rs/db.rs follow-up to step 1; same agent keeps its cache warm
  7: codex   # landlock scopes and denial probes: small, security-sensitive
  8: agy     # denylist entries and docs: large, mechanical text edits
reviewers:
  - codex    # independent read of the whole diff against this plan
  - agy      # second independent read; disagreement is useful on the scheduler
---

# Plan: fix the bugs and gaps found in the Fable code review

Self-contained: every decision below is from the approved spec. Item numbers
(`#n`) match the intent and spec. Repo:
`/mnt/code/Source-home/GitHub/nixarchy-llm-index`, branch
`fix/10-review-findings`.

## How the steps run

- Each implementer works in its own worktree off `fix/10-review-findings`
  (`../llm-index-10-sN`). The lead reviews each diff, commits it in that
  worktree and cherry-picks onto the branch. Implementers never stage or
  commit.
- Wave A in parallel: steps 1, 3, 5, 8. Wave B, each once its
  dependencies are on the branch: step 6 after step 1 (same coder), step 2
  after step 3, step 7 after steps 1 and 5 (it edits `src/index/mod.rs`
  `self_test_in`), step 4 after steps 1, 3 and 7 (it touches `scan.rs`
  `scan()`, `worker/extract.rs` and `worker/mod.rs`, which those steps also
  edit). Cherry-pick order: 1, 5, 3, 8, then 6, 2, 7, 4.
- Shared files in wave A and who owns what: `src/index/mod.rs` — step 1
  the `Sink` trait (around line 74), step 5 `parse_duration` and `Proc`;
  `src/worker/scan.rs` — step 1 only in wave A. The lead resolves any
  cherry-pick conflict by hand and reruns the checks.
- A test "fails on `84fc986`" means it asserts the defect itself (a count,
  a bound, a state), not only that the happy path works. Safety checks
  that already pass on `84fc986` are allowed but are named as such.
- Check for every step, in its worktree:
  `cargo test --all-targets && cargo clippy --all-targets -- -D warnings`.
  Run cargo with `CARGO_TARGET_DIR=target-sN` so worktrees do not share a
  lock.
- Traps for every step: no new dependency; no `unwrap` on input; keep each
  function's existing error handling; every behaviour change gets a test
  that fails on `84fc986`; the public server must still open only
  `public.db` and filter every query on `visible()`.

## Steps

### 1. Scheduler (#1, #2, #7, #8, #16, #17, #18 queue)

Files: `src/index/sched.rs`, `src/index/shares.rs`, `src/worker/scan.rs`
(identity part), `src/index/mod.rs` (the `Sink` trait declaration, around
line 74), `src/index/store.rs` (`Sink` impl).

1. **#1 adoption.** In `scan.rs` `scan()` (lines ~540-609), send
   `identity_before`/`identity_after` as `None` when the mount id does not
   equal the root fd's mount id (today that check only feeds `complete`),
   including the early return when `root_mount_id` fails (~540-549). In
   `sched.rs` `scan_done`: when both identities are `Some` and differ,
   keep today's `offline / identity mismatch`; when either is `None`,
   discard the scan with no state change (`scan_finished(i, now, RETRY)`
   and return, before `queue_extracts`). Adopt when
   `v == Some(Adopt) && before == after`, dropping the `complete`
   condition. Completeness still decides deletions (unchanged).
2. **#1 identity checks.** Split `identity_allows` into a pure
   `fn verdict_of(&self, i) -> Verdict` plus two uses: scanning is allowed
   unless `Mismatch`; extraction and watchers only on `Ok` or `Adopt`.
   The `Mismatch` side effects (clear queue, hold, `offline`) stay where
   they are today. `send_next` and the solo path use the extract check;
   `try_root` chooses `Role::Extract` only when extraction is allowed and
   otherwise falls through to the `Scan` branch, so `Probe` (autofs) and a
   root with queued files still scan.
3. **#1 legacy roots** stay `Mismatch` (no auto-recovery). Existing tests
   `shares.rs` ~158 and `sched.rs` ~1408 keep their expectations.
4. **#2.** In the `Message::Failed` transient branch, set `hold = now +
   RETRY` and requeue *before* `send_next`. In `send_next`, add
   `now < self.roots[i].hold` to `stop`.
5. **#7.** `"changed"` is not passed to `fail()`: drop the request, keep
   the row, set the root's `scan_due = now`. Do not add it to
   `transient()`.
6. **#8.** Add `pub fn backing_off(&self, share, now) -> bool` to
   `ShareBook` in `shares.rs` (`now < not_before`, ignoring `live`). Add
   `fn solo_held(&self, i, now) -> bool` = `now < roots[i].hold ||
   root_blocked(i) || extraction refused by verdict_of(i) ||
   shares.backing_off(…)`. Not `shares.free`. `send_next` stops for solo
   only when `solo.front()` exists and is not held. In dispatch, the drain
   `return` is gated on `!solo_held`; `shares.free` is checked only inside
   `if self.pool_live() == 0`, right before starting the solo worker.
7. **#16.** `DbSink::indexed_files`: `SELECT EXISTS(SELECT 1 FROM files
   WHERE root_id = ?1 AND state = 'ok')`, stop at the first database that
   says yes, return 0 or 1; a database error returns 1.
8. **#17.** `Sink::extracted` and `Sink::failed` return
   `Result<(), String>`; `DbSink` stops logging these errors itself.
   `fail()` calls `sink.failed` first and inserts into `r.failed` only on
   `Ok`. `fail()` and `extract_died` take `now`; update their callers
   (~399, ~703, ~751). On any store error: requeue, set
   `hold = now + RETRY`, log once per hold.
9. **#18 queue.** `queued: HashMap<String, (u64, i64)>`, `queue:
   VecDeque<String>`. `queue_one` updates the map for a duplicate (no
   linear search); `next_allowed` and `has_allowed` read size/mtime from
   the map. One requeue helper (on `Entry::Vacant` only: insert and push to
   the front; an occupied entry is left alone, it is newer) used by
   `reload`, `RootError`, `abandon`, the transient branch and store
   errors, so a newer watch-event entry is never overwritten and `queue`
   and `queued` stay one-to-one.

Tests (sched unit tests): incomplete stable first scan adopts and
indexing continues; legacy stuck root stays offline; a `ScanDone` with
`(Some, None)` or `(None, Some)` identities queues nothing and calls no
`sink.scan_done`; a `scan.rs` test where the root fd is not on the
reported mount sends `None`; `Probe` root with queued files scans, and a
`Kind::Local` root on autofs starts no watcher; with two queued files, a
transient failure of the first holds the root and the second is not sent
within `RETRY`; `changed` keeps the row,
drops the request, makes a scan due, also on a network root; held solo
does not stop other workers; `extracted` failing and `failed` failing each
requeue and the retry succeeds (the file is not in `r.failed` after a failed
`failed`). Queue race: an extraction is in flight, a watch event queues the
same file with newer size/mtime, then the old job is requeued through each
route (`reload`, `RootError`, `abandon`, transient, store error); assert
one queue entry with the newer metadata. `indexed_files`: returns 1 when
only the first database has a row (and does not query the second, via a
counting test sink or a closed second connection), and 1 on a database
error. Update test sinks for the new signatures.

Traps: `scan_done` order matters (verdict before adoption); tests in
`store.rs` call `sink.extracted(...)` about 20 times and need `.unwrap()`
or `?` for clippy; the test at `sched.rs` ~1525 builds `queue`/`queued`
directly and must use the new types.

### 2. `get` paging (#3, #18 get, #11 output)

File: `src/mcp.rs`. Runs after step 3.

- Read the unit kind first: `SELECT kind FROM chunks WHERE file_id = ?1
  ORDER BY ord LIMIT 1`.
- Rows mode: units in chunk order (sheet order, then row). `sheet` +
  `start` is a cursor: start at the first unit of that sheet with row
  `>= start`, continue through later sheets. `end` bounds rows of the
  starting sheet and stops there. One encoding: compare the `sheet`
  argument with `untrusted(stored_name)` (escaped); emit `next.sheet`
  escaped the same way.
- Lines and pages mode: the chunk query adds `AND end >= ?2` with `lo`
  bound as `i64` (`i64::try_from`, else argument error).
- Stream units from the row iterator; stop once the budget is used and
  `next` is known. Keep `last_line` de-duplication and `offset`. In pages
  mode, consecutive chunks with the same `start` are still joined into one
  unit with `\n\n` before `offset` and the budget apply (peek the next
  row before emitting a page).
- `search` and `recent`: `title`, `heading`, `symbol` and the displayed
  `sheet` go through `sanitize::label(…, 200)` before `untrusted`; an
  empty label for an optional field is left out, as an absent one is
  today.
- Tests: three-sheet workbook paged with a small budget returns every row
  once; sheets `<x` and `&lt;x` in one workbook page correctly; large text
  file paging does bounded work per call (count chunk rows fetched and
  units built for a late page: both bounded by about one page, not by the
  file's earlier content); `start` above `i64::MAX` errors; a
  10 000-character title is cut in `search` and `recent`; `end` on a
  multi-sheet workbook stops at the starting sheet. Safety (passes today too): a
  PDF page split over several chunks comes back merged; a unit longer than
  the budget pages with `offset` and loses no character.
- "About one page of work" (spec #18) is measured in Rust (rows fetched,
  units built). SQLite still walks the `(file_id, ord)` index past earlier
  chunks to apply `end >= ?2`; that reads index and row headers, not the
  text, and is accepted. No new index.

Traps: `docs/reference.md` changes are step 8's; `get` must keep the
`visible()` gate and the "not found" for hidden files.

### 3. Labels and control characters (#11, #12)

Files: `src/sanitize.rs`, `src/worker/extract.rs`, `src/extract/sheet.rs`.

- `sanitize`: also remove C0 controls except `\t` and `\n`, DEL, C1
  (U+0080-U+009F), U+00AD, U+034F, U+115F, U+1160, U+3164, U+FFA0,
  U+2028, U+2029, U+206A-U+206F, U+FFF9-U+FFFB, and `\r`.
- New `pub fn label(s: &str, max: usize) -> String`: map `\n`, `\r`,
  `\t`, U+0085, U+2028, U+2029 to space; then `sanitize`; then any
  remaining control char to space; collapse space runs; trim; cut to
  `max` chars.
- `worker/extract.rs`: before `chunk::chunk`, apply `label(…, 200)` to the
  title (and the file-name fallback) and to every segment heading and
  symbol; an empty optional label becomes `None`.
- `sheet.rs`: sheet names are not normalised; cut a name over 1000 chars
  and, if that makes two names in a workbook equal, append `#2`, `#3`, …
- Tests: ESC, C1, each listed code point, `\r\n`; `label` on a
  10 000-char string with newlines; a heading of 100 KB is cut before
  chunking; two 1 200-char sheet names with the same first 1000 chars get
  unique names.

Traps: `sanitize` is used for chunk text, so `\t` and `\n` must stay;
existing sanitize tests must pass.

### 4. HTML, watcher, mtime (#4, #5, #9 mtime)

Files: `src/extract/html.rs`, `src/worker/watch.rs`, `src/worker/scan.rs`
(`walk_subtree` and the mtime line in `scan()`), `src/worker/extract.rs`
(mtime only), `src/worker/mod.rs`.

- `skip_rawtext_element`: compare bytes,
  `bytes[a..b].eq_ignore_ascii_case(tag_name.as_bytes())`.
- `walk_subtree` / `add_watches`: when opening the subtree root or adding
  its inotify watch fails with `ENOENT`, `ENOTDIR`, `EACCES`, `ELOOP` or
  `EXDEV`, skip that path and continue; other errors still degrade. This
  applies only when the path is not the root itself (`rel != ""`): a
  failing root watch still degrades.
- One saturating `fn mtime_ns(sec: i64, nsec: i64) -> i64` helper in
  `src/worker/mod.rs` (it takes plain numbers because `worker/extract.rs`
  uses `fstat`, not `statx`), used by `scan.rs` `scan()` (~571),
  `watch.rs` (~277) and `worker/extract.rs` (~125-127).
- Tests: `<script></aaaaaé</script>` does not panic; a directory created
  and removed before its watch is added leaves the watcher healthy; an
  mtime past 2262 saturates.

Traps: runs after steps 1, 3 and 7 are on the branch, so start from the
branch head; in `scan.rs` touch only `walk_subtree` (~360-372) and the
mtime expression in `scan()`.

### 5. Config and pipe reader (#6, #9 max_file_mb, #18 pipe)

Files: `src/config.rs`, `src/index/mod.rs`.

- Move `parse_duration` from `src/index/mod.rs` to `src/config.rs`
  (`pub`), using `strip_suffix`/`char_indices` for the unit, `checked_mul`,
  and rejecting 0. Its error escapes the value like other config errors.
- `Config::validate` parses every root's `interval` and
  `limits.backoff_max`, and rejects `max_file_mb = 0`.
- `Proc::parse`: keep a search cursor so only new bytes are searched for
  `\n`; reset it whenever the buffer is cleared (including the `MAX_LINE`
  overflow path); drain consumed lines once per read.
- Tests: `5µ`, `0s`, an overflowing number, `max_file_mb = 0` rejected;
  `Proc::parse` with a line split over many reads (assert total bytes
  searched for `\n` is linear in the input, via a counter in the parser)
  and with an over-long line then a good one (cursor reset, no panic).
- Implemented (deviation): the buffer, cursor and counter live in a small
  `Lines` struct that `Proc::parse` calls, because `Proc` owns a real
  `Child` and cannot be built in a unit test.

### 6. Store (#9 model, #13, #15, #17 timeout)

Files: `src/index/store.rs`, `src/db.rs`, `src/embed.rs` (remove
`cache_key` if nothing else uses it, or clippy fails on dead code). Runs
after step 1.

- `db.rs` writer open: `busy_timeout = 5000` first, then the WAL pragma,
  then `DROP TABLE IF EXISTS embed_cache`; remove the table from the
  schema.
- `store_file`: before deleting the file's current chunks, read a map
  `(heading_or_symbol, text) → vec` (non-NULL only) and reuse it for new
  chunks with the same pair; embed only the rest. Remove `vector()`'s
  cache read and write.
- `open_with`, per database, one transaction each: if `meta.model_rev` is
  set and differs from `embed::MODEL_REV`, `UPDATE chunks SET vec = NULL`,
  `UPDATE files SET size = -1 WHERE state = 'ok'`, write the new revision.
- `move_tier`: batches of 256 ids; per batch read, delete from source and
  commit, insert into destination and commit.
- Tests: model revision change resets vectors and sizes and leaves
  `mtime_ns` (the spec's verification line says `mtime_ns`; its design,
  which this plan follows, says `size`); `embed_cache` is gone after open;
  an unchanged chunk keeps its seeded vector across an edit with the
  embedder called only for the changed chunk (counting embedder); a tier
  move of 1 000 files reads at most 256 files per batch (assert via a
  batch-size counter) and the existing `abort_after_first_commit` hook,
  firing after the first batch's source commit, leaves 744 files in the
  source and none in the destination (today: 0 and 0).

Traps: compaction (#8) already runs after `purge` at open; keep the order
open → purge → compact.

### 7. Worker sandbox (#10)

Files: `src/worker/sandbox.rs`, `src/worker/mod.rs` (self-test),
`src/index/mod.rs` (`self_test_in` output). Runs after steps 1 and 5.

- `restrict`: `ABI::V9`, keep `BestEffort`, add
  `.scope(Scope::from_all(abi))?`.
  `AccessFs::from_all(V9)` now includes `ResolveUnix`; `from_read` does
  not, so pathname socket connects are denied.
- Self-test: under the worker rules, probe a pathname socket outside the
  root, an abstract socket and `kill(getppid(), 0)`; print whether each
  is denied. Do not fail the self-test on a kernel that lacks them. Both
  listeners are bound *before* `sandbox::start` (an abstract socket bound
  inside the worker's own domain is allowed by the scope), and the
  pathname one under the self-test's state directory, because the
  coordinator's Landlock rule already denies binding in `/tmp`.
- `src/index/mod.rs` `self_test_in` today discards the child's stdout and
  stderr on success; it prints the probe lines.
- Test: with a live listening pathname socket outside the root, a live
  abstract socket and the parent process: the pathname connect is denied
  with a permission error when the kernel ABI is at least 9; the abstract
  connect and `kill(getppid(), 0)` are denied when the ABI is at least 6;
  each skips with a message on an older kernel. The same connects succeed
  without the rules (so a missing socket never passes as denied). This
  host reports ABI 11.

Traps: the TCP hard check (`tcp_restriction_supported`) stays; parser
crash tests and VM tests must still pass under the new rules.

### 8. Denylist and docs (#14, #19, open questions, #1 and #3 docs)

Files: `src/deny.rs`, `docs/`.

- `deny.rs` `BUILTIN`: add `nix.conf`, `.config/age/`, `rclone.conf`,
  `.config/containers/auth.json`, `credentials.tfrc.json`,
  `.config/glab-cli/`, `.config/github-copilot/`, `secrets.yaml`,
  `secrets.yml`, `secrets.json`, `credentials.json`,
  `service-account*.json`, with a test per pattern.
- `docs/security.md`: list the new patterns; say plainly that the tier
  split guards what an MCP client can ask, not an agent running as the
  owner with a shell.
- `docs/reference.md`: "A file is skipped when" lists every directory name
  in `src/worker/scan.rs` (`.git`, `node_modules`, `.venv`, `venv`,
  `__pycache__`, `.direnv`, `vendor`, `target`, `third_party`, and any
  others in that list), the `result*` prefix, the lockfiles and
  `CACHEDIR.TAG`; `get`'s `sheet` is where reading starts (escaped form,
  as in `next.sheet`); a cut `search` label is not a valid `get`
  argument; on a spreadsheet `end` bounds only the starting sheet; word
  hits on the private server are ranked per database; `maxFileMb` (the
  option name used there) must be positive.
- `docs/troubleshooting.md`: a root stuck in `identity mismatch` from an
  incomplete first scan recovers by setting `share` or renaming the root.
- Check: `npx markdownlint-cli2 "docs/**/*.md"` and the step's cargo
  check.

## Deviations recorded during implementation

- Step 1's new tests run in parallel with
  `embed::tests::query_mode_does_not_map_and_rss_stays_flat`, which measures
  the whole test process's RSS and then failed. That test now re-runs
  itself in a child process, so it measures only itself.
- `nix flake check` failed once in `network-mounts` stage 4: a share
  mounted after the indexer started was indexed only after the root's
  mismatch hold (up to `RETRY`, 60 s) ran out, at the edge of the test's
  60 s limit. `rederive` now clears the hold when a root's mount changes,
  so the requested scan really starts at once (unit test
  `a_share_mounted_later_is_scanned_at_once`, which fails without it). The
  VM test prints status and the journal when that stage fails.

## Tests

- Every step's check passes in its worktree, then on the branch after each
  cherry-pick: `cargo test --all-targets` and `cargo clippy --all-targets
  -- -D warnings`.
- `nix flake check` on the branch (includes the VM tests).
- Final reviews: a fresh Opus agent, Codex and agy each read `git diff
  84fc986..HEAD` against this plan.
- p620 after deploy: `systemctl --user status llm-index`, `llm-index
  status` shows both roots `ok`, a public `search` from Claude, Codex and
  agy returns results, `sqlite3 ~/.local/state/llm-index/public.db .tables`
  shows no `embed_cache`, and the worker self-test reports the three
  probes denied.

## Rollback

- Code: revert the merge commit on `main`, then bump the flake input in
  nixos_config back to `84fc986` and deploy p620.
- State: no migration to undo. `embed_cache` is recreated empty by the old
  code; a model-rev reset only re-indexes.
