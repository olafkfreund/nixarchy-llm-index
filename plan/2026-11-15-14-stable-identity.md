---
status: draft
issue: 14
spec: spec/2026-11-15-14-stable-identity.md
coders:
  1: codex   # small, exact syscall plumbing: ioctl number, struct decoding, length bounds
  2: coder   # mechanical plumbing across storage, Sink, config and Nix options
  3: codex   # the core logic: verdict, gate, precedence table, evidence; safety-critical
  4: coder   # one VM stage in an existing NixOS test
  5: agy     # mechanical docs edits in three files
reviewers:
  - codex    # fresh instance: independent read of the diff against this plan
  - agy      # fresh instance: second read, and the docs wording
---

# Plan: a root keeps its identity when the kernel renames its disk

Self-contained summary of the approved spec
(`spec/2026-11-15-14-stable-identity.md`).

Repo `/mnt/code/Source-home/GitHub/nixarchy-llm-index`, branch
`fix/14-stable-identity-v2`, worktree
`/mnt/code/Source-home/GitHub/llm-index-14`.

## How the steps are run

- **Who commits.** Agents never stage or commit. The lead reviews each
  step's worktree diff, commits it on the branch, and later steps start from
  the updated tip.
- **Worktrees.** Each step works in `../llm-index-14-sN`, made from the
  branch tip at the time the step starts.
- **Order:** steps 1 and 5 run in parallel, then 2, then 3, then 4. Step 2
  needs step 1's `MountIdentity` field, step 3 needs both, and step 4 needs
  the behaviour.

## Approved decisions (carry these, do not re-decide)

### Interfaces shared between steps

- **`src/proto.rs`:** `MountIdentity` gains
  `#[serde(default)] pub fs_uuid: Option<String>`. `Share` is unchanged.
- **`src/worker/scan.rs`:**
  - `pub fn fs_uuid(fd: &impl AsFd) -> Option<String>` runs
    `ioctl(fd, 0x80111500)` with a 17-byte buffer.
  - `fn decode_fsuuid(buf: &[u8; 17]) -> Option<String>` returns `None`
    unless `1 <= buf[0] <= 16`, and otherwise the first `buf[0]` bytes after
    the length as lowercase hex.
  - The `before` and `after` identities in `scan()` both carry it.
- **`src/index/mod.rs`:** a new
  `pub enum StoredUuid { Absent, Known(String), Unreadable }`. The `Sink`
  trait changes as follows:
  - `fn expected_fs_uuid(&self, root: &str) -> StoredUuid;` (new)
  - `fn set_expected_identity(&mut self, root: &str, share: &Share, fs_uuid: Option<&str>);`
    (gains `fs_uuid`; writes both columns in one statement)
  - `fn indexed_meta(&self, root: &str) -> Result<Vec<(String, u64, i64)>, String>;`
    (new). It returns distinct `relpath` with `size` and `mtime_ns` from
    both tiers. A path in both tiers appears once, with the public tier's
    row taken first.
- **`RootSpec`** (`src/index/sched.rs:30-36`) gains
  `pub adopt_uuid: Option<String>`, filled from config.
- **`src/config.rs`:** `Root` gains `adopt_uuid: Option<String>`.
  - Validation strips dashes and requires 2 to 32 hex digits.
  - It's stored lowercase without dashes.
- **`nix/config.nix`:** a root option `adoptUuid` (nullable string),
  rendered as `adopt_uuid`.

### Storage

- **Column:** `roots.fs_uuid TEXT`, added in `Writer` mode when missing:
  check `PRAGMA table_info(roots)`, then
  `ALTER TABLE roots ADD COLUMN fs_uuid TEXT`.
- **Same rules as `share_id`:**
  - it moves with the root across tiers (`index/store.rs:338-380`);
  - it is cleared on a path change (`:374`);
  - it is written together with `share_id` (`:677-690`).
- **Read as three states:**
  - NULL gives `Absent`;
  - 2 to 32 lowercase hex characters give `Known`;
  - an SQL error or anything else gives `Unreadable`.

### Worker

- The ioctl is `FS_IOC_GETFSUUID` on the root fd the worker already holds.
- No sandbox, Landlock or `SystemCallFilter` change.

### Verdict

- **`Verdict::Confirm` is new** in `shares.rs` `verdict()`, checked before
  the final `Mismatch`. It applies when:
  - the expected and current `Share` are equal except `source`;
  - both sources start with `/dev/`;
  - `!is_network(fstype)`;
  - and `configured_ok` holds.
- `adopt_uuid` never widens this case.

### Scheduler

- **`may_extract`** stays `Ok | Adopt`.
- **`solo_held`** treats `Confirm` like `Mismatch`. The solo head-of-line
  loop (`sched.rs:920-936`) evicts `Confirm` heads too.
- **Scans run under `Confirm`.** Wherever a scan is allowed under `Probe`,
  it's also allowed under `Confirm`.
- **Rejection gate:**
  - `RootRt` gains `rejected: Option<Derived>` (the share plus mount ID
    judged at rejection);
  - `verdict_of` returns `Mismatch` while `derived == rejected`;
  - `rederive` clears it when the derived identity changes;
  - a rejection goes through the `check_identity` `Mismatch` path: the
    queue is dropped, the hold set, the state set, and the root's watch
    worker stopped.
- **`set_state`** writes when the state **or** the reason changes.
  `RootRt` gains `reason: String`.
- **Root state under `Confirm`:** `offline`, with reason `identity check`.
  `identity check` joins `REASONS` in `src/status.rs`, and the golden files
  in `plugin/tests/fixtures` are regenerated.

### Precedence table at `scan_done`

S is the stored UUID, W the worker's UUID (before and after, equal), A the
root's `adopt_uuid`, and E the evidence. Scans already discarded (before and
after differ, or `Mismatch`) and `Probe` keep their behaviour. Otherwise the
first matching row decides:

| # | Condition | Result |
| --- | --- | --- |
| 1 | S `Unreadable` | reject |
| 2 | A set | accept iff `W == Some(A)` and the scan is complete; store S = A. Else reject |
| 3 | verdict `Adopt` | accept; store S = W |
| 4 | S `Known(U)` | accept iff `W == Some(U)`; else reject |
| 5 | S `Absent`, verdict `Ok` | accept; store S = W only if W is `Some` and E holds |
| 6 | S `Absent`, verdict `Confirm` | accept iff W is `Some`, the scan is complete and E holds; store S = W. Else reject |

- **Accept:** apply the scan as today and store the current `Share` with S.
- **Reject:** discard the scan, go `offline` with `identity mismatch`, and
  set the gate.

### Evidence E

- **Denominator:** `indexed_meta(root)` minus paths under the scan's
  protected skipped subtrees, using `apply_scan`'s `protects()` filter
  (`store.rs:597-602`).
- **Matches:** paths present among the scan's file entries (`!is_dir`) with
  the same `size` and `mtime_ns`.
- **Holds iff** `denominator >= 1 && matches * 10 >= denominator * 9`.
- **Fails closed:** an `Err` from `indexed_meta` means E is false.

### Hosts

- p620 and razer only; p510 is untouched.

## Steps

The Rust check, inside the dev shell:

```bash
nix develop -c bash -c 'CARGO_TARGET_DIR=target-sN cargo test --all-targets && CARGO_TARGET_DIR=target-sN cargo clippy --all-targets -- -D warnings && cargo fmt --check'
```

Traps for every step:

- no new dependency. `rustix` already provides `ioctl`: use `rustix::ioctl`
  with a `Getter` opcode. Enabling a `rustix` feature is allowed;
- no test-only code in production;
- agents never stage, commit, push, deploy or restart, and never work on
  `main` or in `~/.config/nixos`.

1. **Worker UUID,** in `src/proto.rs` and `src/worker/scan.rs`.
   - Add the `fs_uuid` field, `fs_uuid()` and `decode_fsuuid()` as in the
     Interfaces section.
   - Fill `fs_uuid` in both identities in `scan()`, from the root fd
     (`scan.rs:543-575`, `:613-635`).
   - Update every `MountIdentity { .. }` literal in the crate (tests
     included) with `fs_uuid: None`.
   - **Tests:**
     - `decode_fsuuid` for len 0, 1, 16 and 17, plus a known 16-byte value;
     - `fs_uuid()` on a tempdir returns `Some` or `None` without panicking.

   → verify with the Rust check. Trap: no behaviour change elsewhere. The
   scheduler ignores the field until step 3.

2. **Storage, Sink, config and Nix,** in `src/db.rs`, `src/index/store.rs`,
   `src/index/mod.rs`, `src/index/sched.rs` (only `RootSpec`, the
   `scan_done` call site, and the `Mem` test sink at `:1143`),
   `src/config.rs`, `nix/config.nix` and `src/index/` where `RootSpec` is
   built.
   - Add the column migration and `StoredUuid`.
   - Make the `Sink` trait changes and implement them in `DbSink` and `Mem`.
   - `scan_done` keeps today's behaviour. It passes the worker's
     `before.fs_uuid` to `set_expected_identity` where it sets the identity
     today (`sched.rs:671-675`).
   - Add `adopt_uuid` in config and Nix, and wire it into `RootSpec`.
   - **Tests:**
     - migration on a database created without the column;
     - `fs_uuid` across a tier move and a path change;
     - an `Unreadable` value;
     - `indexed_meta` with the same path in both tiers;
     - `adopt_uuid` validation (canonical, hex, bad);
     - `nix/checks/modules.nix` still evaluates.

   → verify with the Rust check and `nix flake check --no-build`. Trap: the
   readers' `Mode::Both` and `PublicOnly` opens never write.

3. **Behaviour,** in `src/index/shares.rs`, `src/index/sched.rs`,
   `src/status.rs` and `plugin/tests/fixtures/*.json` (regenerated).
   - Implement `Confirm`, the gate, `set_state` reasons, solo eviction, the
     precedence table and E exactly as in the Approved decisions.
   - Add `identity check` to `REASONS`. Regenerate the golden files with
     `UPDATE_STATUS_GOLDEN=1`.
   - **Tests:**
     - every table row and its negative;
     - `Confirm` only for a `/dev` source-only local change with
       `configured_ok`;
     - `adopt_uuid` never giving `Confirm` across a mount point or mount
       root change;
     - extract, `send_next`, solo and watch off under `Confirm` and under
       the gate;
     - the gate cleared by `rederive`;
     - solo eviction of a `Confirm` head;
     - a reason-only `set_state` write;
     - row 5 enrolment only with E;
     - evidence: empty listing, 89% and 90%, one file, a protected subtree,
       both tiers, an incomplete scan, and an `Err`.

   → verify with the Rust check, then the plugin QML test (the golden files
   changed):

   ```bash
   nix build .#checks.x86_64-linux.plugin --no-link
   ```

   Traps:
   - keep `check_identity`'s existing `Mismatch` effects;
   - the network-mounts behaviour must not change.

4. **VM stage,** in `nix/checks/vm.nix`. Add a stage that:
   1. makes two ext4 images, A with `mkfs.ext4 -U <uuid-A>` and B with
      another UUID;
   2. attaches A as a virtio disk, mounts it at the root path, writes a few
      files, starts the service, and waits until the root is `ok`;
   3. asserts `roots.fs_uuid` equals uuid-A without dashes;
   4. remounts A from a different device node: attach it as the second
      disk, or move it with `losetup`, so the source differs. Then waits for
      `ok` with unchanged file counts;
   5. mounts empty B at the same point and asserts
      `offline / identity mismatch` with the file count unchanged.

   Reuse the existing stage helpers.

   → verify with:

   ```bash
   nix build .#checks.x86_64-linux.<vm check name> -L
   ```

   Trap: keep the existing stages and their timeouts.

5. **Docs,** in `docs/security.md`, `docs/reference.md` and
   `docs/troubleshooting.md`, as in spec section 7.
   - `security.md`: the identity guard, `Confirm`, the precedence table, the
     evidence, `adopt_uuid` (it may accept an empty disk), and rollback.
   - `reference.md`: the `adopt_uuid` setting and the `identity check`
     reason.
   - `troubleshooting.md`: "a root is offline with `identity mismatch`",
     including `findmnt -no UUID <mountpoint>`.

   → verify with `markdownlint-cli2` on the three files.

## Tests (lead)

- After each commit, run the Rust check. After step 4, run
  `nix flake check`.
- Reviews by fresh Codex and agy instances, read-only, on
  `git diff origin/main...HEAD` against this plan, before the PR.
- **Live on p620 after deploy** (`just p620` from nixos_config main, with
  the input bumped):
  - `llm-index status` shows `source` `offline / identity check`, then
    `ok` after one scan;
  - file and chunk counts are unchanged, apart from real edits;
  - `share_id` and `fs_uuid` in `roots` (public.db, read-only `sqlite3`)
    show `/dev/sdd1` and `bdd8461e7d6c42b69cfe39967825dd42`.
- **Live on razer:** both roots stay `ok` and enrol a UUID.
- If p620's row 6 fails (E below 90%), set `adoptUuid` for `source` to
  `bdd8461e-7d6c-42b6-9cfe-39967825dd42` in nixos_config, redeploy, and
  remove it afterwards.

## Rollback

- **This repository:** revert the PR (`git revert -m 1 <merge>`).
- **Hosts:** revert the nixos_config input bump and redeploy.
  - The older binary ignores the `fs_uuid` column and guards as before.
  - `share_id` already holds the current device name, so roots stay `ok`.
- The column stays in the database, and it's harmless.
