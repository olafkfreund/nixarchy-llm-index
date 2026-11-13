# Security

What llm-index protects, how, and what it does not.

## Two tiers

Each root is `public` or `private` (default `private`).

- Public roots are stored in `public.db`, private roots in `private.db`.
- `llm-index mcp --tier public` opens `public.db` only. It has no way to name
  `private.db`, and `status` through it never lists private roots.
- `llm-index mcp --tier private` reads both databases.

The public server also fails closed. Before every tool call it checks that the
config loads, that no root is in `config_error`, and that every root and
exclusion path has passed validation. If any check fails it answers only
`index configuration is being checked`, with no path and no data.

## What reaches your model

Tool results go to the agent client and from there to its model provider.

- With a **public** root, its paths, titles and passages reach every client you
  register for it, and that client's provider. Mark a folder `public` only if
  you accept that.
- The **private** server is for clients that run a local model. Registering it
  with a cloud client sends private content to that provider. Nothing in
  llm-index stops you; the choice is yours. See [agents](agents.md).

## Worker sandbox

Scan, extract and watch workers run under Landlock before they touch your files:

- read-only access beneath their own root and `/nix/store`, nothing else;
  the `validate` worker can read the filesystem but cannot write;
- no TCP, where the kernel's Landlock can restrict it (ABI 4 or later);
- address-space limit of 4 GiB, no core dumps, `oom_score_adj` of 1000;
- extract workers are not dumpable, so their crashes are not passed to the
  core-dump helper (no systemd-coredump entry) and unprivileged processes of
  the same user cannot `ptrace` them; other workers stay dumpable.

The coordinator may write only beneath the state directory and `/proc`. Its
Landlock rules are best effort: on a kernel that cannot restrict TCP, only the
systemd units' `RestrictAddressFamilies=AF_UNIX` keeps it off the network, so an
indexer started by hand on such a kernel lacks that layer. The coordinator and
the MCP servers open no listening socket.

The systemd unit adds `NoNewPrivileges`, `RestrictAddressFamilies=AF_UNIX`, a
syscall filter (`@system-service @sandbox`), `MemoryMax`, `MemoryHigh` (75% of
it), `TasksMax`, low CPU and idle I/O priority. The NixOS unit also sets
`PrivateNetwork`. The state directory has mode `0700`, the databases `0600`.

Before each start the unit runs `llm-index index --self-test`, which checks
that a worker cannot write in its root, can read its root, cannot read one file
outside it (the config file), has the right `oom_score_adj`, that the
extract worker is not dumpable and the scan worker is, and, where the
kernel can restrict TCP, cannot open TCP (the TCP probe is skipped otherwise).
If Landlock is missing, the service refuses to start.

## Which files are read

- **Built-in denylist.** Matched case-insensitively against the absolute path.
  It covers dotenv files (`.env*`), keys and certificates (`*.pem`, `*.key`,
  `id_rsa*`, `id_ed25519*`, `*.p12`, `*.pfx`, `*.kdbx`, `*.age`), credential
  and secret files (`.netrc`, `.git-credentials`, `.npmrc`, `.pypirc`,
  `.docker/config.json`, `.cargo/credentials*`, `nix.conf`, `rclone.conf`,
  `.config/containers/auth.json`, `credentials.tfrc.json`, `secrets.yaml`,
  `secrets.yml`, `secrets.json`, `credentials.json`,
  `service-account*.json`), and these folders: `.ssh/`, `.gnupg/`,
  `.password-store/`, `.aws/`, `.azure/`, `.kube/`, `.config/gcloud/`,
  `.config/gh/`, `.config/glab-cli/`, `.config/github-copilot/`,
  `.config/sops/`, `.config/age/`, `.local/share/keyrings/`,
  agent profiles (`.claude/`, `.codex/`, `.gemini/`, `.antigravity/`,
  `.config/Antigravity/`), browser profiles (`.mozilla/`, `.thunderbird/`,
  Chrome, Chromium, Brave, Vivaldi), and `/run/`, `/proc/`, `/sys/`.
  The full list is `BUILTIN` in `src/deny.rs`.
- **`exclude.paths`** skips more paths. Each must be at or under a root.
- **`exclude.liftBuiltin`** removes named built-in patterns from the denylist
  **for private roots only**. On a public root the denylist always applies.
- The state directory is never indexed.
- **No symlinks, no mount crossings.** Workers open files relative to the
  root with `openat2` and the flags `BENEATH`, `NO_SYMLINKS`, `NO_MAGICLINKS`
  and `NO_XDEV`. A symlink in a root path puts the root in `config_error`.
  Symlinks inside a root are skipped and counted (`skipped symlinks` in
  `status`).
- Ignore files (`.gitignore`, `.ignore`, `.llmindexignore`, global git ignore)
  are honored.

The denylist is applied when scanning, when extracting, and again when a result
is returned, so changing the config hides files at once.

## Untrusted document content

Documents can contain text that tries to steer an agent. llm-index reduces the
risk but cannot remove it.

- Unicode tag characters, bidi controls, zero-width characters and variation
  selectors are stripped from document text, titles, headings and symbols
  before anything is returned.
- `search` passages and `get` text are wrapped in `<untrusted-document>`
  with `&` and `<` escaped, so they cannot close the wrapper. Titles,
  symbols, headings and sheet names are escaped but not wrapped.
- The agent snippet tells the agent to treat all of it as data. See
  [reference](reference.md#untrusted-content).

## What is not protected

- **Parser bugs.** Parsers run on hostile input. A crash is contained to one
  file and one sandboxed process, not prevented.
- **Anyone with your account, including a local agent with a shell.** The
  databases are readable by the user the service runs as, and by root; file
  permissions are the only barrier. The tier split guards what an MCP client
  can ask the server, not an agent running as the owner with shell access
  (which can run `llm-index mcp --tier private` or read your files directly).
- **Paths are returned as stored, but unsafe names are never indexed.** A
  file or directory whose name has a control character, an invisible or
  bidirectional Unicode character (the set stripped from content), or bytes
  that are not valid UTF-8 is skipped (reason `unsafe_name`), and so is
  everything under it. It is skipped, not cleaned: a cleaned name would no
  longer open the file, and two names could collapse into one. Rows of this
  kind are removed by the next complete scan, and the MCP tools drop any such
  path they find. Other odd names (long, misleading but printable) still
  reach the model.
- **Configured paths follow the same rule.** The program refuses a root,
  exclusion or `state_dir` with such characters
  (`path contains control or invisible characters`).
- **Content you mark public** is sent to the model providers of the clients
  you connect.
- **A cloud client with the private server.** See above.
- **Kernel and Nix store.** Workers can read `/nix/store`.
