# Troubleshooting

Find your symptom, check the cause, apply the fix. Start every diagnosis with
the service status and the log:

```sh
# Home Manager
systemctl --user status llm-index
journalctl --user -u llm-index -e
llm-index status

# NixOS (replace alice with the owner)
systemctl status llm-index-alice
journalctl -u llm-index-alice -e
llm-index --config /etc/llm-index/alice.toml status
```

Messages below are quoted from the program.

## The service does not start

**`Landlock is unavailable: refusing to start`** or
**`config_error: landlock unavailable`**
: The kernel has no Landlock, or it is not in the active LSM list.
  Check with `cat /sys/kernel/security/lsm`; `landlock` must be listed. NixOS
  enables it by default; a custom `security.lsm` or kernel may not. There is no
  override.

**`state dir … is on a network or FUSE filesystem`**
: `stateDir` is on NFS, CIFS or FUSE. Move it to a local disk.

**`state dir … is inside a synced folder (… has ".stfolder")`**
: A parent folder holds a sync-client marker (`.stfolder`, `.sync`, `.dropbox`,
  `.nextcloudsync.log` or `.sync_*.db`). Set `stateDir` outside the synced tree
  or stop syncing it.

**`config.toml is on a network or FUSE filesystem: refusing to start`**
: The config file itself sits on a share. Keep it on a local disk.

**Self-test failures** (`llm-index index --self-test`):
`worker can open TCP`, `worker can read outside its root`,
`worker cannot read its root`, `worker created a file in its root`,
`oom_score_adj is …`, `coordinator can write outside the state dir`
: The sandbox does not hold on this host. Do not work around it. Run the
  self-test by hand to see which check fails:

  ```sh
  llm-index index --self-test                                         # Home Manager
  llm-index --config /etc/llm-index/alice.toml index --self-test      # NixOS
  ```

**Build or rebuild fails with a message naming an option**
: The modules check your config at evaluation time. The messages are:
  `limits.workers must be at least 2`; `roots: every path must be absolute and
  normalized (no ., .., // or trailing /)`; `roots: duplicate root path`;
  `roots: root names must be nonempty`; `exclude.paths: every exclusion must be
  absolute and normalized`; `exclude.paths: every exclusion must be at or under
  a root path`; `roots: interval must match [0-9]+[smhd] and be nonzero`;
  `limits.backoffMax must match [0-9]+[smhd] and be nonzero`; and on NixOS
  `stateDir must be under /var/lib/`. Fix the value named.

**`state_dir must be an absolute, normalized path`**, **`workers must be at
least 2`**, **`duplicate root path`**, **`invalid root path`**,
**`invalid exclusion`**
: The same rules, seen when a hand-written config is loaded.

**`roots or limits changed: restarting to apply`**
: Not a fault. The service exits when its roots or limits change so that
  systemd restarts it with the new config.

**`no absolute XDG_CONFIG_HOME or HOME`**
: The CLI cannot find its default config. Pass `--config <path>`.

## A root is not `ok`

Run `llm-index status` and read the state and reason.

| State and reason | Cause | Fix |
| --- | --- | --- |
| `config_error`, `root path contains a symlink` | A component of the root path is a symlink. | Use the real path in `path`. |
| `config_error`, `root path is not a directory` | The path is a file or does not resolve to a directory. | Fix `path`. |
| `config_error`, `landlock unavailable` | See "Landlock" above. | |
| `offline`, an OS error or a stall reason | The path is missing or the share stopped answering. | Mount the share; check the path. Retried after 60 s, then at doubling intervals up to `limits.backoffMax`. |
| `offline`, `identity mismatch` | The mount at the path is not the one the root was set up for (for example the wrong share is mounted). | Mount the right share, or fix `share`. |
| `degraded`, `watch degraded` (also `watcher exited`, `watch setup stalled`) | The watcher failed, for example the inotify limit or event queue was exceeded. The root is polled every two minutes instead. | Raise `fs.inotify.max_user_watches` or `max_queued_events`. |

A `config_error` root hides its files from search until fixed. `offline` and
`degraded` roots keep their files searchable, marked `stale: true`.

## A tool says `index configuration is being checked`

Public server only. It is its one answer for any config or validation problem:
the config is missing or invalid, a root is in `config_error`, or a root or
exclusion path has not yet been validated.
If it persists, run `llm-index status` as the owner: the private view shows the
real reason, including `config error:` lines. The private MCP server returns the
real message directly.

## A file is not found

`search` and `get` give no reason; `not found` covers missing and hidden files.
Check, in order:

1. **Is it under a root?** Roots are listed by `llm-index status`.
2. **Is the root `ok`?** See above.
3. **Is it denied?** Built-in denylist (`.env*`, keys, `.ssh/`, browser and
   agent profiles; see [security](security.md)), `exclude.paths`, an ignore
   file (`.gitignore`, `.ignore`, `.llmindexignore`), or the state directory.
   To allow a built-in pattern in a **private** root, add it to
   `exclude.liftBuiltin`.
4. **Is the type supported?** See [reference](reference.md#what-is-indexed).
   Unsupported extensions (for example `.pptx`) are not indexed.
5. **Is it over a limit?** Larger than `maxFileMb`, extracted text above
   `maxTextBytes`, or a spreadsheet above `maxCells`.
6. **Did extraction fail?** Failed files are recorded with a reason. The owner
   can list them:

   ```sh
   sqlite3 -readonly ~/.local/state/llm-index/private.db \
     "select abspath, reason from files where state = 'failed'"
   ```

   Use `public.db` for public roots, and `/var/lib/llm-index/<owner>` on NixOS.
   Reasons include `too_large`, `timeout`, `zip_bomb` and `worker died`; a parse
   error gives its own message. Skip labels are `mismatch`, `binary`,
   `minified`, `generated`, `too_large` and `unsupported`.
7. **Is it new?** Local roots update in seconds; network roots wait for their
   `interval`. Check `queued/done` in `status`.

`meaning search unavailable` means `mode: "meaning"` was used but the embedding
model could not be loaded. Use `words` or `hybrid`. If the log says
`store: embedder unavailable` or `store: no model directory baked in; indexing
without vectors`, the package was built without its model.

`path must be absolute` and `folder must be an absolute path`: give an absolute
path.

## A network share hangs or goes offline

- The root goes `offline` and its files stay searchable but `stale`. This is
  expected while the share is down. It is retried automatically.
- Other roots keep working: only one worker runs per share.
- After a remount, the root comes back at the next retry (up to
  `limits.backoffMax`; lower it for faster recovery).
- If a mount shows `identity mismatch`, the share at that path changed. Check
  `share` and the mount.
- llm-index does not mount anything. Mounting is yours.

## The client shows no tools

1. **Wrong or missing `--config`.** On NixOS the config is
   `/etc/llm-index/<owner>.toml`, not the default. Add
   `--config /etc/llm-index/<owner>.toml` to the client's arguments, or use
   `config.services.llm-index.instances.<owner>.mcpServers.public`, which
   includes it.
2. **`llm-index` is not on `PATH`.** Neither module installs it there. Add the
   package (see [installation](installation.md)) or use the absolute store path
   from `mcpServers.public`. Codex does not expand `~` or `$VAR`.
3. **The client did not reload.** Restart it, then run its check command
   (`claude mcp list`, `codex mcp list`, `gemini mcp list`).
4. **The server runs but answers an error.** Run it by hand as the owner and
   look at stderr:

   ```sh
   llm-index mcp --tier public --config /etc/llm-index/alice.toml
   ```

   It waits for JSON-RPC on stdin; press Ctrl-D to stop. The server starts
   even when the config is bad: the problem shows up as the answer to each
   tool call.
