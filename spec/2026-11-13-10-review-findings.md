---
status: draft
issue: 10
intent: intent/2026-11-13-10-review-findings.md
---

# Spec: fix the bugs and gaps found in the Fable code review

Item numbers match the intent. Each design names the file and function to
change; the plan gives the order and the tests.

## Design

### Correctness

**1. Identity adoption and recovery** (`src/index/sched.rs` `scan_done`,
`try_root`, `identity_allows`; `src/index/shares.rs` `verdict`).

- Adopt on any scan whose mount identity did not change during the scan
  and whose root fd lives on that mount: `before.is_some() && before ==
  after`, whether or not the scan was complete. Today the fd check
  (`mount_id == fd_id`, `src/worker/scan.rs` around line 551) is folded only
  into `complete`. It moves to the identities themselves: `scan.rs` sends
  `identity_before`/`identity_after` as `None` when the mount id does not
  match the root fd's, and `scan_done` discards a scan with no identity
  (today it falls through to `queue_extracts`). Completeness still decides
  whether missing rows are deleted.
- `try_root` picks `Extract` only when the identity allows extraction, so
  a root whose verdict is `Probe` (autofs) falls through to `Scan` instead
  of choosing `Extract`, being refused, and never scanning. Watchers start
  only for `Ok` or `Adopt`.
- Recovery for a root already stuck (`expected` NULL, no `share`, indexed
  files > 0) stays manual, and this deviates from the intent's "recovers
  on its own": without a recorded identity nothing on disk tells a real
  root from an unmounted mount point, which can hold leftover files with
  the same names, and adopting it would let the next complete scan delete
  the real rows. `docs/troubleshooting.md` gives the two fixes: set
  `share` for the root (the verdict already adopts then), or rename the
  root. No root on p620 is stuck.
- `Mismatch` with a recorded identity is unchanged.

**2. Transient retry** (`sched.rs` `Message::Failed` handler, `send_next`).
Set `hold` and requeue before calling `send_next`, and add
`now < self.roots[i].hold` to the `stop` condition in `send_next`. A worker
on a held root then exits after its current file, and the root waits
`RETRY` as intended.

**3. `get` on spreadsheets** (`src/mcp.rs` `get`). For `rows`, the units are
ordered by chunk order, which is sheet order then row. `sheet` plus `start`
become a cursor: output starts at the first unit of that sheet with row
`>= start` and continues through later sheets. `end`, when given, bounds
rows in the starting sheet and stops there. There is one encoding: the
escaped name (`untrusted`, which is injective), the same form `search`
returns in `sheet`. `next.sheet` is emitted escaped and `get` compares its
`sheet` argument with the escaped stored name. A long name is matched in
full, not as the 200-character label `search` displays; `search`'s
`sheet` field is the label, and `docs/reference.md` says a cut label is not
a valid `get` argument (use `next.sheet`, or omit `sheet`). The docs also
say `sheet` is where reading starts, not a filter. Alternative rejected below.

**4. HTML slice** (`src/extract/html.rs` `skip_rawtext_element`). Compare
bytes: `bytes[a..b].eq_ignore_ascii_case(tag_name.as_bytes())`. No string
slicing remains in that function.

**5. Vanished directory** (`src/worker/watch.rs` additions loop,
`src/worker/scan.rs` `walk_subtree`). When opening the subtree root itself
fails with `ENOENT`, `ENOTDIR` or `EACCES` (the first and last are what
`scan.rs` `recoverable` already accepts; `ENOTDIR` covers a directory
replaced by a file; `ELOOP` and `EXDEV` cover a directory swapped for a
symlink or a mount, which `descend` already skips), `add_watches` skips
that path and the watcher carries on. The same errors from
`inotify.watches().add` on that path are skipped too. Other errors still
degrade, as today.

**6. Durations** (`src/index/mod.rs` `parse_duration`, `src/config.rs`
`validate`). Split the unit with `strip_suffix`/`char_indices` so a
multi-byte last character is an error, use `checked_mul`, and reject 0.
Move `parse_duration` to `config.rs` (the indexer calls it from there) and
call it from `Config::validate` for every root's `interval` and for
`limits.backoff_max`; the error message escapes the value as other config
errors do, so `check-config`, the self-test and `mcp` reject what the indexer
would reject.

**7. "changed" keeps the good row** (`sched.rs` `Message::Failed`). A
`"changed"` failure is not passed to `fail()`. The queued request, which
carries the old size and mtime, is dropped, and the root's `scan_due` is
set to `now` so the next scan (which respects `hold`) queues the file again
with fresh metadata. On a watched root the watcher usually queues it first.
This works for network roots, which have no watcher, and never retries a
request whose metadata is known to be stale.

**8. Solo retry** (`sched.rs` `send_next`, dispatch). New
`fn solo_held(&self, i, now) -> bool`: the root's `hold`, `root_blocked`,
its identity refusing extraction, or the share's backoff (`now <
not_before`). It does not use `shares.free`, which is also false while a
healthy worker is live on the share, so a worker would never drain for the
retry. `send_next` stops for solo only when the solo front is not held,
i.e. the retry is really waiting for the pool. Dispatch keeps its own
`shares.free` check before starting the solo worker.

**9. Smaller gaps.**

- `max_file_mb = 0`: `Config::validate` rejects 0 (the Nix option already
  needs a positive integer). Scan and extract then agree.
- mtime: `scan.rs` and `watch.rs` use the same saturating conversion as
  `worker/extract.rs` (one shared helper in `src/worker/mod.rs`).
- Model revision: at store open (`store.rs` `open_with`), per database
  and in one transaction each: if `meta.model_rev` is set and differs from
  `embed::MODEL_REV`, set every `chunks.vec` to NULL and `files.size` to
  -1 where `state = 'ok'`, then write the new revision. `size` is never
  returned by the MCP tools, so `recent` and the date filters keep working
  during the re-index; `mtime_ns` is untouched. `known()` no longer
  matches, so every file is
  re-extracted and re-embedded by the normal scan. A model change is rare;
  a full re-index is the simple correct answer.

### Hardening

**10. Worker sandbox** (`src/worker/sandbox.rs` `restrict`). Raise the ABI
to `ABI::V9` (landlock 0.4.7 has `ABI::V9`, `AccessFs::ResolveUnix` and
`Scope`; checked in the crate source) and add
`.scope(Scope::from_all(abi))?`. With V9, `AccessFs::from_all` includes
`ResolveUnix`, which is not in `from_read`, so connecting to any pathname
UNIX socket is denied; the scopes deny abstract sockets and signals to
processes outside the worker's domain. Compatibility stays `BestEffort` for
these newer rights: on an older kernel the worker keeps today's protection.
Minimum ABI is unchanged; the socket rule is enforced from ABI 9 and the
scopes from ABI 6. The self-test (`src/worker/mod.rs`) probes all three
(pathname socket outside the root, abstract socket, `kill(ppid, 0)`) and
prints whether each is denied, so a weaker kernel is visible rather than
silent. p620's kernel reports Landlock ABI 11.
The existing hard checks (filesystem enforced, TCP bind/connect) are
unchanged. The coordinator and `mcp` are not changed.

**11. Labels** (`src/worker/extract.rs`, `src/extract/sheet.rs`, new
`sanitize::label`).

- Before chunking, every title (including the file-name fallback), heading
  and symbol passes through `sanitize::label(s, 200)`: first `\n`, `\r`,
  `\t`, U+0085, U+2028 and U+2029 become spaces, then `sanitize`, then any
  remaining control character becomes a space, runs of spaces collapse,
  outer spaces are trimmed, and the result is cut to 200 characters. An
  empty label becomes `None` where the field is optional. Applied before
  `chunk::chunk`, so a
  huge heading is never copied into every chunk.
- Sheet names are an identity (item 3 uses them as the paging position), so
  they are not normalised. `sheet.rs` cuts a name longer than 1000
  characters and, if that makes two names in one workbook equal, appends
  `#2`, `#3` and so on, so names stay unique.
- `mcp.rs` applies `label(…, 200)` to `title`, `heading`, `symbol` and the
  displayed `sheet` in `search` and `recent` hits, so rows already in the
  index are covered without a re-index. `get`'s `next.sheet` is the paging
  key (item 3), not a display field.

**12. Control characters** (`src/sanitize.rs`). `sanitize` also removes C0
controls except `\t` and `\n`, DEL, and all C1 controls (U+0080-U+009F),
plus U+00AD, U+034F, U+115F, U+1160, U+3164, U+FFA0, U+2028, U+2029,
U+206A-U+206F and U+FFF9-U+FFFB. `\r` is removed (`lines()` already treats
`\r\n` as a line end).

**13. Embed cache** (`src/index/store.rs`, `src/db.rs`). Remove the
`embed_cache` table. Its job, not embedding an unchanged chunk again, is
done by reading the file's current chunks in `store_file` before they are
deleted: a map from `(heading_or_symbol, text)` to `vec` (non-NULL only),
reused for any new chunk with the same pair. The relpath is the same, so
the embedding input is identical. `db.rs` runs `DROP TABLE IF EXISTS
embed_cache` on the writer at open; the next compaction (#8) returns the
space. This deletes the stale vectors and stops the duplicate storage.

**14. Denylist** (`src/deny.rs` `BUILTIN`). Add `nix.conf`,
`.config/age/`, `rclone.conf`, `.config/containers/auth.json`,
`credentials.tfrc.json`, `.config/glab-cli/`, `.config/github-copilot/`,
`secrets.yaml`, `secrets.yml`, `secrets.json`, `credentials.json` and
`service-account*.json`, following the existing convention (a directory
pattern ends in `/`). `.config/gh/` and `.config/sops/` are already there.
Matching rules are the existing ones. `docs/security.md`, which lists the
built-in patterns, is updated.

### Scale and robustness

**15. Tier move in batches** (`store.rs` `move_tier`). Process ids in
batches of 256: read the batch, delete it from the source and commit, insert
it into the destination and commit. A crash loses at most one batch, which
is re-indexed, and never leaves a copy in both. Peak memory is the id list
(8 bytes a file) plus one batch of rows.

**16. Existence, not count** (`store.rs` `indexed_files`). Query
`SELECT EXISTS(SELECT 1 FROM files WHERE root_id = ?1 AND state = 'ok')`,
stopping at the first database that has one, and return 0 or 1. A database
error still answers non-zero (conservative), as today. The verdict only
tests zero.

**17. Store errors** (`Sink::extracted`, `Sink::failed`, `sched.rs`). Both
return `Result<(), String>`. Every failure path already goes through
`fail()` (extract failure, worker death, timeout); `fail()` now calls
`sink.failed` first and records `r.failed` only when that write succeeded.
On any store error the scheduler requeues the file, sets the root's `hold`
(item 2 makes the worker stop) and logs once per hold; `DbSink` stops
logging these itself. `extract_died` gets `now` so it can set the hold.
The writer connection sets `busy_timeout = 5000` first, before the WAL
pragma and the `DROP TABLE` of item 13. Every requeue site (`reload`,
`RootError`, `abandon`, transient failure, store error) goes through one
helper that does not overwrite a newer entry a watch event already queued.

**18. Quadratic paths.**

- `queue_one`: `queued` becomes `HashMap<String, (u64, i64)>`, holding the
  latest size and mtime, and `queue` holds only the relpath. A duplicate
  updates the map entry; `next_allowed` takes size and mtime from the map
  when it pops. No linear search. The requeue helper above uses
  `entry().or_insert`, so `queue` and `queued` stay one-to-one.
- `get`: the unit kind is read first (`SELECT kind … ORDER BY ord LIMIT
  1`). In lines and pages mode the chunk query skips chunks that end before
  `start` (bound as `i64`; a `start` above `i64::MAX` is an argument
  error). Units are produced lazily from the row iterator and building
  stops as soon as the output budget is used and `next` is known. Rows
  mode keeps sheet order (item 3) and streams the same way. Overlap
  de-duplication (`last_line`) and the intra-unit `offset` keep working.
  Each page then costs about one page of work.
- Coordinator pipe (`src/index/mod.rs` `Proc::parse`): remember how far
  the buffer has been searched for a newline and search only the new bytes.
  The cursor resets whenever the buffer is cleared (including the
  `MAX_LINE` overflow path), and consumed lines are drained once per read.

**19. Docs.** `docs/reference.md` "A file is skipped when" lists the
skipped directory names, the `result*` prefix, lockfiles and `CACHEDIR.TAG`.

### Open questions from the intent

- Ranking across `public.db` and `private.db`: unchanged; `docs/reference.md`
  says word hits are ranked per database on the private server.
- Local agents with a shell: `docs/security.md` says plainly that the tier
  split protects against what a client can ask the MCP server, not against
  an agent running as the owner with shell access.

## Alternatives rejected

- **Item 1: never queue extracts from an incomplete first scan.** A root
  with one persistently failing subdirectory would then never be indexed.
- **Item 1: auto-adopt any stuck root,** or adopt one where a scanned
  relpath is already known. An unmounted mount point can hold leftover
  files with the same names, so both can adopt the wrong filesystem and
  delete the real rows.
- **Item 3: accept both raw and escaped sheet names.** Ambiguous: a sheet
  named `<x` and one named `&lt;x` collide.
- **Item 7: treat `changed` as transient.** The retry carries the stale
  size and mtime, so it fails again every time and blocks the scan that
  would refresh it.
- **Item 3: a new `cursor` argument.** Changes the tool schema for clients;
  redefining `sheet` as the start point fixes the documented loop with no
  schema change.
- **Item 10: a seccomp filter denying `socket()` in the worker.** Needs a new
  dependency or hand-written BPF; Landlock V9 already in the crate covers
  pathname sockets, abstract sockets and signals.
- **Item 13: prune `embed_cache` rows on delete.** The key is a hash of
  model, relpath, heading and text; pruning needs that recomputed for every
  deleted chunk, and the table still duplicates every vector.
- **Item 17: mark the root `degraded` on a store error.** `degraded` means
  the watcher fell back to polling; overloading it would mislead `status`.
  The log and the retry are enough for a condition (disk full) the owner
  sees elsewhere.

## Risks

- **Item 1:** adopting on an incomplete but stable first scan has the same
  exposure as today's adoption on a complete one (an empty root adopts
  whatever is mounted). A root already stuck needs the owner to act.
- **Item 9 model change** re-indexes everything once (about 24 minutes on
  p620). Only on a model bump; this change does not bump the model.
- **Item 10** could deny something a parser needs. pdfium and the parsers
  do not use UNIX sockets or signals; the VM tests and the parser crash
  tests run the real worker under the new rules.
- **Item 12** removing `\r` changes stored text for CRLF files. Line
  numbers are counted by `lines()`, which already ignored `\r`.
- **Item 13** the first start after upgrade drops the table; files edited
  later re-embed only changed chunks, as today.
- **Item 14** a wanted file named `credentials.json` or `secrets.yaml` is no
  longer indexed. These names are almost always secrets.
- **Host:** only p620 runs it. The upgrade is a normal restart.

## Verification

- `cargo test` and `cargo clippy -- -D warnings` pass. New tests, each
  failing on 84fc986:
  - sched: incomplete first scan with a stable mount adopts and indexing
    continues; a stuck legacy root stays `offline`; a transient failure
    holds the root and does not resend within `RETRY`; a `changed` failure
    keeps the row, drops the request and makes a scan due (including on a
    network root); a held solo item does not stop other workers; each of
    `extracted` and `failed` failing requeues, and the retry succeeds.
  - mcp: a three-sheet workbook paged with a small budget returns every
    row of every sheet exactly once; sheets named `<x` and `&lt;x` in one
    workbook page correctly; paging a large text file does work
    proportional to the page.
  - sheet: two names equal after the 1000-character cut get unique names.
  - mcp: `get` on a large text file pages through it with each call's
    work bounded; `start` above `i64::MAX` is an argument error.
  - index: `Proc::parse` with a line split across many reads, and with an
    over-long line followed by a good one.
  - sched: an autofs root (`Probe`) with queued files still scans; a scan
    whose root fd is not on the reported mount adopts nothing.
  - sandbox: under the worker rules, connecting to a pathname socket
    outside the root, connecting to an abstract socket and `kill(ppid, 0)`
    are denied when the kernel's ABI supports them (the test reports and
    skips otherwise); p620 (kernel 7.3) runs them.
  - html: `<script></aaaaaé</script>` does not panic.
  - config: `5µ`, `0s`, an overflowing number, and `max_file_mb = 0` are
    rejected.
  - sanitize/label: ESC, C1, the listed code points, a 10 000-character
    title and a heading with newlines.
  - store: a model revision change resets vectors and `mtime_ns`; a tier
    move of 1 000 files works in batches; `embed_cache` is gone after open
    and unchanged chunks keep their vector across an edit.
  - deny: each new pattern.
  - watch: a directory created and removed before its watch is added does
    not degrade the watcher.
- `nix flake check` passes, including the VM tests.
- p620: deploy, both roots end `ok`, a public search from Claude, Codex and
  agy returns results, and `sqlite3 public.db .tables` shows no
  `embed_cache`.
