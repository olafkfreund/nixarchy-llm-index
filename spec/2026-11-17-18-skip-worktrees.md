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
  helper already uses `statx` with `SYMLINK_NOFOLLOW`, requires a regular
  file, and opens it with `openat2` under `RESOLVE`, `NOFOLLOW` and a 1 MiB
  cap. So nothing outside the root is opened, and a `.git` directory or
  symlink returns `None`.
- The first line must be `gitdir: <path>`.
- It is a worktree when the parent of `<path>`'s last component is named
  `worktrees`:

  ```rust
  Path::new(p.trim()).parent().and_then(Path::file_name) == Some("worktrees".as_ref())
  ```

  This matches `<repo>/.git/worktrees/<name>` and the bare-repo form
  `<repo>.git/worktrees/<name>`.
- A submodule (`…/.git/modules/<name>`) does not match, so it stays indexed.
- The path is compared as text and never opened. It usually points outside
  the root, and the sandbox could not open it anyway.

Add `fn skip_contents(dir: &OwnedFd) -> Option<&'static str>`. It returns
`Some("cache")` for the existing `CACHEDIR.TAG` test, `Some("worktree")` when
`linked_worktree` matches, and `None` otherwise. This one helper is then used
in the three places below.

### 2. Full scans: `Walker::descend`

Replace the inline `CACHEDIR.TAG` block (`scan.rs:490-504`) with
`skip_contents(&child)`, reporting
`visit(&child_rel, &st, None, Some(reason))`:

- The child directory is reported as skipped with reason `worktree` and is
  not descended into.
- `walk()` never runs the check on the root itself, only on children. That
  is the approved "configured root that is a worktree is still indexed".

### 3. The watcher: `src/worker/watch.rs`

There are three gaps. The first two also apply to `CACHEDIR.TAG` today.

**a. A new folder** (`CREATE` or `MOVED_TO`) goes to `add_watches`, then
`Walker::walk_subtree`, which never checks the subtree's own top folder.
`walk_subtree` will call `skip_contents` on each folder it opens along `rel`
(except the root) and on the final one. If any match, it returns `Ok(())`
without visiting, so no watches are added.

**b. Events under a worktree that is already watched.** Before a pending
event is flushed as `WatchEvent`, the watcher calls a new
`Walker::under_skipped(rel) -> bool`. It opens each ancestor folder below
the root with `open_dir`, the same way `walk_subtree` does, and checks
`skip_contents`. A match drops the event. This also covers the case where
git creates the folder first and writes the `.git` file a moment later: the
folder's files are still debounced in `pending` when `.git` lands.

**c. A `.git` file that appears or changes in a watched folder.** This is
handled like the existing ignore-file case (`watch.rs:203-208`):

- An event named `.git` in a watched `dir`, where `dir` now passes
  `linked_worktree`, removes the watches on `dir` and below with
  `remove_watch`.
- It then sends `WatchEvent { relpath: dir, size: None, mtime_ns: None }`.
  The scheduler already turns that into `request_scan(i, now, TOUCH_DELAY)`
  (`sched.rs:539`), and that scan removes the folder's indexed files.

Cost: (b) adds one `open_dir` and a `statx` per ancestor for each flushed
event, and flushes are debounced. That is small next to the extraction the
event triggers.

`Walker::allowed_event` (`scan.rs:527`) has no callers. It stays as it is;
removing it is not part of this task.

### 4. Removing what is already indexed

A skipped folder whose reason is not protected by `store::protects`
(`store.rs:93`: `mount`, errno reasons) has its indexed files deleted at
`scan_done`. That is the existing path that `cache`, `ignored` and
`directory` already use. `worktree` is not protected, so the first scan after
the upgrade removes the roughly 15,000 copies on p620.

### 5. The identity evidence check: `src/index/sched.rs:756-781`

`evidence()` already leaves out files under protected skipped paths. It
will also leave out files under paths skipped as `worktree`: they are
missing because they were skipped on purpose, not because the disk is a
different one.

- The rule stays `total > 0`, so a wrong disk made entirely of worktrees
  still fails.
- `(StoredUuid::Known(u), …)` never calls `evidence()`, so p620 and razer
  are unaffected either way.
- This only matters for a root with no stored UUID and a `Confirm` verdict.

### 6. Docs

- `docs/reference.md`, "What is indexed": linked git worktrees are skipped
  (a `.git` file whose `gitdir` ends in `worktrees/<name>`). Submodules and
  a root that is itself a worktree are still indexed.
- `docs/architecture.md`, "Watching": one line on the same rule for live
  events.

`status` and the closed reason list do not change, because skipped folders
are not reported per reason. The MCP tools, tiers and schema do not change
either.

## Alternatives rejected

- **Skip any folder whose `.git` is a file.** That also drops submodules,
  which the intent rules out.
- **Resolve `gitdir` and check that it exists.** It points outside the root,
  where the sandbox denies access, and it costs an extra open for nothing.
- **Use ignore rules or `exclude.paths` per host.** Worktrees get arbitrary
  names in arbitrary places (intent, Problem).
- **Mark `worktree` as a protected reason.** The copies already indexed
  would then never be removed.
- **Add a config option.** The approved intent says always on.

## Risks

- **Mass removal on first upgrade.** p620 loses about 14,700 files from
  `public.db`. There is no VACUUM on that path (#8 is VACUUM on root
  rename), so the file only stops growing; it does not shrink until a later
  VACUUM. The deletes run in one `scan_done` transaction, as they do for any
  large ignore change. Both hosts are affected.
- **Unusual `.git` files.** Hand-written gitdir paths, or worktrees under a
  folder that is not named `worktrees`, are not recognised. They stay
  indexed, which is today's behaviour, so this fails safe.
- **Watcher timing.** If git writes `.git` after a file has already been
  flushed, that file can be indexed once. The `.git` event in (3c) triggers
  a scan, which removes it. The worst case is a few seconds of duplicate
  hits.

## Verification

Unit tests:

- **`scan.rs`, in temporary directories:**
  - a linked worktree is reported as `worktree` and not descended into;
  - a submodule `.git` file is descended into;
  - a `.git` directory behaves as before;
  - a `.git` symlink is not followed or treated as a worktree;
  - a root that is itself a worktree indexes its files;
  - `walk_subtree` on a worktree folder visits nothing.
- **`watch.rs`:**
  - `under_skipped` is true for a path below a worktree and false for one
    below a submodule;
  - a `.git` file appearing turns into a size-less `WatchEvent` for the
    folder.
- **`sched.rs`:** `evidence()` ignores a `worktree`-skipped subtree and
  still rejects when nothing else matches.

Repository checks: `cargo test`, `cargo clippy -- -D warnings`, and
`nix flake check`, including the VM checks and the plugin check.

Live on p620, after the nixos_config bump:

1. `llm-index status` shows `source` going from 51,820 files to about
   37,100, and `ok`.
2. A search for a string that exists in both `GitHub/nixarchy` and its
   worktrees returns no hits under the worktree paths.
3. `git worktree add` a scratch worktree under `GitHub/`, wait 30 s, and
   check that `recent(1)` lists none of its files. Then remove it.
