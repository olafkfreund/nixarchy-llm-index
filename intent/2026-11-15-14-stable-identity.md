---
status: draft
issue: 14
author: olafkfreund
---

# Intent: a root keeps its identity when the kernel renames its disk

## Problem

llm-index guards each root with a mount identity. Before it trusts a
scan, the filesystem under the root must be the one it indexed before.
This stops a missing mount (an empty directory where a disk should be) from
looking like "every file was deleted" and wiping the index. The identity is
the mount's filesystem type, mount root, mount point and **source**, taken
from `/proc/self/mountinfo` (`src/worker/scan.rs:99-131`). It is stored per
root in `roots.share_id`.

For a local disk, the source is the kernel device name (`/dev/sdc1`). That
name is not stable: the kernel numbers disks in the order they appear, which
can change between boots. On p620, after a restart, the disk holding
`/mnt/code` came up as `/dev/sdd1` instead of `/dev/sdc1`. `verdict()`
(`src/index/shares.rs:57-81`) then reported `Mismatch`, and the `source`
root has been `offline` with `identity mismatch` since 2026-11-14 14:31.

- **Effect:** code search on p620 serves a frozen snapshot, and new or
  changed files are not indexed.
- **No recovery path:** nothing clears it short of renaming the root, which
  forces a full reindex (about 54k files on p620).
- **Not noticed:** it was only spotted by a manual `llm-index status` (#15
  adds a desktop indicator).

## Proposed outcome

- **The same filesystem keeps its identity.** A root keeps its identity
  across restarts, as long as the same filesystem is mounted at the same
  place, even when the kernel gives the disk a different `/dev/sdX` (or
  `nvme…`) name.
- **The guard still works.** A really different filesystem still gives
  `Mismatch` and keeps the root offline. That covers a missing mount with
  the parent filesystem underneath, and a different disk at the mount point.
- **Existing roots recover without a reindex.** That includes p620's
  `source` today, which was stored with the old, unstable source.
- **Network shares behave as now.** Their source (`server:/export`) is
  already stable.

## Affected users and systems

- This repository: the mount identity (scan worker), the verdict, the stored
  `share_id`, and the docs that describe the identity guard.
- Hosts that run llm-index: p620 (broken now) and razer (its `/extdisk` is a
  second NVMe, so it's exposed to the same renaming).
- The `share` setting, which names a mount source for network roots. It must
  keep working.

## Constraints

- **Never weaker.** No change may let a missing or wrong filesystem be
  adopted in a way that deletes index entries for files that still exist on
  the real disk.
- **Sandbox unchanged.** The scan worker's Landlock and seccomp sandbox is
  not widened just to read device metadata. If the stable key needs
  privileges the worker lacks, the unsandboxed coordinator gets it or the
  design picks another key.
- **Upgrade in place.** Existing databases upgrade without a manual step and
  without a reindex.
- **Fails closed.** A filesystem that offers no stable key behaves as today.
- **Usual rules.** Agents don't commit or deploy, and p510 is out of scope.

## Open questions

1. **How existing roots migrate.** p620's stored identity holds
   `/dev/sdc1`, so it can't be matched against a new stable key directly.
   The draft assumes a one-time migration: a stored `/dev/…` source that
   differs **only** in the device name (same filesystem type, mount root and
   mount point) is accepted once and rewritten with the stable key. Is a
   wrong-disk adoption, which costs a reindex of that root but no data, an
   acceptable residual risk for that one upgrade?
2. **Which stable key.** Candidates are:
   - the filesystem UUID from the kernel (`FS_IOC_GETFSUUID` on the root's
     file descriptor);
   - `/dev/disk/by-uuid`;
   - `statfs` `f_fsid`, which is UUID-derived on ext4 and btrfs but
     device-number-derived on XFS, so not stable there.

   The spec picks one. The intent only needs "stable across restarts for
   ext4, btrfs and XFS on p620 and razer".
