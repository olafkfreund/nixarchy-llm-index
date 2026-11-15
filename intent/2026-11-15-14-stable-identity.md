---
status: draft
issue: 14
author: olafkfreund
---

# Intent: a root keeps its identity when the kernel renames its disk

## Problem

llm-index guards each root with a mount identity. Before it trusts a
scan, the filesystem under the root must be the one it indexed before.
This stops a missing or wrong mount (an empty directory, or another disk,
where the real disk should be) from looking like "every file was deleted"
and wiping that root's index. For a public root, it also stops a wrong disk
from being indexed into the tier cloud agents read.

The stored identity (`roots.share_id`, `src/index/store.rs:677-690`) is four
fields from `/proc/self/mountinfo` (`src/worker/scan.rs:99-131`):

- filesystem type;
- mount root;
- mount point;
- **source**.

The mount ID and submounts are compared only within one scan and are not
stored, which is correct, since they change across boots.

For a local disk, the source is a kernel device name such as `/dev/sdc1`.
That name is not stable: disks are numbered in the order they appear, which
can change between boots. The same applies to `/dev/nvme…` names, numbered
device-mapper nodes (`/dev/dm-N`), the member device shown for a
multi-device btrfs, and `fuseblk` mounts.

On p620, after a restart, the disk holding `/mnt/code` came up as
`/dev/sdd1` instead of `/dev/sdc1`. `verdict()`
(`src/index/shares.rs:57-81`) reports `Mismatch`. The `source` root has been
`offline` with `identity mismatch` since 2026-11-14 14:31:

- **Effect:** code search on p620 serves a frozen snapshot, and new or
  changed files are not indexed.
- **No recovery path:** nothing clears it short of renaming the root, which
  forces a full reindex (about 54k files on p620). An intentional disk
  replacement has the same dead end.
- **Not noticed:** it was only spotted by a manual `llm-index status` (#15
  adds a desktop indicator).

## Proposed outcome

- **The same filesystem keeps its identity.** A local root keeps its
  identity across restarts as long as the same filesystem is mounted at the
  same place, whatever device name the kernel gives it. This holds for ext4,
  btrfs and XFS, the filesystems on p620 and razer.
- **The guard still works.** A different filesystem still gives `Mismatch`
  and keeps the root offline. That covers:
  - a missing mount with the parent filesystem underneath;
  - a different disk at the mount point;
  - an empty disk of the same type.
- **Existing roots move to the stable identity without a reindex,** but only
  with evidence that it's the same filesystem, not just a matching mount
  point. A root that can't show that evidence stays offline, as today.
- **The owner gets an explicit, deliberate way** to accept the current
  filesystem for a root that is offline with `identity mismatch`. This covers
  an intentional disk replacement or a migration without enough evidence.
- **Network shares behave as now.** That includes the `share` setting.

## Affected users and systems

- This repository:
  - the identity the scan worker reports;
  - the identity the coordinator checks before it starts scan, extract and
    watch workers (`src/index/sched.rs`, `shares.rs:28-35`);
  - the verdict;
  - the stored `share_id`;
  - the docs.

  The docs don't describe the identity guard today, so it needs new wording.
- Hosts that run llm-index: p620 (broken now) and razer (its `/extdisk` is a
  second NVMe, so it's exposed to the same renaming).

## Constraints

- **Never weaker.** No change may let a missing, wrong or empty filesystem be
  adopted automatically. Adopting one deletes the index entries of files the
  real disk still holds, and for a public root it can index a wrong disk into
  the public tier.
- **Unchanged guards.** The scan-time checks stay as they are:
  - the identity is bound to the open root descriptor;
  - before and after identities are compared;
  - submount changes are detected.
- **No wider access.** The worker and coordinator sandboxes are not
  widened. The coordinator still never opens a root (the
  `coordinator_never_opens_a_root_child` test). Any new key comes from
  something a process may already do.
- **Fails closed:**
  - a root already enrolled with a stable key whose key can't be read now
    stays offline, rather than falling back to the device name;
  - a filesystem that offers no stable key behaves as today;
  - an identity record that can't be read stays a mismatch, as it does now
    (`shares.rs:69-71`).
- **Rollback is safe.** Running an older binary after the upgrade may put
  local roots offline, but it must not adopt anything or delete anything.
- **Upgrade in place.** Existing databases upgrade without a manual step,
  and identities are kept for both tiers and across tier moves
  (`store.rs:338`).
- **Usual rules.** Agents don't commit or deploy, and p510 is out of scope.

## Open questions

1. **Evidence for the automatic migration.** The draft assumes:
   - a stored `/dev/…` identity that differs only in the source is migrated
     automatically if a sample of the root's indexed files still exists on
     the current filesystem with the same size and modification time;
   - the database already stores both (`expected_size`, `expected_mtime_ns`);
   - an empty or different disk fails that check and stays offline.

   Is that the evidence you want, or do you prefer to always require the
   explicit owner action (see 2) for an already-broken root like p620's?
2. **The shape of the owner action.** It must not add a control socket (the
   #15 decision), and the database writer is the service. The draft assumes
   a one-shot setting in the Nix config (for example an `adopt` value naming
   the current filesystem), which you remove after one use, so the action
   stays declarative and reviewable. The spec designs it.
