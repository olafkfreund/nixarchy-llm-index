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
   scan. A restart does not help. One unreadable subdirectory in a new root
   is enough.
2. **Temporary extract failures spin** (confirmed). On a transient `Failed`,
   `send_next` runs before the root's `hold` is set, and `next_allowed` never
   checks `hold`. Two failing files on a stale network share retry each
   other at full speed.
3. **`get` drops spreadsheet sheets** (confirmed). `next` carries `sheet`;
   passing it back filters to that sheet only, and the call then returns
   `next: null`. Without `sheet`, `start` drops low rows of every sheet.
   Sheet names are escaped in `search` but raw in `get` and `next`.
4. **HTML pre-pass panics** (confirmed). `src/extract/html.rs`
   `skip_rawtext_element` slices `html[lt+2..lt+2+len]` at a byte offset
   that can fall inside a multi-byte character. Ordinary non-English pages
   with inline scripts kill the worker.
5. **A vanished directory degrades the watcher** (traced). `walk_subtree`
   propagates the error for the new directory itself, so `mktemp -d` and a
   quick `rm` puts the root into polling with a full re-walk.
6. **Bad durations panic or loop** (confirmed). `parse_duration`
   (`src/index/mod.rs`) panics on a multi-byte last character and can
   overflow; `Config::validate` does not check `interval` or `backoff_max`,
   so a bad value restart-loops the indexer while `mcp` keeps serving.
7. **A file edited between scan and extract loses its good row** (traced).
   `"changed"` is not treated as transient, so the indexed row is replaced
   by a failed one.
8. **A parked solo retry starves extraction** (traced). While `solo` is
   non-empty, `send_next` stops every worker after one file, for up to
   `backoffMax`.
9. **Smaller correctness gaps** (traced): `max_file_mb = 0` means "skip all"
   in scan and "unlimited" in extract; mtime arithmetic overflows in scan and
   watch but saturates in extract; `meta.model_rev` is never read, so a model
   change leaves old vectors.

### Hardening

10. **Worker sandbox scopes** (confirmed). `src/worker/sandbox.rs` `restrict`
    handles filesystem and TCP access only. It sets no Landlock scope for
    abstract UNIX sockets or signals, and the units allow `AF_UNIX`. A parser
    exploit could reach the session bus or agent sockets.
11. **Uncapped titles, headings and sheet names** (confirmed). They are
    escaped but have no length cap and no newline or control-character
    cleanup. A 200 KB `title:` reaches every search hit, and a huge heading
    copied into every chunk can exhaust worker memory.
12. **Control characters in text** (traced). `sanitize` strips invisible
    characters only; ESC, OSC and other C0/C1 controls after the first 8 KiB
    survive, plus a few invisible code points it misses.
13. **`embed_cache` grows forever** (confirmed). Nothing deletes from it, so
    vectors of deleted files, or files moved to private, stay in `public.db`.
14. **Denylist gaps** (traced). Common secret files with indexable
    extensions are not denied: `nix.conf`, `age/keys.txt`, `rclone.conf`,
    `containers/auth.json`, `credentials.tfrc.json`, `secrets.yaml`.

### Scale and robustness

15. **Tier move loads a whole root into memory** (confirmed):
    `src/index/store.rs` `move_tier`.
16. **One full count per extracted file** (confirmed): `indexed_files` runs
    `count(*)` where only "any?" is needed.
17. **Store write errors are dropped** (confirmed): `Sink::extracted` and
    `failed` log and return `()`, and there is no `busy_timeout`.
18. **Quadratic paths** (traced): `queue_one` searches the queue linearly;
    `get` rebuilds every line per call; the coordinator rescans its pipe
    buffer for a newline on every read.
19. **Undocumented skips** (confirmed): directories named `vendor`, `target`,
    `venv`, `third_party` or starting with `result` are skipped at any depth,
    and `docs/reference.md` does not say so.

## Proposed outcome

- Each item above is fixed or, where a fix is not worth it, documented with
  the reason. Each fix that changes behaviour has a test that fails before
  and passes after.
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
- The sandbox may only get stricter.
- Approvals come from the owner; Codex and agy output is advice only.
- p510 is not touched.

## Open questions

- Ranking: word hits from `public.db` and `private.db` are fused as two
  lists, so their rank-1 hits tie regardless of BM25. Proposed: leave as is
  (the private server only) and document it.
- The tier split does not stop a local agent with a shell from running
  `llm-index mcp --tier private` or reading `~/Documents`. Proposed: out of
  scope here; say so plainly in `docs/security.md`.
