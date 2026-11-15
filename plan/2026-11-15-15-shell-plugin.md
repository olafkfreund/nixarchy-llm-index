---
status: approved
issue: 15
spec: spec/2026-11-15-15-shell-plugin.md
coders:
  1: codex   # small, privacy-critical Rust: normalisation, closed reason list, path-free mapping
  2: codex   # tricky pure logic: merge order, level table, unit mapping, with QtTest
  3: agy     # large but mechanical QML: manifest, singleton state, panel and view
  4: coder   # Nix packaging plus a self-contained lint script and fixtures
  5: agy     # mechanical docs edits in three files
reviewers:
  - codex    # fresh instance: independent read of the diff against this plan
  - agy      # fresh instance: QML against the shell components, and the docs wording
---

# Plan: an Omarchy shell plugin for llm-index status and settings

Self-contained summary of the approved spec (`spec/2026-11-15-15-shell-plugin.md`).

Repo `/mnt/code/Source-home/GitHub/nixarchy-llm-index`, branch
`feat/15-shell-plugin`. Step 6 is a separate task in nixos_config, with its
own issue and short artifacts that point here, as for #2906.

## How the steps are run

- **Who commits.** Agents never stage or commit. After the lead reviews a
  step's worktree diff, the lead commits it on `feat/15-shell-plugin` as
  `feat(...): step N … (#15)`.
- **Worktrees.** Each step runs in its own worktree,
  `../llm-index-15-sN`, made from the branch tip **at the time the step
  starts**.
- **Order:**
  - steps 1, 2 and 5 start together from the plan-approval commit. The
    lead commits them in that order;
  - step 3 starts after step 2's commit, because it needs `Model.js`;
  - step 4 starts after step 3's commit, because it needs the whole
    `plugin/`.

## Approved decisions (carry these, do not re-decide)

### The JSON contract (shared by steps 1, 2, 3 and 5)

`llm-index status --json [--brief]` prints exactly one object on stdout:

```json
{
  "generated_at": 1794709381,
  "brief": false,
  "roots": [
    {"name": "source", "tier": "public", "state": "ok", "reason": null,
     "last_scan": 1794709381, "queued": 0, "done": 53697,
     "skipped_symlinks": 238, "files": 41920, "chunks": 236442}
  ],
  "config_errors": [
    {"root": "docs", "what": "exclusion", "reason": "symlink in path"}
  ],
  "settings": {
    "roots": [{"name": "source", "tier": "public", "kind": "auto",
               "interval": "7d", "share": false}],
    "exclusions": 1,
    "lifted_builtin": 0,
    "limits": {"max_file_mb": 0, "max_text_bytes": 0, "max_cells": 0,
               "memory_max_mb": 0, "workers": 0, "backoff_max": "1h"}
  },
  "sandbox": "landlock available",
  "databases": [{"name": "public.db", "bytes": 1}, {"name": "private.db", "bytes": 1}]
}
```

- **Field meanings:**
  - `generated_at` is in Unix seconds, and so is `last_scan`.
  - `state` is one of `ok`, `offline`, `degraded`, `config_error` or
    `pending`.
  - `reason`, `last_scan`, `queued`, `done` and `skipped_symlinks` can be
    `null`.
- **Brief vs full.** `settings`, `sandbox` and `databases` are present in
  both modes, since they're cheap. With `"brief": true`, `files` and
  `chunks` are `null` in every row.
- **Never in the output:** a filesystem path anywhere, a share source
  string, or a lifted glob.
- **Text output.** Without `--json`, the output is byte-identical to
  today's.
- **Errors** keep exit code 2 and go to stderr.
- **Unchanged:** the MCP `status` tool (`src/mcp.rs:677`) and
  `build(_, true)`.

### Rust behaviour

- **`--brief` runs no count query.** It never prepares or runs the file and
  chunk count queries (`src/status.rs:65-68`, `:105-106`).
- **Normalisation (JSON only):**
  - `state` `""` becomes `pending`;
  - a root missing from `roots` becomes `pending` with null fields;
  - an empty reason becomes `null`.
- **Closed reason list.** This applies to **both** root reasons and
  config-error reasons. A reason is kept if `public_reason(r)`
  (`status.rs:33`) passes, or if it is one of:

  ```text
  identity mismatch, watch degraded, watcher exited, watch setup stalled,
  stdin blocked, no progress, extract timeout, scan timeout, worker died,
  disk full, symlink in path, root path contains a symlink,
  root path is not a directory, landlock unavailable
  ```

  Anything else becomes `other`. (The last three came from
  `src/worker/sandbox.rs` during the branch review; they were missing from
  the first list.)
- **Config-error mapping:**
  - an exact root-path match gives `what: "root"`;
  - otherwise the deepest configured root containing the path by whole
    components gives `what: "exclusion"` (`/data/a` contains `/data/a/x`,
    not `/data/ab`);
  - no match gives `root: null, what: "unknown"`.
- **Settings counts.** `settings.exclusions` is `exclude.paths.len()` and
  `lifted_builtin` is `exclude.lift_builtin.len()` (`config.rs:54-58`).
  `share` is `root.share.is_some()`.

### The Model contract (shared by steps 2 and 3)

`Model.js` is `.pragma library` with no Quickshell imports. `now` is always
passed in, in Unix seconds.

- **State object:**

  ```text
  {
    live, liveAt,         // last good snapshot's roots + config_errors, and its generated_at
    full, fullAt,         // counts + settings + sandbox + databases from the last full run
    unit,                 // "running"|"starting"|"failed"|"stopped"|"unknown", plus sub, result
    briefError,           // null or a category
    fullError,            // null or a category
    busy                  // a full run is in progress
  }
  ```

- **Functions:**

  | Function | What it does |
  | --- | --- |
  | `empty()` | The initial state. |
  | `validate(obj)` | The object when it matches the contract above, otherwise `null`. |
  | `applySnapshot(st, snap)` | Merge rule: replaces `live` only if `snap.generated_at >= liveAt`. Replaces `full` only if `!snap.brief` and `snap.generated_at >= fullAt`. A brief snapshot never touches `full`. Clears the matching error. |
  | `applyError(st, kind, category)` | `kind` is `brief` or `full`. `category` is one of `not installed`, `status failed`, `bad output` or `timed out`. |
  | `unitState(text)` | Parses `ActiveState=`, `SubState=` and `Result=` lines from `systemctl show`. Maps `active` to `running`; `activating`, `deactivating` and `reloading` to `starting`; `failed` to `failed`; `inactive` to `stopped`; anything else to `unknown`. |
  | `level(st, now)` | Returns `down`, `problem`, `indexing` or `ok`, per the table below. |
  | `tooltip(level)` | The fixed strings in the table below. |
  | `fmtCount`, `fmtBytes`, `fmtAge(sec)` | Formatting. |

- **Constants:** `BACKLOG = 1000`, `BRIEF_PERIOD_S = 60`, `STALE_S = 180`.
  - Stale means 3 background brief periods, whether or not the panel is
    open. A healthy snapshot isn't "stale" the moment the panel opens.

### Level table

| Level | Condition (first match wins) | Tooltip |
| --- | --- | --- |
| `down` | `unit` is `stopped`, `failed` or `unknown` | "llm-index: service not running" |
| `problem` | any live root `offline`, `degraded` or `config_error`; any config error; `briefError` set; no `live`, or `now - liveAt > STALE_S` | "llm-index: needs attention" |
| `indexing` | any live root `pending`, any null `last_scan`, or any `queued >= BACKLOG` | "llm-index: indexing" |
| `ok` | otherwise | "llm-index: ok" |

Private roots count. The icon draws when the level isn't `ok` or a panel is
open.

### Shell behaviour

- **The plugin:** ID `olafkfreund.llm-index`, `kinds: ["bar-widget"]`.
  - `qmldir` lists `singleton IndexState 1.0 IndexState.qml` and
    `Model 1.0 Model.js`, as the spec says.
- **`IndexState`** is a singleton.
  - It counts viewers with `acquire()` and `release()`, like
    `RebuildState.qml`.
  - It counts open panels across monitors with `panelOpened()` and
    `panelClosed()`, giving `openPanels`. It doesn't use a boolean.
- **Processes**, at most one of each kind at once; a tick that finds one
  running is skipped:

  | Process | Command | When | Deadline |
  | --- | --- | --- | --- |
  | unit | `systemctl --user show llm-index.service -p ActiveState -p SubState -p Result` | every 10 s | 5 s |
  | brief | `llm-index status --json --brief` | every 60 s; every 5 s while `openPanels > 0` | 10 s |
  | full | `llm-index status --json` | when a panel opens (with an immediate brief and unit run), on Refresh, after Restart | 60 s |

  - When a deadline hits, the process sets a per-process `timedOut` flag,
    then sets `running = false`. `onExited` checks the flag first, so
    `timed out` isn't overwritten by `status failed`.
  - A process that fails to start gives `not installed`, a non-zero exit
    gives `status failed`, and invalid JSON gives `bad output`.
  - Each process has a stderr `StdioCollector` that is never read.
  - The plugin has no `console.*` or `print`.
- **`busy`** is true while the full process runs. The view shows a busy
  marker.
- **The bar button** is a `BarIconButton` with:
  - `text` set to the Nerd Font glyph U+F1C0 (database). An empty `text`
    with no `iconComponent` renders at opacity 0;
  - `useActiveColor: true`;
  - `active` true when the level is `down`, `problem` or `indexing`;
  - `activeColor` set to `Color.urgent` for `down` and `problem`, and
    `Color.accent` for `indexing`;
  - `implicitWidth` 0 when the level is `ok` and no panel is open
    (`rebuild-panel/Panel.qml:50-51`, `:83-88`);
  - `tooltipText` from `Model.tooltip`.
- **IPC:** the target is `olafkfreund.llm-index.bar`, with
  `manageIpc: false`. It offers `toggle`, `open`, `close` and `status()`,
  which returns `{"level": "<level>"}` only.
- **The view:**
  - **Status tab:** a unit header (state, sub-state, result), then a row
    per root: tier badge, state, reason, last-scan age, queued/done, and
    files and chunks with "counts as of …". After the rows come the
    config errors, DB sizes, and the sandbox line labelled "Landlock
    available".
  - **Settings tab:** the projection, and the line "Managed by Nix:
    `services.llm-index` in Home Manager. Change it in your system
    configuration and rebuild." It has no edit controls.
  - With no `live` and no `full`, it shows "Status unavailable" plus the
    error category.
- **Actions:**
  - **Refresh:** a full run.
  - **Restart:** a `ConfirmDialog`, with focus handled as follows:
    - when `opened` becomes true, the dialog's focus item calls
      `forceActiveFocus()`;
    - `PanelKeyCatcher.blocked` is true while the dialog is open;
    - the focus item's `Keys.onPressed` calls `handleKey(event)`;
    - `confirmed` runs `systemctl --user restart llm-index.service`, and
      its exit triggers unit and full runs;
    - the view keeps a minimum height so the dialog fits.
  - **Open log:** runs this argv:

    ```text
    omarchy-launch-floating-terminal-with-presentation journalctl --user -fu llm-index
    ```

- **Text:** every `Text`, including `PanelSectionHeader` (a `Text`,
  `Ui/PanelSectionHeader.qml:8`), sets `textFormat: Text.PlainText`. There
  are no links and no `Qt.openUrlExternally`. Every `Process` gets an
  explicit argv: no `sh` or `bash` with `-c`.
- **The binary:** `llm-index` from the session PATH, with the default
  config path.

### nixos_config and order

- **nixos_config:**
  - `programs.nixarchy.plugins."olafkfreund.llm-index".src`;
  - `local.omarchy.enableOnce`;
  - the key `SUPER + CTRL + ALT + I`, with action
    `omarchy-shell shell toggle olafkfreund.llm-index '{}'`;
  - `programs.nixarchy.menu.extraEntries."apps.llm-index"`, with the same
    action;
  - an input bump.
- **Order:** #14 (the unstable `/dev/sdX` identity) is fixed and deployed
  before step 6 deploys. Steps 1 to 5 don't depend on it.

## Steps

The Rust check, inside the dev shell (it sets the pdfium and model paths):

```bash
nix develop -c bash -c 'CARGO_TARGET_DIR=target-sN cargo test --all-targets && CARGO_TARGET_DIR=target-sN cargo clippy --all-targets -- -D warnings && cargo fmt --check'
```

The QML test, in a shell with Qt. `qmltestrunner` is unwrapped, so it
needs the import path and a writable `HOME`:

```bash
nix shell nixpkgs#qt6.qtdeclarative -c sh -c 'q=$(dirname $(dirname $(readlink -f $(command -v qmltestrunner)))); HOME=$(mktemp -d) QML_XHR_ALLOW_FILE_READ=1 QT_QPA_PLATFORM=offscreen qmltestrunner -import "$q/lib/qt-6/qml" -input plugin/tests/tst_model.qml'
```

Traps for every step:

- no new Rust or Nix dependency, apart from Qt tools inside checks;
- no test-only code paths in production;
- agents never stage, commit, push, deploy or restart, and never work on
  `main` or in `~/.config/nixos`.

1. **`status --json` and `--brief`** in `src/main.rs:54-55` and `:138-141`,
   plus `src/status.rs`.
   - **CLI:** `Status` gets `#[arg(long)] json: bool` and
     `#[arg(long, requires = "json")] brief: bool`. Pass both to
     `status::run`.
   - **Count queries:** move the body of `build` into an inner function
     with a `brief: bool` parameter. When it's set, skip preparing and
     running the two count queries and set both counts to `null`.
     - `build(store, public)` keeps its signature (used at
       `src/mcp.rs:677`) and calls the inner function with
       `brief = false`.
   - **New `fn json(store, config, brief) -> Res<Value>`**, producing the
     contract above:
     - calls the inner function with `public = false`;
     - normalises the rows;
     - applies the closed list (a `const` slice next to `STATES`) to root
       **and** config-error reasons;
     - maps config errors with component-wise `Path::starts_with`;
     - adds `settings`, `generated_at` and `brief`.
   - Confirm the reason list against every `set_state`, `mark_degraded` and
     `abandon` call site in `src/index/` and `src/worker/`. Report any
     literal missing from the list instead of adding it silently.
   - **Golden files.** Write the JSON for a deterministic fixture, with
     `generated_at` set to 0, to `plugin/tests/fixtures/status-full.json`
     and `status-brief.json`. A test compares the output with them, so the
     Rust side and `tst_model.qml` (step 2 reads the same files) can't
     drift.
   - **Tests:**
     - the shape;
     - brief null counts;
     - **brief runs no count query:** on a fixture DB with
       `ALTER TABLE chunks RENAME TO chunks_x`, brief succeeds and full
       errors;
     - normalisation;
     - the reason filter on a root and on a config error (a path-bearing
       reason becomes `other`);
     - config-error mapping: exact match, nested `/data/pub` and
       `/data/pub/moving`, `/data/a` against `/data/ab`, and a stale row;
     - no `/` in any reason or config-error value or in `settings`;
     - `lifted_builtin` as a count;
     - golden text output.

   → verify with the Rust check, then, from the worktree, with the fixture
   config the tests build:

   ```bash
   cargo run -- --config <fixture config> status --json --brief | jq .
   ```

   Traps:
   - the config must be set before `begin()`, because `visible()` needs
     it, as `run` does today;
   - `Mode::Both` stays read-only (`db.rs:63-71`).

2. **`plugin/Model.js` and `plugin/tests/tst_model.qml`** (new),
   implementing the Model contract and the level table above.
   - `tst_model.qml` is a QtTest `TestCase` that imports `../Model.js`.
     It covers:
     - every level row, and priority between rows;
     - full then brief; brief then a late, older full (reversed order); an
       older brief ignored; a brief never clearing counts;
     - unit mapping for each `ActiveState`, plus empty and garbage input;
     - an initial failure, a failure after success, and recovery;
     - stale at `STALE_S + 1` and not at `STALE_S - 1`;
     - `validate` rejecting the wrong shape;
     - `validate` accepting both golden files, read with `XMLHttpRequest`
       from `fixtures/`.
   - Step 2 runs in parallel with step 1. Until step 1's golden files
     exist, write them by hand from the contract above. The lead
     reconciles them at step 1's commit.

   → verify with the QML test command. Trap: no `console.*` or `print`,
   even in the test.

3. **`plugin/manifest.json`, `qmldir`, `IndexState.qml`, `Panel.qml` and
   `IndexView.qml`** (new), implementing the Shell behaviour above.
   - Model them on
     `/mnt/code/Source-home/GitHub/nixarchy/pkgs/rebuild-panel/`.
   - **`manifest.json`:**
     - `schemaVersion: 1`, `id: "olafkfreund.llm-index"`,
       `name: "llm-index"`;
     - `version` `0.1.0`, `author` `olafkfreund`, `license` `MIT`;
     - a one-line `description`;
     - `kinds: ["bar-widget"]`, `entryPoints.barWidget: "Panel.qml"`;
     - `barWidget` with `displayName: "llm-index"`, `category: "System"`,
       `allowMultiple: false`, `defaultSection: "right"`.
   - **Components:** use only `qs.Ui` and `qs.Commons` items that
     rebuild-panel uses, or that are listed in `Ui/qmldir` (`Panel`,
     `KeyboardPanel`, `BarIconButton`, `PanelKeyCatcher`,
     `PanelSectionHeader`, `ConfirmDialog`), plus `Color` and `Style`.

   → verify with:

   ```bash
   nix shell nixpkgs#qt6.qtdeclarative -c sh -c 'for f in plugin/*.qml; do qmllint --bare "$f" 2>&1 | grep "\[syntax\]" && exit 1; done; true'
   ```

   It catches syntax only. Wrong properties show up in the lead's live load.
   Traps:
   - no `sh -c` or `bash -c`;
   - no `Qt.openUrlExternally`;
   - no console output;
   - don't write `shell.json`.

4. **Packaging and `checks.plugin`:** `flake.nix:37-42` and `:60-80`, new
   `nix/plugin.nix`, `nix/checks/plugin.nix` (the check, called like the
   other checks), `plugin/tests/lint.sh` and `plugin/tests/bad/`.
   - **`packages.${system}.plugin`:** `lib.fileset.toSource` of `plugin/`
     minus `plugin/tests`, **wrapped in a derivation**, because `toSource`
     alone is not one:

     ```nix
     pkgs.runCommandLocal "llm-index-shell-plugin" { inherit src; } "cp -r --no-preserve=mode $src $out"
     ```

     `manifest.json` sits at `$out`.
   - **`checks.${system}.plugin`:** a `runCommand` with
     `qt6.qtdeclarative`. It sets `export HOME=$TMPDIR`,
     `QT_QPA_PLATFORM=offscreen` and `QML_XHR_ALLOW_FILE_READ=1`
     (the golden files load through `XMLHttpRequest`), then runs:
     - **the syntax gate**, copied from nixarchy `tests/qml.nix:57-80`:
       `qmllint --bare … || true`, failing on any `[syntax]` line, with its
       own broken-brace self-test fixture;
     - **`lint.sh`** on `plugin/*.qml` and `plugin/*.js`, which must
       pass;
     - **the lint self-test:** one fixture per rule in `plugin/tests/bad/`.
       `lint.sh` must fail on each, and the check greps for that rule's
       own diagnostic, so a broken rule can't hide behind another;
     - **`qmltestrunner`** with `-import ${qt6.qtdeclarative}/lib/qt-6/qml`
       on `tst_model.qml`.
   - **`lint.sh` rules**, each printing `RULE <name>: <file>:<line>`:

     | Rule | Fails when |
     | --- | --- |
     | `plaintext` | a QML block of type `Text`, `PanelSectionHeader`, `Label`, `TextEdit` or `TextInput` lacks `textFormat: Text.PlainText` **in that same block**. Its fixture has two `Text` blocks, one compliant and one not. |
     | `console` | `console.` or `print(` appears in `.qml` or `.js`. |
     | `shell` | the `"sh"`, `"bash"` or `"-c"` argv strings appear, or `sh -c` / `bash -c`. |
     | `openurl` | `Qt.openUrlExternally` appears. |

   → verify with:

   ```bash
   nix build .#plugin -o result-plugin && ls result-plugin && nix build .#checks.x86_64-linux.plugin --no-link
   ```

   Traps:
   - the Rust package's fileset (`nix/package.nix:36-38`) must not include
     `plugin/`;
   - existing checks keep their names.

5. **Docs:** `docs/reference.md` (`status` section near `:30`), `README.md`
   and `docs/security.md`.
   - In `reference.md`, document:
     - `status --json` and `--brief`, with the contract, the field
       meanings, and that brief counts are `null`;
     - the closed reason list;
     - the config-error mapping.
   - Add a short "Desktop plugin" section to the README. It covers what the
     plugin shows, the key, and that settings stay in Nix.
   - In `security.md`, record:
     - private metadata appears only in the open panel;
     - the tooltip and IPC carry a level only;
     - stderr is discarded;
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
       `programs.nixarchy.menu.extraEntries."apps.llm-index"`, at the levels
       where `omarchy-gmessages.nix` and `omarchy-keys.nix` set them;
     - bump the input.
   - Run `just validate` and build p620 and razer.
   - Deploy with `just p620`, then `just deploy-via-p620 razer`, announcing
     on the agent bus first.

   Traps:
   - work in a worktree, deploy from main;
   - p510 is never touched;
   - check the chord again against the host files at that time.

## Tests (lead)

- **After each step's commit,** run the Rust check and the QML test. After
  step 4, run `nix flake check`, covering the existing VM, fixture, closure
  and module checks, plus `checks.plugin`.
- **Build links:**

  ```bash
  nix build .#default -o result-cli
  nix build .#plugin -o result-plugin
  ```

- **Text output.** On p620, comparing today's binary with the branch build
  prints nothing:

  ```bash
  diff <(llm-index status) <(./result-cli/bin/llm-index status)
  ```

- **Timing and paths.** On p620, using `/usr/bin/time` with
  `./result-cli/bin/llm-index` (and on razer after step 6):
  - `status --json --brief` under 50 ms;
  - `status --json` close to today's 3 s;
  - both outputs pass `Model.validate`: run `tst_model.qml` with
    `LLM_INDEX_LIVE_JSON` pointing at a saved copy, which is optional and
    skipped when unset;
  - with `jq`, no `/` in any reason or config-error field, and none in
    `settings`.
- **Live load before the PR merges,** on p620 only. This is the only check
  that catches a wrong `qs.Ui` property.
  1. Record whether `olafkfreund.llm-index` exists in
     `~/.config/omarchy/plugins` (it should not).
  2. Link `result-plugin` there, prefix `PATH` for the shell with
     `result-cli/bin`:

     ```bash
     systemctl --user set-environment PATH=$PWD/result-cli/bin:$PATH
     ```

     then rescan and enable the plugin.
  3. Check the shell journal for load errors, and that both tabs render
     real data.
  4. **Cleanup always runs, even if a check fails:** disable the plugin,
     remove the link, `systemctl --user unset-environment PATH`, then
     rescan.

  If the environment change doesn't reach the shell, accept the "Status
  unavailable" view for this load test and leave data checks to after
  step 6.
- **After step 6, on p620 and then razer:**
  - after a re-login, the plugin is enabled;
  - the key and the menu row open it;
  - the tabs match `llm-index status`;
  - Refresh shows the busy marker, then updates;
  - Open log opens a terminal following the journal;
  - `systemctl --user stop llm-index` gives `down` within 10 s. Restart
    from the panel (keyboard confirm) clears it. If the panel check fails,
    run `systemctl --user start llm-index` afterwards;
  - `omarchy-shell olafkfreund.llm-index.bar status` and the tooltip show a
    level only;
  - the shell journal has nothing from the plugin;
  - a disable survives a re-login, then re-enable it;
  - on p620's two monitors, sample every 5 s for 2 minutes with the panel
    open. `pgrep -c -f 'status --json --brief'`, the full-run count and
    the `systemctl --user show llm-index` count each stay at most 1.
- **Deviation from the spec's verification.** The live "rename the config
  so the status read fails" test is replaced by `tst_model.qml`'s
  failure-after-success and stale cases, so the live config is never
  touched.
- **Reviews:** fresh Codex and agy instances, read-only, on
  `git diff origin/main` against this plan before the PR.

## Rollback

- **This repository:** revert the PR. Use `git revert -m 1 <merge>` for a
  merge commit, or `git revert <sha>` for a squash. The new flags and the
  plugin package are additive, and the text `status` and the MCP are
  unchanged. A revert here doesn't change any host until nixos_config moves
  its pinned input.
- **nixos_config:** revert step 6's commit and redeploy **before** bumping
  the input to a commit without `.#plugin`, or evaluation fails.
  - The plugin is uninstalled.
  - The `enableOnce` marker
    (`~/.local/state/nixarchy-local/enabled-once/olafkfreund.llm-index`)
    stays. Delete it before a later re-deploy so the plugin is enabled
    again, or run `omarchy plugin enable olafkfreund.llm-index` once.
  - Re-login so the shell drops the singleton.
- Index databases are never touched.

## Deviations found in review (recorded with the code)

- `generated_at` is stamped when the read snapshot starts, not after the
  counts, so a slow full run can't overwrite a newer brief result.
- Keyboard: the view follows rebuild-panel's pattern instead of
  `PanelKeyCatcher`. The view takes focus, Escape closes, and Tab walks the
  focusable buttons. `PanelKeyCatcher` swallowed Tab and Enter, so no button
  was reachable.
- The tab content scrolls inside a bounded `Flickable`, so long root lists
  stay inside the panel.
- Deadlines record the failure immediately, send SIGTERM, and SIGKILL after
  2 s. A start that never happens is caught by the deadline, not guessed
  right after `running = true`. A refresh asked for during a full run runs
  once it ends.
- IPC uses the shell's `ShellIpc` (one live handler per target across
  monitors) instead of a bare `IpcHandler`.
- Theme colours go through `Commons.Color` (`import qs.Commons as Commons`).
  A bare `Color` resolved to undefined in the live shell.
- `Model.validate` requires integer counts in full snapshots.
- The lint tokenises whole files: all string forms and comments, own-block
  `textFormat` only, qualified type names, and path-qualified shells. Each
  bad fixture states its expected count, and the check compares it.
- Root names are copied as configured. The docs say so instead of claiming
  no path can appear anywhere.
- `nix/package.nix` includes `plugin/tests/fixtures`, because the packaged
  cargo test reads the golden files. The plugin sources stay out.
- If `llm-index` is missing from `PATH`, Quickshell itself logs the failed
  start (command name only). The docs say so.
