---
status: approved
issue: 18
author: olafkfreund
---

# Intent: skip linked git worktrees when scanning a root

## Problem

Linked git worktrees in a root are indexed as if they were separate
repositories. A linked worktree is a folder whose `.git` is a file containing
`gitdir: <repo>/.git/worktrees/<name>`.

Measured on p620 on 2026-11-17, read-only against `public.db`:

- 14,695 of the 51,852 indexed files in `source` (28%) are inside 50 linked
  worktrees. Examples are `GitHub/nixarchy-wt/*`, `GitHub/nixarchy-2983`,
  `GitHub/nixarchy-flux-69` and `Calitti/refs-engine-293`.
- Each of them is a near-copy of its main checkout, so a search returns the
  same passage several times. That pushes other repositories' hits out of the
  top 10.
- They make the index bigger: `public.db` is 1.7 GB.
- Worktrees are created and deleted all the time, so they cause churn.
  Deleted `nixarchy-flux-*` worktrees took 7,204 files out of the #14
  evidence check on p620, and the root needed a one-off `adoptUuid`.

Configuration only covers this partly. `exclude.paths` takes fixed absolute
paths, and ignore files (`.llmindexignore`, gitignore) can match a stable
container folder or naming pattern. But worktrees are created under arbitrary
names in arbitrary places, so every new one would need a new rule.

In this measured index, build output was a small part: 28 `dist/` and 53
`build/` files. That is because ignore rules and the built-in list (`SKIP_DIRS`
in `src/worker/scan.rs`) happened to cover it here, not a general guarantee.

## Proposed outcome

- A scan no longer indexes the contents of a linked git worktree.
- The watcher does not either. That includes a worktree created while the
  service runs, and a folder that becomes a worktree after it was already
  watched (its `.git` file appears later).
- Files already indexed from one are removed on the next scan.
- The main checkout keeps being indexed as today.
- `docs/reference.md` ("What is indexed") says that this happens, so a
  missing worktree in the results is not a surprise. The closed list of
  status reasons does not change unless the spec shows a need.

## Affected users and systems

- The scan worker (`src/worker/scan.rs`) and the watcher.
- Every root on every host. In practice this is p620's `source` (about 15,000
  fewer files) and razer's `source`.
- Agents searching the public tier see fewer duplicate hits.

## Constraints

- The scan worker's sandbox and its rules for following paths (`RESOLVE`:
  beneath, no symlinks, no crossing mounts) stay the same. Recognising a
  worktree must not open anything outside the root.
- A `.git` *file* is also used by git submodules (`gitdir: ../.git/modules/…`).
  Submodules are real content and must stay indexed, unless the spec decides
  otherwise for a stated reason.
- Removing about 15,000 files must not make the #14 identity guard reject a
  scan, not only the first cleanup scan but any later one. Today the 90%
  evidence check (`src/index/sched.rs:752-827`) runs only for a root with no
  stored filesystem UUID. With `Verdict::Ok` it then only postpones storing
  the UUID; with `Verdict::Confirm` (the `/dev` name changed) it rejects the
  scan.
  - p620's and razer's `source` roots both have a stored UUID, so neither is
    affected today.
  - A root without one would be: skipping 14,695 of 51,852 files leaves at
    most 71.7% matching.
  - Wrong-disk rejection must keep working.
- No change to the MCP tools, the tiers or the database schema unless the spec
  shows one is needed.

## Open questions

1. Should this always be on, like `SKIP_DIRS`, or on by default with an
   option to index worktrees again? **Recommendation:** always on. Someone
   who wants one worktree indexed can add it as its own root.
2. Should a root whose top folder is itself a linked worktree be indexed?
   **Recommendation:** yes. The rule applies only to folders found below a
   root, never to a root you configured yourself.
3. A worktree can hold work that is not in the main checkout yet, such as an
   unmerged branch. That work stops being searchable. Is that acceptable?
   **Recommendation:** yes. That is what the change is for, and question 2
   gives a way back for one worktree.
