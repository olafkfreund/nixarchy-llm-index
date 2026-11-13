---
status: draft
issue: 10
author: olafkfreund
---

# Intent: fix the bugs and gaps found in the Fable code review

## Problem

Three read-only Fable reviewers read the code at 84fc986 on 2026-11-13.
Claude checked the high findings against the code. None of them lets a
public-tier client read private-tier data, but several break indexing,
weaken the sandbox or scale badly. "Confirmed" below means the code path
was read end to end; "traced" means a reviewer traced it and Claude
spot-checked the code.

### Correctness

1. **A new root can be stuck offline forever** (confirmed).
   `src/index/sched.rs` `scan_done` records the share identity only on an
   accepted (complete) scan, but `queue_extracts` runs either way. After one
   file is stored, `src/index/shares.rs` `verdict(None, None, _, 1)` is
   `Mismatch`, and `identity_allows` clears the queue and refuses every later
   scan. A restart does not help. An incomplete first scan caused by an
   error other than `EACCES`/`ENOENT` (for example `EIO` on one
   subdirectory) is enough; those two are already treated as recoverable.
2. **Temporary extract failures spin** (confirmed). On a transient `Failed`,
   `send_next` runs before the root's `hold` is set, and `next_allowed` never
   checks `hold`. Two failing files on a stale network share retry each
   other with no delay until the worker's file or time cap ends it.
3. **`get` drops spreadsheet sheets** (confirmed). `next` carries `sheet`;
   passing it back filters to that sheet only, so `next` becomes `null`
   once that sheet is finished and later sheets are never returned.
   Without `sheet`, `start` drops low rows of every sheet.
   Sheet names are escaped in `search` but raw in `get` and `next`.
4. **HTML pre-pass panics** (confirmed). `src/extract/html.rs`
   `skip_rawtext_element` slices `html[lt+2..lt+2+len]` at a byte offset
   that can fall inside a multi-byte character, e.g. `</aaaaaé` inside a
   `<script>`. Text in a script that has a multi-byte character just after
   `</` kills the worker.
5. **A vanished directory degrades the watcher** (traced). `walk_subtree`
   propagates the error for the new directory itself, so `mktemp -d` and a
   quick `rm` can put the root into polling with a full re-walk.
6. **Bad durations panic or loop** (confirmed). `parse_duration`
   (`src/index/mod.rs`) panics on a multi-byte last character and can
   overflow; `Config::validate` does not check `interval` or `backoff_max`,
   so a bad value stops the indexer at start (and systemd restarts it)
   while `mcp` keeps serving a stale index.
7. **A file edited between scan and extract loses its good row** (traced).
   `"changed"` is not treated as transient, so the indexed row is replaced
   by a failed one.
8. **A parked solo retry slows extraction** (traced). While `solo` is
   non-empty, `send_next` stops every extract worker after one file, so
   for up to `backoffMax` every file costs a new worker process.
9. **Smaller correctness gaps** (traced): `max_file_mb = 0` means "skip
   every non-empty file" in scan and "unlimited" in extract; mtime
   arithmetic overflows in scan and watch but saturates in extract; `meta.model_rev` is never read, so a model
   change leaves old vectors.

### Hardening

- **10. Worker sandbox scopes** (confirmed). `src/worker/sandbox.rs` `restrict`
  handles filesystem and TCP access only. It sets no Landlock scope for
  abstract UNIX sockets or signals, and `nix/config.nix` allows `AF_UNIX`.
  After a parser exploit these are possible channels out of the sandbox
  (abstract sockets, signals to the user's processes); whether a given
  pathname socket is reachable is to be checked in the spec.
- **11. Uncapped titles, headings and sheet names** (confirmed). They are
  escaped but have no length cap and no newline or control-character
  cleanup. A 200 KB `title:` reaches every search hit, and a huge heading
  copied into every chunk can exhaust worker memory.
- **12. Control characters in text** (confirmed). `sanitize` strips a list
  of invisible characters only; ESC and other C0/C1 controls survive
  anywhere in the text (the 8 KiB check rejects only NUL). U+00AD,
  U+034F, U+115F, U+1160, U+3164, U+FFA0, U+2028/2029, U+206A-206F and
  U+FFF9-FFFB also survive.
- **13. `embed_cache` grows forever** (confirmed). Nothing deletes from it, so
  vectors (keyed by a hash; no text) of deleted files, or files moved to
  private, stay in `public.db`. No MCP query reads the table.
- **14. Denylist gaps** (traced). Common secret files with indexable
  extensions are not denied: `nix.conf`, `age/keys.txt`, `rclone.conf`,
  `containers/auth.json`, `credentials.tfrc.json`, `secrets.yaml`.

### Scale and robustness

- **15. Tier move loads a whole root into memory** (confirmed):
  `src/index/store.rs` `move_tier`.
- **16. One full count per extracted file** (confirmed): `indexed_files` runs
  `count(*)` over both databases where only "any?" is needed, from
  `send_next` and other scheduling paths.
- **17. Store write errors are dropped** (confirmed): `Sink::extracted` and
  `failed` log and return `()`, and the scheduler moves on as if the
  file were stored. No `busy_timeout` is set explicitly.
- **18. Quadratic paths** (traced): `queue_one` searches the queue linearly;
  `get` rebuilds every line per call; the coordinator rescans its pipe
  buffer for a newline on every read.
- **19. Undocumented skips** (confirmed): directories named `vendor`, `target`,
  `venv`, `third_party` or starting with `result`, lockfiles and
  `CACHEDIR.TAG` directories are skipped at any depth, and
  `docs/reference.md` does not say so.

## Proposed outcome

- Items 1-8, 10-13, 15 and 17 are fixed in code. Items 9, 14, 16 and 18
  are fixed in code unless the spec shows a fix is not worth it, in which
  case it is documented with the reason. Item 19 is a docs fix. Each fix
  that changes behaviour has a test that fails before and passes after.
- Existing state recovers on its own after upgrade: a root already stuck
  by item 1 adopts its identity again; stale cache rows (item 13) are
  removed; vectors from an older model are recomputed (item 9). Removed
  cache rows are a logical delete; the bytes go at the next compaction
  (#8), not by forensic erase.
- The tier guarantee is unchanged: the public server still opens only
  `public.db` and every query still filters on `visible()`.
- The existing test suites (`cargo test`, `nix flake check` with the VM
  tests) pass.
- p620 runs the fixed version, both roots end `ok`, and all three clients
  still find results.

## Affected users and systems

- Every install of llm-index; p620 is the only deployment.
- nixos_config: a flake input bump, plus two small host changes found in the
  same review: set `registerPublic = false` (the server is registered twice)
  and test `DRY_RUN` instead of the deprecated `DRY_RUN_CMD` in the
  activation script. Those are one-line changes and are exempt from this
  workflow; they ride the same deploy.

## Constraints

- No schema change that needs a manual migration; existing state on p620
  keeps working, or the indexer rebuilds what it must on its own.
- The sandbox may only get stricter. On a kernel without the newer
  Landlock scopes the worker keeps today's protection (best effort), as it
  does today for other Landlock features; the spec states the minimum.
- Approvals come from the owner; Codex and agy output is advice only.
- p510 is not touched.

## Open questions

- Ranking: word hits from `public.db` and `private.db` are fused as two
  lists, so their rank-1 hits tie regardless of BM25. Proposed: leave as is
  (the private server only) and document it.
- The tier split does not stop a local agent running as the owner, with a
  shell, from running `llm-index mcp --tier private` or reading
  `~/Documents`. Proposed: out of
  scope here; say so plainly in `docs/security.md`.
