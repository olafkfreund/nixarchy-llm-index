---
status: draft
issue: 18
spec: spec/2026-11-17-18-skip-worktrees.md
coders:
  1: codex   # small, tricky detection logic and walker changes in scan.rs
  2: codex   # watcher edge cases (root exemption, setup race, marker removal)
  3: coder   # scheduler bookkeeping plus tests, following existing patterns in sched.rs
  4: coder   # docs only
reviewers:
  - codex    # independent read of the full branch diff against this plan
  - agy      # second independent read; it timed out on the spec, so its prompt names the files
---

# Plan: skip linked git worktrees when scanning a root

## Approved decisions (self-contained)

- A folder below a root is a **linked worktree** when:
  - its `.git` is a regular file whose first line is `gitdir: <path>`;
  - `<path>` ends in `<G>/worktrees/<name>`;
  - `<G>`'s name is `.git` or ends in `.git`.

  The path is compared as text and never opened.
- **What stays indexed:**
  - submodules (`…/.git/modules/…`, including a submodule named
    `worktrees`);
  - a `.git` folder or symlink;
  - a root that is itself a worktree.
- **Always on.** There is no option.
- **Skipped folders** are reported as `worktree`, and their contents are not
  walked. `cache` (`CACHEDIR.TAG`) shares the helper. `worktree` and `cache`
  are not protected, so `scan_done` deletes their rows.
- **Watcher:**
  - A skipped folder keeps one watch on itself and none below it.
  - When a watch is installed, the folder is re-checked.
  - Events below a skipped folder are dropped.
  - A `.git` event in a non-root folder updates its watches, either way,
    and sends a size-less `WatchEvent` for the folder, which triggers a
    scan.
  - A `.git` event at the root is ignored.
- **Scheduler:**
  - An accepted scan records the folders it skipped as `worktree` or
    `cache`.
  - Queued entries under them are dropped.
  - `queue_one` ignores paths under them.
  - `Extracted` results under them are not stored.
  - `evidence()` leaves them out.
  - `total > 0` still applies.
- **No change** to status reasons, the MCP tools, tiers or the schema. The
  docs gain one rule each in `reference.md` and `architecture.md`.

## Steps

### 1. `src/worker/scan.rs`: detection and the walker

**Change:**

- Add `fn linked_worktree(dir: &OwnedFd) -> bool`. It reuses
  `read_ignore(dir, ".git")` (`scan.rs:173-211`): a regular file only, no
  symlink following, capped at 1 MiB. It takes the first line, strips the
  `gitdir:` prefix and trims whitespace into `p: &Path`. It is true exactly
  when:

  ```rust
  p.parent().and_then(Path::file_name) == Some("worktrees".as_ref())
      && p.parent()
          .and_then(Path::parent)
          .and_then(Path::file_name)
          .is_some_and(|g| g.as_bytes().ends_with(b".git"))
  ```

  This matches `.git` itself too, because `.git` ends with `.git`.
- Add `pub(crate) fn skip_contents(dir: &OwnedFd) -> Option<&'static str>`:
  `worktree` first, then the existing `CACHEDIR.TAG` test as `cache`.
  `watch.rs` imports it with `use super::scan::skip_contents`.
- In `Walker::descend`, replace the inline `CACHEDIR.TAG` block
  (`scan.rs:494-504`) with
  `if let Some(r) = skip_contents(&child) { visit(&child_rel, &st, Some(&child), Some(r))?; continue; }`.
  Pass `Some(&child)`: the watcher needs the fd to watch the skipped folder
  itself. The scanner's visitor ignores the fd (`scan.rs:598`, `|rel, st,
  _, reason|`), so scans are unaffected.
- In `walk_subtree` (`scan.rs:379-405`), check each folder opened along
  `rel` (excluding the root) and the final fd. If one matches, call
  `visit(<that rel>, &st, Some(&fd), Some(reason))` once and return without
  descending. The watcher needs the fd to keep one watch on the folder. For
  `rel == ""`, do not check.

**Tests,** in `mod tests`, using temporary folders under
`std::env::temp_dir()` named per test and process, as in
`swapped_directory_is_not_followed` at `scan.rs:850`:

- `gitdir: /x/repo/.git/worktrees/a` is skipped;
- `gitdir: /x/repo.git/worktrees/a` is skipped;
- `gitdir: ../.git/modules/foo` is walked;
- `gitdir: ../.git/modules/worktrees/foo` is walked;
- a `.git` folder is walked as before;
- a `.git` symlink to a file holding a worktree gitdir is walked, and the
  link is not followed;
- `walk()` on a root whose own `.git` is a worktree file visits its files;
- `walk_subtree` on a worktree folder visits only that folder, with reason
  `worktree`.

**Check:** `cargo test -p llm-index worker::scan` and
`cargo clippy --all-targets -- -D warnings`.

**Traps:**

- Keep every open fd-relative, under `RESOLVE`. Never open the `gitdir`
  target.
- `read_ignore` prints to stderr on oversized files. That is fine, but do
  not add new `eprintln!` lines per folder.
- Only `descend` and `walk_subtree` change. `reason()` and `allowed_event`
  do not.

### 2. `src/worker/watch.rs`: the watcher

**Change:**

- **Adding watches.** In `add_watches`'s `add` callback (`watch.rs:66-107`),
  the early return `if reason.is_some() || …` (`watch.rs:70`) changes:
  - A visit with reason `worktree` or `cache` and `Some(fd)` installs the
    watch on that folder as usual. The walker does not descend into it.
  - Other reasons still return early.
- **Re-check after every install.** After the watch for any non-root folder
  is installed, call `skip_contents(fd)` again and compare it with the
  reason the folder was visited with:
  - **It became skipped**, because `.git` appeared between the walker's
    check and the watch. Record the folder in a `changed: Vec<PathBuf>`.
    After the walk, prune the watches below it, keep its own, and queue a
    size-less pending entry for it. This is the same handling as a `.git`
    event.
  - **It stopped being skipped**, because the marker went away. Record it
    for a follow-up `add_watches(Some(rel))` and a size-less pending entry.

  After the watch is installed, any later change raises an inotify event,
  handled below.
- **Dropping events.** Add
  `pub(crate) fn under_skipped(&mut self, rel: &Path) -> bool` to `Walker`
  in `scan.rs`, directly after `allowed_event` (`scan.rs:527`). Step 2 edits
  `scan.rs` too, so steps 1 and 2 run in order on the same branch. It opens each ancestor below
  the root with `open_dir` and returns true when `skip_contents` matches.
  Call it in the flush loop (`watch.rs:252-265`) before sending a
  `WatchEvent`, and drop the event if it returns true.
- **`.git` events.** In the event loop, next to the ignore-file case
  (`watch.rs:203-208`), handle an event named `.git` in a watched `dir`
  where `dir != ""`. Re-open `dir` and evaluate `skip_contents`:
  - If it is skipped, remove the watches strictly below `dir` and keep
    `dir`'s own. This needs a variant of `remove_watch` that keeps the top
    one, or a re-add of `dir` after removal.
  - If it is not skipped, push `dir` to `additions`.
  - Either way, insert a size-less pending entry for `dir`. The flush sends
    `WatchEvent { relpath: dir, size: None, mtime_ns: None }`; do not apply
    `under_skipped` to this entry. Then `continue`, so `dir/.git` itself is
    never queued as a file.

**Tests,** in `watch.rs` `mod tests`, using the pattern of
`vanished_subtree_does_not_degrade_watcher` at `watch.rs:322`:

- `under_skipped` is true for `wt/a/b.md` below a worktree and false below a
  submodule;
- writing `.git` into a watched folder yields one size-less event for the
  folder, and a later file edit in it yields nothing;
- removing `.git` then editing a file yields a normal event for the file;
- changing `.git` at the root, then editing a file, still yields the file's
  event;
- **initial setup:** a worktree present before the watcher starts gets
  exactly one watch, on the folder itself. Removing its `.git` restores the
  watches below it;
- **setup race,** made deterministic with a test-only hook: in tests the
  `add` callback runs a closure after the walker check and before the
  install, and that closure writes `.git`. The folder ends with only its
  own watch and one size-less pending entry.

**Check:** `cargo test -p llm-index worker::watch` and clippy as in step 1.

**Traps:**

- `remove_watch` at `watch.rs:127` removes a subtree including its top
  folder. Do not call it with `""`.
- Activation of watcher changes is tested only in-process: the watcher runs
  sandboxed in production, so open nothing new outside the root fd.

### 3. `src/index/sched.rs`: scheduler

**Change:**

- **New field.** Add `skipped_dirs: HashSet<String>` to `RootRt`
  (`sched.rs:83-107`), initialised empty where `failed` is (`sched.rs:167`).
- **On an accepted scan** (`sched.rs:815-835`, before `queue_extracts`).
  Guard this with `if accepted`, because lines 815-827 also run for partial
  scans.
  - Set `skipped_dirs` from `buf.skipped` for the reasons `worktree` and
    `cache`.
  - Drop entries of `queue` and `queued` under any of them. Use
    `store::under_any`.
- **Ignore new work under them:**
  - `queue_one` (`sched.rs:853`) returns early when `rel` is under
    `skipped_dirs`.
  - In the `Message::Extracted` arm (`sched.rs:440-461`), skip
    `sink.extracted` when the path is under `skipped_dirs`. Still take the
    job and call `send_next`.
- **Evidence.** In `evidence()` (`sched.rs:752-781`), extend `protected` to
  also include paths whose reason is `worktree` or `cache`.

**Tests,** in `sched.rs` `mod tests`, using `identity_scan` (`sched.rs:1504`)
and the `Mem` sink (`sched.rs:1298`):

- a watch event queues `wt/f.md`. Then an accepted scan skips `wt` as
  `worktree`. The queue no longer holds it, and an `Extracted` for it stores
  nothing;
- the next accepted scan without that skip lets `queue_one` queue it again;
- `evidence` with `StoredUuid::Absent` and `Verdict::Confirm`:
  - 50% of the files are under a `worktree` skip and the rest match:
    accepted;
  - the same with a `cache` skip: accepted;
  - all of them are under the skip: rejected (`total == 0`);
  - 50% under an `ignored` skip: rejected. The other reasons are unchanged.
- a partial scan with a `worktree` skip leaves `skipped_dirs` unchanged.

**Store test,** in `src/index/store.rs` `mod tests` (`store.rs:856`), with the
real SQLite store:

- index `repo/a.md` and `wt/a.md`, each with chunks;
- run a complete `scan_done` that lists `repo/a.md` and has skipped
  `("wt", "worktree")`;
- check that `files` and `chunks` have no rows for `wt/a.md`, and that
  `repo/a.md`'s rows are intact.

**Check:** `cargo test -p llm-index index::` and clippy.

**Traps:**

- Do not mark `worktree` or `cache` as protected in `store::protects`. That
  would stop their rows from being deleted.
- `skipped_dirs` changes only on an accepted scan. A rejected or partial
  scan leaves it as it was.

### 4. Docs

**Change:**

- `docs/reference.md`, "What is indexed" (around lines 324-334): add the
  worktree rule, saying that submodules and a root that is itself a worktree
  are still indexed.
- `docs/architecture.md`, "Watching" (lines 59-67): one line saying the
  watcher applies the same rule to live events.

**Check:** read `git diff -- docs/` against the approved decisions above.
No line may contradict them. `nix flake check` in step 5 builds the package,
whose fileset includes the docs.

**Traps:** keep the docs path-free, with no host paths. That is the
existing style.

### 5. Lead: review, PR, merge

**Run:**

- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `nix flake check`
  (VM and plugin checks).
- Reviews: a fresh Opus reviewer given only this plan and `git diff main`,
  plus Codex and agy (`reviewers:`). The Codex review is a new
  `codex exec --ephemeral` session, never the one that implemented steps 1
  and 2.
- Open the PR linking the intent, spec and plan. Wait for CI and merge on
  your OK.

**Traps:** commits, the PR and the merge are the lead's. Implementers never
stage or commit.

### 6. Lead: deploy to p620 and razer

nixos_config gets a one-line rev bump in `flake.nix:153` plus the
`flake.lock` change. This is exempt from the artifact workflow.

**Route:**

1. A worktree, then `just validate` and a PR. Wait for CI.
2. After the merge, `git pull --ff-only` in `~/.config/nixos`.
3. Announce on the agent bus.
4. `just p620`, then `just deploy-via-p620 razer`.

**Live check:**

- `llm-index status`: `source` is `ok` with about 37,100 files.
- A string present in `GitHub/nixarchy` and its worktrees gets no
  worktree-path hits.
- A scratch `git worktree add` under `GitHub/` shows none of its files in
  `recent(1)` after 30 s. Then remove it.
- razer is `ok`.

## Tests

| Command | Expected |
| --- | --- |
| `cargo test` | all pass, including the tests from steps 1-3 |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `nix flake check` | all checks pass, including VM `disk-identity` and plugin |

## Rollback

- **Code:** revert the merge commit in nixarchy-llm-index.
- **Hosts:** set nixos_config's pinned rev back to `54e0fbe` and redeploy.
  The next scan indexes the worktrees again. No schema or data migration is
  involved.
