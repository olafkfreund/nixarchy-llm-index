---
status: draft
issue: 18
intent: intent/2026-11-17-18-skip-worktrees.md
---

# Spec: skip linked git worktrees when scanning a root

Approved intent decisions:

- Always on, with no option.
- A configured root that is itself a worktree is still indexed.
- Work that exists only in a worktree stops being searchable.

## Design

### 1. Recognising a worktree: `src/worker/scan.rs`

Add `fn linked_worktree(dir: &OwnedFd) -> bool`:

- Read `.git` in `dir` with the existing `read_ignore(dir, ".git")`. That
  helper uses `statx` with `SYMLINK_NOFOLLOW`, requires a regular file, and
  opens it with `openat2` under `RESOLVE`, `NOFOLLOW` and a 1 MiB cap. So
  nothing outside the root is opened, and a `.git` directory or a symlink
  returns `None`.
- The first line must be `gitdir: <path>`. The path is compared as text and
  never opened: it usually points outside the root, and the sandbox could not
  open it anyway.
- It is a worktree when `<path>` ends in `<G>/worktrees/<name>` and `<G>`'s
  name is `.git` or ends in `.git`. This matches:
  - `<repo>/.git/worktrees/<name>`, written by `git worktree add` in a normal
    repository;
  - `<repo>.git/worktrees/<name>`, the same for a bare repository with the
    usual name.
- It does not match, so these stay indexed:
  - a submodule, `…/.git/modules/<name>`, including one whose own name is
    `worktrees` (`.git/modules/worktrees/foo`, where `<G>` is `modules`);
  - a checkout with a separate git directory that merely ends in
    `worktrees/<name>`, unless that directory's parent is named `*.git`;
  - a bare repository not named `*.git`. That fails safe: it is indexed, as
    today.

Add `fn skip_contents(dir: &OwnedFd) -> Option<&'static str>`. It returns
`Some("worktree")` when `linked_worktree` matches, else `Some("cache")` for
the existing `CACHEDIR.TAG` test, else `None`. This one helper is used in
every place below.

### 2. Full scans: `Walker::descend`

Replace the inline `CACHEDIR.TAG` block (`scan.rs:494-504`) with
`skip_contents(&child)`, reporting
`visit(&child_rel, &st, None, Some(reason))`:

- The folder is reported as skipped and not descended into.
- `walk()` never runs the check on the root itself, only on children. That
  is the approved "a configured root that is a worktree is still indexed".

### 3. The watcher: `src/worker/watch.rs`

A skipped folder keeps one watch on itself and none below it. That way a
`.git` file that appears, changes or goes away is always seen. There are
about 50 such folders on p620, so about 50 watches.

- **a. Adding watches.** `walk_subtree` calls `skip_contents` on each folder
  it opens along `rel` (except the root) and on the final one. When a folder
  matches, `add_watches` installs the watch on that folder only and does not
  descend. Then it checks `skip_contents` again on the same fd, after the
  watch is installed: a `.git` written between the first check and the watch
  is then caught, and handled as in (c).
- **b. Events below a skipped folder.** Before a pending event is flushed as
  `WatchEvent`, the watcher calls a new `Walker::under_skipped(rel) -> bool`.
  It opens each ancestor folder below the root with `open_dir`, as
  `walk_subtree` does, and checks `skip_contents`. A match drops the event.
  Events on files directly inside a watched worktree folder go through the
  same check.
- **c. A `.git` event** (create, modify, move or delete) in a watched folder
  `dir`, where `dir` is not the root:
  - If `dir` is now skipped, the watches below `dir` are removed. `dir`'s own
    watch is kept.
  - If `dir` is no longer skipped, `add_watches(Some(dir))` restores the
    watches below it.
  - Either way, a size-less `WatchEvent { relpath: dir }` is sent. The
    scheduler already turns that into `request_scan(i, now, TOUCH_DELAY)`
    (`sched.rs:539`). That scan removes the folder's files, or indexes them
    again.

  A `.git` event at the root itself is ignored: the root is never skipped.

Cost: (b) adds one `open_dir` and a `statx` per ancestor for each flushed,
debounced event. That is small next to the extraction the event triggers.

`Walker::allowed_event` (`scan.rs:527`) has no callers and stays as it is.

### 4. Removing what is already indexed, and keeping it out

- **Rows.** A skipped folder whose reason `store::protects` (`store.rs:93`)
  does not protect has its rows deleted at `scan_done` (`store.rs:598-624`).
  `worktree` and `cache` are not protected, so the first complete, accepted
  scan after the upgrade deletes the roughly 15,000 copies on p620, rows and
  chunks alike.
- **Queued and in-flight work.** A file queued by `queue_one` before the
  scan noticed the worktree would otherwise be extracted after the delete
  and come back. The fix is in `sched.rs`:
  - At an accepted `scan_done`, the root keeps
    `skipped_dirs: HashSet<String>`, the folders the scan reported as
    `worktree` or `cache`.
  - Entries under those folders are removed from `queue` and `queued`.
  - An extraction result for a path under them is not stored: in the
    `Message::Extracted` arm (`sched.rs:440-461`), `sink.extracted` is
    skipped for it. The job is still taken and `send_next` still runs.
  - `queue_one` ignores paths under them.
  - The set is replaced on every accepted scan, so a folder that stops being
    a worktree is indexed again after the next scan.

### 5. The identity evidence check: `src/index/sched.rs:752-781`

`evidence()` already leaves out files under protected skipped paths. It will
also leave out files under folders the scan skipped as `worktree` or `cache`.
Those files are missing on purpose, not because the disk is a different one.

- Folders left out earlier, by `exclude.paths`, `ignored` or `directory`,
  still count as today. They are configuration changes, not something found
  on the disk, so this task does not change them.
- The rule stays `total > 0`. If a disk consists only of skipped folders,
  nothing is left to compare, and it is still rejected.
- `(StoredUuid::Known(u), …)` never calls `evidence()`, so p620 and razer
  are not affected.

### 6. Docs

- `docs/reference.md`, "What is indexed": linked git worktrees are skipped.
  Give the rule, and say that submodules and a root that is itself a
  worktree are still indexed.
- `docs/architecture.md`, "Watching": one line saying the same applies to
  live events.

`status` and the closed reason list stay as they are. Skipped folders are not
reported per reason. The MCP tools, the tiers and the schema do not change.

## Alternatives rejected

- **Skip any folder whose `.git` is a file.** That also drops submodules,
  which the intent rules out.
- **Check only that the parent folder is named `worktrees`.** It matches a
  submodule named `worktrees` (Codex review).
- **Resolve `gitdir`, or read `commondir`.** Both are outside the root, and
  the sandbox denies access there.
- **Fix it per host with ignore rules or `exclude.paths`.** Worktrees get
  arbitrary names in arbitrary places (intent, Problem).
- **Mark `worktree` as protected.** The copies already indexed would never
  be removed.
- **Remove a worktree folder's own watch.** Then a `.git` that goes away is
  never seen, and watching never resumes (Codex review).
- **Add a config option.** The approved intent says always on.

## Risks

- **Mass removal at the first upgrade.** p620 loses about 14,700 files from
  `public.db` in one `scan_done` transaction, the same path as any large
  ignore change. That path runs no VACUUM, so the file does not shrink until
  a later VACUUM. Both hosts are affected.
- **Unusual `.git` files.** A hand-written `gitdir`, or a bare repository not
  named `*.git`, is not recognised. Those folders stay indexed, as today, so
  this fails safe.
- **Watcher timing.** A file flushed just before `.git` lands can be indexed
  once. The `.git` event in (3c) then triggers a scan, and (4) removes the
  file and keeps it out. The re-check in (3a) closes the setup window.

## Verification

**Unit tests, `scan.rs`** (temporary folders):

- a linked worktree, in both the normal and the bare `*.git` form, is
  reported as `worktree` and not descended into;
- submodules are descended into: one at `.git/modules/foo` and one at
  `.git/modules/worktrees/foo`;
- a `.git` folder and a `.git` symlink behave as before;
- a root that is itself a worktree is indexed;
- `walk_subtree` on a worktree folder visits only the folder.

**Unit tests, `watch.rs`:**

- `under_skipped` is true below a worktree and false below a submodule;
- a `.git` file that appears gives a size-less `WatchEvent` for the folder,
  and its watch is kept;
- removing `.git` restores the watches, and a later edit inside gives a
  normal event;
- changing `.git` at the root keeps every watch, and a later edit gives an
  event.

**Unit tests, `sched.rs`:**

- a file queued, then skipped by a scan, is not extracted, and a late result
  for it is discarded;
- after the next scan where the folder is no longer skipped, the file is
  queued again;
- `evidence()` ignores a `worktree` or `cache` subtree, and still rejects
  when nothing else matches.

**Store test:** after a scan that skips a worktree, its `files` and `chunks`
rows are gone and the main checkout's rows are intact.

**Checks:** `cargo test`, `cargo clippy -- -D warnings`, and
`nix flake check` (VM and plugin checks).

**Live on p620,** after the nixos_config bump:

1. `llm-index status` shows `source` at about 37,100 files, down from
   51,820, and `ok`.
2. A string that exists in both `GitHub/nixarchy` and its worktrees gets no
   hits under the worktree paths.
3. Add a scratch worktree under `GitHub/` with `git worktree add`, wait 30 s,
   and check that `recent(1)` lists none of its files. Then remove it.
