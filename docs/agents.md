# Connect an agent

llm-index speaks MCP over stdio. Each agent client starts
`llm-index mcp --tier public` (or `--tier private`) as a child process and
talks to it. Setup is: give the client the command, then tell the agent when to
use it.

Before you start, finish [installation](installation.md) and check that
`llm-index status` lists your roots.

## Which tier to register

| Tier | Sees | Register it with |
| --- | --- | --- |
| `public` | public roots only | any client |
| `private` | public and private roots | clients that run a **local** model only |

Tool results travel to the model. With a cloud client, a `public` root's
paths, titles and passages reach that client and its model provider. Only
put folders in a `public` root if you accept that. A `private` server's
results would reach the provider the same way, so do not register it with a
cloud client. See [security](security.md).

This page registers the public server. Private registration is covered under
[Local models](#local-models).

## Give the client the command

There are two ways to name the command. Use either with any client below.

1. **`llm-index` on `PATH`.** Neither module does this for you: see
   [installation](installation.md). On NixOS add
   `--config /etc/llm-index/<owner>.toml` to the arguments, because that is
   not the CLI default.
2. **The Nix-generated server.** The modules expose read-only
   `mcpServers.public` and `mcpServers.private`: `{ command; args; }` with
   the absolute store path and the right `--config`. Nothing needs to be on
   `PATH`.

To read route 2 as JSON:

```sh
# Home Manager (standalone); adjust the flake path and user to your setup
nix eval --json .#homeConfigurations.<you>.config.services.llm-index.mcpServers.public

# NixOS; adjust the flake path, host and owner to your setup
nix eval --json /etc/nixos#nixosConfigurations.<host>.config.services.llm-index.instances.<owner>.mcpServers.public
```

The output is an object with `command` and `args`. Copy them into the client's
entry. Below, route 1 is shown with `llm-index` and no `--config`; on NixOS
append `"--config", "/etc/llm-index/<owner>.toml"` to the arguments, or use
the values from `nix eval` instead.

### What `registerPublic` does

On Home Manager, `registerPublic = true` (the default) writes
`programs.mcp.servers.llm-index-public`. That only helps clients whose Home
Manager module reads `programs.mcp`; it does nothing for a client that does
not. The private server is never registered automatically. NixOS has no
`registerPublic`.

## Claude Code

Source: <https://code.claude.com/docs/en/mcp>, and `claude mcp add --help`
(checked on version 2.1.322).

Add the server for your user, in all projects. Put `--` before the server's own
flags:

```sh
claude mcp add -s user llm-index-public -- llm-index mcp --tier public
```

`-s` is the scope: `local` (default), `user` or `project`. `project` writes a
`.mcp.json` file in the project; this page does not cover its contents.

The JSON form takes the same `{ command; args }` object that `nix eval`
prints:

```sh
claude mcp add-json -s user llm-index-public '{"command":"llm-index","args":["mcp","--tier","public"]}'
```

Check it:

```sh
claude mcp list
```

This lists the configured servers and health-checks the approved ones.

## Codex

Sources: `codex mcp --help` (checked on codex-cli 0.167.2), the config types at
<https://github.com/openai/codex/blob/main/codex-rs/config/src/mcp_types.rs>,
and the user docs at
<https://github.com/openai/codex/blob/main/docs/config.md>.

Edit `~/.codex/config.toml`:

```toml
[mcp_servers.llm-index-public]
command = "llm-index"
args = ["mcp", "--tier", "public"]
```

Optional keys are `env`, `env_vars` and `cwd`. Or use the CLI:

```sh
codex mcp add llm-index-public -- llm-index mcp --tier public
```

Codex starts the command directly, without a shell. `~` and `$VAR` in
`command` or `args` are **not** expanded. Use absolute paths, for example
`"--config", "/etc/llm-index/alice.toml"`.

Check it:

```sh
codex mcp list
codex mcp get llm-index-public
```

## Gemini CLI

Source: <https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md>.
Not tested locally: Gemini CLI is not installed on the machine this page was
written on.

Edit `~/.gemini/settings.json` (for your user) or `.gemini/settings.json` (for
one project):

```json
{
  "mcpServers": {
    "llm-index-public": {
      "command": "llm-index",
      "args": ["mcp", "--tier", "public"]
    }
  }
}
```

Optional keys are `env`, `cwd`, `timeout` (milliseconds) and `trust`. Leave
`trust` at `false`.

There is also a CLI, `gemini mcp add [options] <name> <command> [args...]`. Its
`-s, --scope` is `user` or `project` and defaults to **project**, so pass
`-s user` for a user-wide entry. Whether server flags need a `--` there is not
confirmed; run `gemini mcp add --help` before using it, or use the settings
file above.

Check it:

```sh
gemini mcp list
```

## Other clients

**agy (Antigravity):** unverified. See its own documentation for where it reads
MCP servers.

**Any other MCP client** that can start a stdio server needs three things: a
command (`llm-index`, or the store path), its arguments
(`mcp --tier public`, plus `--config <path>` on NixOS), and the stdio
transport. No other client format is documented here.

## Tell the agent to use it

Registering the server makes the tools available. The agent snippet tells the
agent what they are for and how to treat their output. Paste it into the
client's instruction file:

| Client | File |
| --- | --- |
| Claude Code | `CLAUDE.md` |
| Codex | `AGENTS.md` |
| Gemini CLI | `GEMINI.md` |

The text is the module option `agentInstructions` (read-only), also shipped as
`share/llm-index/AGENTS.snippet.md` in the package and in the repository as
`share/AGENTS.snippet.md`. In Nix, for example:

```nix
config.services.llm-index.agentInstructions                      # Home Manager
config.services.llm-index.instances.<owner>.agentInstructions    # NixOS
```

The snippet also tells the agent that anything derived from a document is
untrusted data. See [reference](reference.md#untrusted-content).

## Local models

llm-index does not care which model answers. The chat model is independent of
the index; only the client matters. A local-model setup works when its
front-end supports MCP servers over stdio. If it does, register the server
exactly as above. If it does not, llm-index cannot be used from it.

Register the **private** server only in a front-end that runs a local model
and sends nothing to a hosted provider. The reason: private results contain
your private files' paths and text, and the client forwards tool results to its
model. If that model is hosted, private content leaves the machine.

For such a front-end, use the same `command` and `args` with `--tier private`
(and `--config <path>` on NixOS), or take `mcpServers.private` from the
module.

## Embeddings are separate

Search by meaning uses a static, pinned, offline embedding model that ships in
the package. It does not call any remote service and does not depend on the
chat model. See [feasibility](feasibility.md).

## Troubleshooting

If the client shows no tools, the usual causes are a missing `--config` on
NixOS or `llm-index` not on `PATH`. See [troubleshooting](troubleshooting.md).
