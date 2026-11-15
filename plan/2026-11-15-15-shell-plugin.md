---
status: draft
issue: 15
spec: spec/2026-11-15-15-shell-plugin.md
coders:
  1: codex   # small, privacy-critical Rust: normalisation, closed reason list, path-free mapping
  2: codex   # tricky pure logic: merge order, level table, unit mapping, with QtTest
  3: agy     # large but mechanical QML: manifest, singleton state, panel and view
  4: coder   # Nix packaging plus a self-contained lint script and fixture
  5: agy     # mechanical docs edits in three files
reviewers:
  - codex    # independent read of the diff against this plan
  - agy      # second read; checks the QML against the shell components and the docs wording
---

# Plan: an Omarchy shell plugin for llm-index status and settings

Self-contained summary of the approved spec (`spec/2026-11-15-15-shell-plugin.md`).

- **Where:** repo `/mnt/code/Source-home/GitHub/nixarchy-llm-index`, branch
  `feat/15-shell-plugin`.
- **Worktrees:** each step works in its own worktree (`../llm-index-15-sN`,
  branched from the branch tip). The lead reviews each one, then
  cherry-picks in order 1 to 5.
- **Parallel steps:** steps 1, 2 and 5 touch disjoint files and run in
  parallel.
- **Dependencies:** step 3 needs step 2's `Model.js` API. Step 4 needs
  `plugin/` from steps 2 and 3.
- **nixos_config:** step 6 is a separate task in nixos_config, with its own
  issue and short artifacts that point here, as for #2906.

## Approved decisions (carry these, do not re-decide)

- **Interface.** `llm-index status --json [--brief]`. `--brief` requires
  `--json`. Without `--json`, the text output is byte-identical to today's.
  - JSON mode prints exactly one object on stdout.
  - Errors keep exit code 2 and go to stderr.
  - The MCP `status` and `build(_, true)` are unchanged.
- **`--brief`** never prepares or runs the file and chunk count queries
  (`src/status.rs:65-68`, `:105-106`). `files` and `chunks` are `null` in
  every row.
- **Owner-view normalisation, JSON only:**
  - `state` `""` becomes `pending`;
  - a root missing from `roots` becomes `pending` with null reason,
    last_scan, queued, done and counts;
  - an empty reason becomes `null`.
- **Closed reason list.** A reason is kept if `public_reason(r)`
  (`status.rs:33`) passes, or if it is one of:

  ```text
  identity mismatch, watch degraded, watcher exited, watch setup stalled,
  stdin blocked, no progress, extract timeout, scan timeout, worker died,
  disk full, symlink in path
  ```

  Anything else becomes `other`.
- **Config errors** become `{"root", "what", "reason"}`:
  - an exact root-path match gives `what: "root"`;
  - otherwise the deepest root containing the path by whole components
    gives `what: "exclusion"` (`/data/a` contains `/data/a/x` but not
    `/data/ab`);
  - no match gives `root: null, what: "unknown"`;
  - the path is never output.
- **`settings`:**
  - `roots[]` with `{name, tier, kind, interval, share: bool}`;
  - `exclusions` (a count) and `lifted_builtin` (a count);
  - `limits` with all six fields.
  - There are no paths anywhere.
- **Top level:** `generated_at` (Unix seconds) and `brief` (bool).
- **Plugin.** The ID is `olafkfreund.llm-index`, with
  `kinds: ["bar-widget"]`. It has:
  - a singleton `IndexState` with `acquire()` and `release()`;
  - `Model.js` for all the logic;
  - `Panel.qml` with a `BarIconButton` that hides itself, a `KeyboardPanel`
    and an IPC target `olafkfreund.llm-index.bar` (`manageIpc: false`)
    offering `toggle`, `open`, `close` and `status()`, which returns
    `{"level"}` only.
- **Rates:**

  | Check | When | Deadline |
  | --- | --- | --- |
  | `systemctl --user show llm-index.service -p ActiveState -p SubState -p Result` | every 10 s | 5 s |
  | `--brief` | every 60 s, or every 5 s while the panel is open | 10 s |
  | full status | when the panel opens, and on Refresh | 60 s |

  - At most one process of each kind runs at once; a tick that finds one
    running is skipped.
  - A process past its deadline is killed and counts as a failure.
- **Merge:**
  - live fields (per-root state, reason, last_scan, queued and done, and
    `config_errors`) come from the response with the newest `generated_at`;
  - an older response leaves them unchanged;
  - counts and `settings` come only from full responses, with their own
    timestamp;
  - a brief response never nulls them.
- **Errors:**
  - stderr goes to a collector that is never read;
  - failures map to fixed categories: `not installed`, `status failed`,
    `bad output` and `timed out`;
  - the plugin has no `console.*` or `print`.
- **Unit state mapping:**

  | `ActiveState` | Shown as |
  | --- | --- |
  | `active` | `running` |
  | `activating`, `deactivating`, `reloading` | `starting` |
  | `failed` | `failed` |
  | `inactive` | `stopped` |
  | anything else, or the query failed | `unknown` |

- **Levels, in priority order:**

  | Level | Condition | Tooltip |
  | --- | --- | --- |
  | `down` | the unit is `stopped`, `failed` or `unknown` | "llm-index: service not running" |
  | `problem` | any root `offline`, `degraded` or `config_error`; any config error; the last brief failed, or the live data is older than 3 brief periods | "llm-index: needs attention" |
  | `indexing` | any root `pending`, any null `last_scan`, or `queued >= 1000` (constant `BACKLOG`) | "llm-index: indexing" |
  | `ok` | none of the above | "llm-index: ok" |

  The icon draws when the level isn't `ok` or the panel is open. Private
  roots count.
- **Actions:**
  - **Refresh:** a full status.
  - **Restart:** behind a `ConfirmDialog`. Runs
    `systemctl --user restart llm-index.service`, then refreshes.
  - **Open log:** runs
    `omarchy-launch-floating-terminal-with-presentation journalctl --user -fu llm-index`.
- **Settings tab:**
  - read-only, with no edit controls;
  - shows the line "Managed by Nix: `services.llm-index` in Home Manager.
    Change it in your system configuration and rebuild."
- **Text:** every `Text`, including `PanelSectionHeader`, sets
  `textFormat: Text.PlainText`. There are no links.
- **Binary:** `llm-index` from the session PATH, with the default config
  path.
- **nixos_config:**
  - `programs.nixarchy.plugins."olafkfreund.llm-index".src`;
  - `local.omarchy.enableOnce`;
  - the key `SUPER + CTRL + ALT + I`, with action
    `omarchy-shell shell toggle olafkfreund.llm-index '{}'`;
  - `programs.nixarchy.menu.extraEntries."apps.llm-index"` with the same
    action;
  - an input bump.
- **Order:** #14 (the unstable `/dev/sdX` identity) is fixed and deployed
  before step 6 deploys. Steps 1 to 5 don't depend on it.

## Steps

The Rust check, inside the dev shell (it sets the pdfium and model paths):

```bash
nix develop -c bash -c 'CARGO_TARGET_DIR=target-sN cargo test --all-targets && CARGO_TARGET_DIR=target-sN cargo clippy --all-targets -- -D warnings && cargo fmt --check'
```

Traps for every step:

- no new Rust or Nix dependency, apart from Qt tools inside checks;
- no test-only code paths in production;
- agents never stage, commit, push, deploy or restart, and never work on
  `main` or in `~/.config/nixos`.

1. **`status --json` and `--brief`** in `src/main.rs:54-55` and `:138-141`,
   plus `src/status.rs`.
   - **CLI:** `Status` gets `#[arg(long)] json: bool` and
     `#[arg(long, requires = "json")] brief: bool`.
     Pass both flags to `status::run`.
   - **Count queries:** add a `brief: bool` parameter to `build`, or an
     inner function that `build` calls. When it's set, skip preparing and
     running the two count queries, and set the counts to `null`.
     - Keep `build(store, public)`'s signature for `src/mcp.rs:677` by
       wrapping it.
   - **New `fn json(store, config, brief) -> Res<Value>`:**
     - calls the inner builder with `public = false`;
     - applies the normalisation and the closed reason filter (a `const`
       slice next to `STATES`);
     - maps `config_errors` (component-wise `Path::starts_with`; exact
       match first, then the deepest containing root);
     - adds `settings` from `Config` and `generated_at`/`brief`.
   - Confirm the reason list against every `set_state`, `mark_degraded` and
     `abandon` call site in `src/index/` and `src/worker/`. Report any
     literal missing from the list instead of adding it silently.
   - **Tests** (in `status.rs`'s test module, or a new one):
     - the JSON shape;
     - brief null counts;
     - **brief runs no count query:** on a fixture DB with
       `ALTER TABLE chunks RENAME TO chunks_x`, `--brief` succeeds and full
       mode errors;
     - the normalisation cases;
     - the reason filter (`other` for a path-bearing reason);
     - config-error mapping: exact match, nested `/data/pub` and
       `/data/pub/moving`, `/data/a` against `/data/ab`, and a stale row;
     - no input path in the output (assert that no `/` appears in any
       reason or config-error value, or in `settings`);
     - `lifted_builtin` as a count;
     - the text output unchanged (a golden string for a fixture).

   → verify with the Rust check, then
   `cargo run -- status --json --brief | jq .` against a fixture config.
   Traps:
   - the `visible()` SQL function needs the config set before `begin()`,
     as `run` does today;
   - `Mode::Both` opens read-only (`db.rs:63-71`), so never switch to
     `Writer`.

2. **`plugin/Model.js` and `plugin/tests/tst_model.qml`** (new).
   - `Model.js` is `.pragma library`. Its pure functions:
     - `validate(obj)` returns a snapshot or null (checks the shape);
     - `merge(prev, snap)` applies the merge rule;
     - `unitState(showOutput)` parses `KEY=VALUE` lines;
     - `level(state)` and `tooltip(level)`;
     - `fmtCount`, `fmtBytes` and `fmtAge`.

     Export the constants `BACKLOG = 1000` and `STALE_PERIODS = 3`.
   - `tst_model.qml` is a QtTest `TestCase` that imports `../Model.js`. It
     covers:
     - the level table, row by row, with priority;
     - a merge with full then brief, and brief then a late older full
       (reversed order);
     - an older brief ignored;
     - unit mapping for each `ActiveState`, plus empty and garbage input;
     - an initial failure, a failure after success, and recovery;
     - stale detection;
     - `validate` rejecting the wrong shape.

   → verify with:

   ```bash
   nix shell nixpkgs#qt6.qtdeclarative -c sh -c 'QT_QPA_PLATFORM=offscreen qmltestrunner -input plugin/tests/tst_model.qml'
   ```

   Traps:
   - no Quickshell imports in `Model.js`, so it stays testable;
   - no `console.*`.

3. **`plugin/manifest.json`, `qmldir`, `IndexState.qml`, `Panel.qml` and
   `IndexView.qml`** (new). Model them on
   `/mnt/code/Source-home/GitHub/nixarchy/pkgs/rebuild-panel/` (`Panel.qml`,
   `RebuildState.qml`, `manifest.json`).
   - **`manifest.json`** follows the rebuild manifest, with:
     - `id` `olafkfreund.llm-index`, `name` `llm-index`;
     - `kinds: ["bar-widget"]`, `entryPoints.barWidget: "Panel.qml"`;
     - `barWidget` with `displayName` "llm-index", `category` "System",
       `allowMultiple: false`, `defaultSection: "right"`.
   - **`qmldir`** declares `singleton IndexState 1.0 IndexState.qml`.
   - **`IndexState.qml`:**
     - `pragma Singleton`, with `acquire()` and `release()` viewer counting
       as in `RebuildState.qml`, and `panelOpen` driving the brief rate;
     - three `Process` objects (unit, brief, full), each with a
       `StdioCollector` on stdout and one on stderr that is never read;
     - deadline `Timer`s that set `running = false` and record `timed out`;
     - results go through `Model.merge`;
     - `startRestart()` and `openLog()` use `Process` with fixed argv.
   - **`Panel.qml`:**
     - `BarIconButton` with `implicitWidth` 0 when the level is `ok` and
       the panel is closed (`rebuild-panel/Panel.qml:50-51`, `:83-84`);
     - `tooltipText` from `Model.tooltip`;
     - `KeyboardPanel` hosting `IndexView`;
     - `IpcHandler` with target `olafkfreund.llm-index.bar`.
   - **`IndexView.qml`:**
     - **Status tab:** a header with the unit state, sub-state and result,
       then a row per root (tier badge, state, reason, last scan age,
       queued/done, files and chunks with "counts as of …"), config
       errors, DB sizes, and the sandbox line labelled "Landlock
       available";
     - **Settings tab:** the projection and the managed-by-Nix line;
     - **Action row:** Refresh, Restart and Open log;
     - "Status unavailable" with the error category when there's no
       snapshot;
     - `ConfirmDialog`: while it's open, `PanelKeyCatcher.blocked` is true
       and a `Keys.onPressed` on its focus item calls `handleKey(event)`;
       the view keeps a minimum height so it fits.

   Every `Text` and `PanelSectionHeader` sets
   `textFormat: Text.PlainText`.

   → verify with:

   ```bash
   nix shell nixpkgs#qt6.qtdeclarative -c qmllint --bare plugin/*.qml
   ```

   It checks syntax only. A live load on p620 happens in the lead's Tests.
   Traps:
   - use only components that rebuild-panel or `Ui/qmldir` list;
   - no `sh -c`;
   - no `Qt.openUrlExternally`;
   - no console output;
   - don't write `shell.json`.

4. **Packaging and `checks.plugin`:** in `flake.nix:37-42` and `:60-80`,
   new `nix/plugin.nix`, new `plugin/tests/lint.sh` and
   `plugin/tests/broken/Bad.qml`.
   - **`packages.${system}.plugin`:** `lib.fileset.toSource` of `plugin/`,
     minus `plugin/tests`, so `manifest.json` sits at the output root.
   - **`checks.${system}.plugin`:** a `runCommand` with
     `qt6.qtdeclarative` that runs:
     - `qmllint --bare` syntax checks, as in nixarchy `tests/qml.nix:14-21`;
     - `lint.sh` on `plugin/*.qml`;
     - `lint.sh` on `plugin/tests/broken/`, which **must fail**;
     - `qmltestrunner` offscreen on `tst_model.qml`.
   - **`lint.sh`** scans each QML object block of type `Text`,
     `PanelSectionHeader`, `Label`, `TextEdit` or `TextInput`.
     - It fails if a block lacks `textFormat: Text.PlainText` in that same
       block.
     - It also fails on `console.`, `print(`, `sh -c` and
       `Qt.openUrlExternally`.
     - `Bad.qml` holds one of each violation, including a `Text` without
       `textFormat` in a file where another `Text` has it.

   → verify with:

   ```bash
   nix build .#plugin && nix build .#checks.x86_64-linux.plugin && ls result/
   ```

   Traps:
   - the Rust package's fileset (`nix/package.nix:36-38`) must not pick up
     `plugin/`;
   - don't rename the existing checks.

5. **Docs:** `docs/reference.md` (`status` section near `:30`), `README.md`
   and `docs/security.md`.
   - In `reference.md`, document:
     - `status --json` and `--brief`, with the field list and their
       meaning;
     - the closed reason list;
     - the config-error mapping;
     - that `--brief` counts are null.
   - Add a short "Desktop plugin" section to the README. It covers what the
     plugin shows, the key, and that settings stay in Nix.
   - In `security.md`, record:
     - private metadata appears only in the open panel;
     - the tooltip and IPC carry a level only;
     - the plugin discards stderr;
     - Open log is the owner's explicit exception;
     - `status --json` is path-free.

   → verify with `markdownlint-cli2` on the three files. Trap: the
   repository's wrapping (MD013).

6. **nixos_config deploy** (lead, separate task). It starts after this
   repository's PR merges and #14 is deployed.
   - Open an issue in nixos_config with short intent, spec and plan that
     point to this spec.
   - Edit `hosts/common/nixos/llm-index.nix` in a worktree:
     - import `./omarchy-enable-once.nix`;
     - in the user's HM block, set the plugin `src` to
       `inputs.nixarchy-llm-index.packages.${pkgs.stdenv.hostPlatform.system}.plugin`
       and add `local.omarchy.enableOnce`;
     - add `local.omarchy.keys.extra` and
       `programs.nixarchy.menu.extraEntries."apps.llm-index"`, at the
       levels where `omarchy-keys.nix` and `omarchy-gmessages.nix` set
       them;
     - bump the input.
   - Run `just validate` and build p620 and razer.
   - Deploy with `just p620`, then `just deploy-via-p620 razer`, announcing
     on the agent bus first.

   Traps:
   - work in a worktree, deploy from main;
   - p510 is never touched;
   - check the chord again against host files at that time.

## Tests (lead)

- After each cherry-pick, run the Rust check. After step 4, run
  `nix flake check`, covering the existing VM, fixture, closure and module
  checks, plus `checks.plugin`.
- **Text output.** On p620, comparing today's binary with the branch build
  prints nothing:

  ```bash
  diff <(llm-index status) <(./result/bin/llm-index status)
  ```

- **Timing.** On p620 (and razer after step 6), using `/usr/bin/time`:
  - `status --json --brief` under 50 ms;
  - `status --json` close to today's 3 s;
  - with `jq`, no `/` in any reason or config-error field, and none in
    `settings`.
- **Live load before the PR merges,** on p620 only:
  1. Link `result-plugin` into `~/.config/omarchy/plugins/` as
     `olafkfreund.llm-index`.
  2. Rescan and enable it with `omarchy plugin enable`, and check the
     shell journal for load errors.
  3. Remove the link and disable it again afterwards.

  This is the only check that catches a wrong `qs.Ui` property.
- **After step 6, on p620 and then razer:**
  - after a re-login the plugin is enabled;
  - the key and the menu row open it;
  - the tabs match `llm-index status`;
  - `systemctl --user stop llm-index` gives level `down` within 10 s, and
    Restart from the panel (keyboard confirm) clears it;
  - with the config renamed, the level reaches `problem` within 3 minutes
    (rename it back afterwards);
  - `omarchy-shell olafkfreund.llm-index.bar status` and the tooltip show a
    level only;
  - the shell journal has nothing from the plugin;
  - a disable survives a re-login;
  - on p620's two monitors, `pgrep -c -f 'status --json'` is at most 1.
- **Reviews:** Codex and agy, read-only, on `git diff origin/main` against
  this plan before the PR.

## Rollback

- **This repository:** revert the merge commit. The new flags and the plugin
  package are additive, and the text `status` and the MCP are unchanged.
- **nixos_config:** revert the step 6 commit and redeploy. The plugin is
  uninstalled, and its `enableOnce` marker is harmless. Index databases are
  never touched.
