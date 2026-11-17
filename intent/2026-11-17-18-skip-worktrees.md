---
status: draft
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

Configuration cannot fix this. `exclude.paths` takes fixed absolute paths, and
worktrees come and go under new names.

Build output is not the problem. Gitignore and the built-in list (`SKIP_DIRS`
in `src/worker/scan.rs`) already filter it: only 28 `dist/` and 53 `build/`
files are indexed.

## Proposed outcome

- A scan no longer indexes the contents of a linked git worktree.
- Files already indexed from one are removed on the next scan.
- The main checkout keeps being indexed as today.
- Status or the docs say that this happens, so a missing worktree in the
  results is not a surprise.

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
- Removing about 15,000 files must not trip the #14 identity guard on the next
  restart. The 90% evidence check counts indexed files.
- No change to the MCP tools, the tiers or the database schema unless the spec
  shows one is needed.

## Open questions

1. Should this be on by default with a way to turn it off (an option to
   index worktrees again), or always on?
2. Should a root whose top folder is itself a linked worktree be indexed? The
   rule above would skip it entirely.
