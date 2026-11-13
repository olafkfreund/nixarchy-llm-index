# Installation

llm-index runs as one background service that indexes the folders you name.
Pick a route, add the flake, describe your roots, rebuild.

Requirements: `x86_64-linux`, Nix with flakes, and a kernel with Landlock
(see [Landlock](#landlock)).

## Choose a route

| | Home Manager | NixOS |
| --- | --- | --- |
| Runs as | your user, as a systemd user unit `llm-index.service` | a system unit `llm-index-<owner>.service`, as user `<owner>` |
| Options under | `services.llm-index` | `services.llm-index.instances.<owner>` |
| Turned on by | `enable = true` | defining the instance (there is no `enable`) |
| Config file | `$XDG_CONFIG_HOME/llm-index/config.toml` | `/etc/llm-index/<owner>.toml` |
| State directory | `$XDG_STATE_HOME/llm-index` (`~/.local/state/llm-index`) | `/var/lib/llm-index/<owner>` |
| Needs | Home Manager | an existing user account `<owner>` |

Use Home Manager for a personal machine. Use NixOS to run one index per
account from the system configuration. One route per index: do not index the
same roots through both.

## Add the flake

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    llm-index = {
      url = "github:olafkfreund/nixarchy-llm-index";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
}
```

The flake exposes `homeManagerModules.default`, `nixosModules.default` and
`packages.x86_64-linux.default`.

## Home Manager

```nix
{ inputs, config, ... }:
{
  imports = [ inputs.llm-index.homeManagerModules.default ];

  services.llm-index = {
    enable = true;
    roots = {
      code = {
        path = "${config.home.homeDirectory}/Source";
        tier = "public";
      };
      documents = {
        path = "${config.home.homeDirectory}/Documents";
        # tier defaults to "private"
      };
    };
    exclude.paths = [ "${config.home.homeDirectory}/Documents/Scratch" ];
  };

  # The module does not put llm-index on PATH.
  home.packages = [ config.services.llm-index.package ];
}
```

The root path must be the real path with no symlink anywhere in it; check with
`realpath ~/Source` and use what it prints. Apply it with your usual Home
Manager switch. The unit starts at login.

With `registerPublic = true` (the default) the module sets
`programs.mcp.servers.llm-index-public`. That helps only clients whose Home
Manager module reads `programs.mcp`. The private server is never registered
automatically. See [agents](agents.md).

## NixOS

```nix
{ inputs, config, ... }:
{
  imports = [ inputs.llm-index.nixosModules.default ];

  # The module does not create the account; it only sets User= on the unit.
  users.users.alice = {
    isNormalUser = true;
    packages = [ config.services.llm-index.instances.alice.package ];
  };

  services.llm-index.instances.alice = {
    roots = {
      code = {
        path = "/home/alice/Source";
        tier = "public";
      };
      documents.path = "/home/alice/Documents";
    };
  };
}
```

Rebuild with `nixos-rebuild switch`. The instance name is the user the service
runs as. The user must exist; the module does not create it.

The module does not put `llm-index` on `PATH`. Add
`config.services.llm-index.instances.<owner>.package` to
`environment.systemPackages` or, as above, to the owner's
`users.users.<owner>.packages`.

The config file is `/etc/llm-index/<owner>.toml`, not the CLI default, so
every command needs `--config`:

```sh
llm-index --config /etc/llm-index/alice.toml status
```

## Run the commands without `PATH`

Each route exposes read-only `mcpServers.public` and `mcpServers.private`
values: `{ command; args; }` with the absolute store path of the binary and
the right `--config` already filled in. Use them to register a client without
touching `PATH`:

```nix
# Home Manager
config.services.llm-index.mcpServers.public
# NixOS
config.services.llm-index.instances.alice.mcpServers.private
```

They evaluate to, for example:

```nix
{
  command = "/nix/store/…-llm-index-0.1.0/bin/llm-index";
  args = [ "mcp" "--tier" "public" "--config" "/etc/llm-index/alice.toml" ];
}
```

## Options

Defaults are those of the modules. Home Manager options sit directly under
`services.llm-index`. NixOS options sit under
`services.llm-index.instances.<owner>` and differ only where noted.

### Instance

| Option | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enable` | boolean | `false` | Home Manager only. NixOS has no `enable`. |
| `package` | package | the flake's `default` | The `llm-index` package. |
| `registerPublic` | boolean | `true` | Home Manager only. Sets `programs.mcp.servers.llm-index-public`. |
| `stateDir` | string | HM: `$XDG_STATE_HOME/llm-index`; NixOS: `/var/lib/llm-index/<owner>` | Holds `public.db` and `private.db`, mode `0700`. On NixOS it must be under `/var/lib/` (it becomes the unit's `StateDirectory`). |
| `mcpServers.public` | read-only | | `{ command; args; }` of the public MCP server. |
| `mcpServers.private` | read-only | | `{ command; args; }` of the private MCP server. |
| `agentInstructions` | read-only string | the text of `share/AGENTS.snippet.md` | Paste into `CLAUDE.md` / `AGENTS.md` / `GEMINI.md`. See [agents](agents.md). |

### `roots.<name>`

`<name>` must not be empty.

| Option | Type | Default | Meaning |
| --- | --- | --- | --- |
| `path` | string | none (required) | Absolute, normalized path with no symlink component. |
| `tier` | `"public"` \| `"private"` | `"private"` | Only public roots are visible to the public MCP server. |
| `kind` | `"auto"` \| `"local"` \| `"network"` | `"auto"` | Storage kind. `auto` is detected at run time and counts as network when sizing `TasksMax`. |
| `interval` | string | `"15m"` | Rescan interval for network roots, including `auto` roots detected as network. Local roots are watched; while their watcher is degraded they are polled every 120 s, whatever `interval` says. `[0-9]+[smhd]`, nonzero. |
| `share` | null or string | `null` | Share identity for a root on a network mount, for example `"nas:/export"`. |

### `exclude`

| Option | Type | Default | Meaning |
| --- | --- | --- | --- |
| `exclude.paths` | list of string | `[]` | Paths to skip. Each must be at or under a root. |
| `exclude.liftBuiltin` | list of string | `[]` | Built-in denylist patterns to lift. Applies to private roots only. See [security](security.md). |

### `limits`

| Option | Type | Default | Meaning |
| --- | --- | --- | --- |
| `limits.maxFileMb` | positive integer | `50` | Larger files are skipped. |
| `limits.maxTextBytes` | positive integer | `33554432` | Extracted text above this fails the file. |
| `limits.maxCells` | positive integer | `2000000` | Cell budget per workbook (`.xlsx`, `.ods`, `.xls`). For `.ods` and `.xls` it is checked after the file is opened, so it bounds the work, not the first load. |
| `limits.memoryMaxMb` | positive integer | `1024` | `MemoryMax` of the unit, in MiB. `MemoryHigh` is 75% of it, rounded down. |
| `limits.workers` | integer | `2` | Worker processes. At least 2. |
| `limits.backoffMax` | string | `"60m"` | Longest wait before retrying an offline network share. `[0-9]+[smhd]`, nonzero. |

### Checked when you build

A bad value fails at evaluation, before anything is deployed. The modules
reject:

- a root or exclusion path that is not absolute and normalized (no `.`, `..`,
  `//` or trailing `/`);
- two roots with the same path;
- an empty root name;
- an exclusion not at or under a root;
- an `interval` or `backoffMax` that does not match `[0-9]+[smhd]`, or is zero;
- `limits.workers` below 2;
- a root, exclusion or `stateDir` containing a control character (C0 or DEL);
- NixOS only: a `stateDir` outside `/var/lib/`.

The program is stricter than the modules: when it loads the config it also
refuses invisible and bidirectional Unicode characters in those paths, and
paths that are not valid UTF-8.

## Permissions

The service reads your files as the user it runs as. It cannot read what that
user cannot read.

- Home Manager: your account.
- NixOS: `<owner>`. For roots under another user's home, `<owner>` needs read
  access there. Prefer one instance per account.

Workers run under Landlock and can read only beneath their root and
`/nix/store`. The unit gets `NoNewPrivileges`, `AF_UNIX`-only sockets and a
syscall filter. On NixOS the unit also sets `PrivateNetwork`. Details in
[security](security.md).

## Network roots

llm-index does not mount shares. Mount the share yourself (NFS, CIFS and so
on), then point a root at the mount point.

```nix
roots.nas = {
  path = "/mnt/nas/archive";
  kind = "network";
  share = "nas:/export/archive";
  interval = "30m";
};
```

- `kind = "auto"` detects NFS, CIFS, SMB and FUSE mounts at run time. Set
  `"network"` to be explicit.
- Network roots are rescanned every `interval`. Local roots are watched for
  changes.
- If a share stops answering, its root goes `offline`. Its files stay
  searchable with `stale: true`. The share is retried after 60 seconds, then
  at doubling intervals up to `limits.backoffMax`.
- One worker at a time runs per share, so a hung mount cannot take more than
  one slot.
- A root that cannot be verified fails closed: it is marked `offline`,
  `degraded` or `config_error` rather than indexed blindly. Check with
  `llm-index status`.

## State directory

`stateDir` holds the databases. The service refuses to start if it is:

- on a network or FUSE filesystem;
- inside a folder that holds a sync-client marker: `.stfolder`, `.sync`,
  `.dropbox`, `.nextcloudsync.log` or `.sync_*.db`.

Startup rejects a sync marker in **any** parent folder, whatever the sync client
excludes. If you sync your home folder (Syncthing, Dropbox, Nextcloud), set
`stateDir` outside the synced tree; excluding it in the client is not enough.
The state directory is never indexed.

SQLite temp files go in the state directory (the service sets `SQLITE_TMPDIR`).
After a root is removed, a database is compacted when over a quarter of it is
free; that needs free space of up to twice the database size in `stateDir`.

## Landlock

The service needs Landlock in the running kernel. NixOS enables it by default.
Before each start, the unit runs `llm-index index --self-test`. If Landlock is
missing the service refuses to start; there is no override.

```sh
llm-index --config /etc/llm-index/alice.toml index --self-test
```

On Home Manager leave out `--config`.

## Service and logs

Home Manager:

```sh
systemctl --user status llm-index
journalctl --user -u llm-index
llm-index status
```

NixOS (replace `alice` with the owner):

```sh
systemctl status llm-index-alice
journalctl -u llm-index-alice
llm-index --config /etc/llm-index/alice.toml status
```

The first scan takes a while on large folders. `status` shows `queued/done`
per root.

## Upgrading

Update the flake input, then rebuild and switch as usual:

```sh
nix flake update llm-index
```

On NixOS the unit restarts when its config file changes. State is kept in `stateDir`.

## Uninstalling

1. Remove the module configuration (and the `PATH` entry) and rebuild. This
   stops and removes the unit and the config file.
2. Delete the state directory by hand. The module does not:

   ```sh
   rm -rf ~/.local/state/llm-index      # Home Manager
   sudo rm -rf /var/lib/llm-index/alice # NixOS
   ```

3. Remove the `llm-index` entry from each agent client.
