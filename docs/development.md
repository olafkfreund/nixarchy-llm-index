# Development

Build, test and change llm-index. Everything runs on `x86_64-linux` with Nix
flakes.

## Dev shell

```sh
nix develop
```

The shell provides `cargo`, `rustc`, `clippy`, `rustfmt`, `pkg-config`,
`pdfium` and `oniguruma`, and sets:

| Variable | Value |
| --- | --- |
| `RUSTONIG_SYSTEM_LIBONIG` | `true`: link the system Oniguruma. |
| `LLM_INDEX_PDFIUM_LIB` | The `libpdfium.so` in the Nix store. |
| `LLM_INDEX_MODEL_DIR` | The embedding model directory in the Nix store. |

Outside the shell the last two are `unset` and the binary cannot read PDFs or
embed text. `llm-index paths` prints the values compiled in.

## Build and run

```sh
nix build            # the package, result/bin/llm-index
cargo build          # inside nix develop
```

## Tests

```sh
cargo test --all-features
cargo clippy --all-features --all-targets
cargo fmt --check
```

`--all-features` includes the test-only `eval` feature (the hidden
`eval-recall` command). The integration tests in `tests/` run the real
binary, workers and databases on temporary fixtures: `e2e.rs` (index then
search), `mcp.rs` (tools over stdio), `parser_crash.rs` (a hostile file fails
only itself) and `selftest.rs` (the sandbox self-test).

## Flake checks

```sh
nix flake check --no-build      # evaluates everything, builds nothing
nix flake check                 # builds and runs every check (slow; VM tests)
nix build .#checks.x86_64-linux.<name>   # one check
```

| Check | What it proves |
| --- | --- |
| `fixture` | Indexes the fixture tree in a sandboxed build and checks recall and what must stay out of the index. |
| `closure` | The package closure is at most 500 MB and still contains pdfium and the model. |
| `modules` | Both modules evaluate with minimal configs and have the documented properties (units, config files, `TasksMax`, assertions). |
| `tier` | The public server never shows private roots, paths or text; the private server sees both. |
| `freshness` | An edited or replaced file is searchable under its new content. |
| `reclassify` | Config changes (a tier change, a removed root, a private root nested in a public one, a new exclusion, a corrupt config) hide files from the public server at once or close it until validated. |
| `sandbox` | The self-test passes under the module; on a kernel without Landlock the unit refuses to start. |
| `access` | Another user cannot read the state directory; modes are `0700` and `0600`; nothing listens on a socket; synced or network state directories are refused; the state directory is not indexed. |
| `watch` | The indexer recovers from an exhausted inotify watch limit and from a queue overflow. |
| `network` | With an NFS share that hangs: at most one worker per share, hits are marked stale, nothing is deleted while the share is away, and a different export at the same path is an identity mismatch. |
| `network-mounts` | Automounts, a tmpfs over a subdirectory, dead shares next to a healthy one, a share mounted after start. |

The VM checks boot NixOS machines and take minutes each. Run one:

```sh
nix build .#checks.x86_64-linux.sandbox -L
```

## Benchmark

`bench/run.sh` builds a 10,000-file corpus and measures indexing time, memory
and latency against the targets. Method and results: [benchmark](benchmark.md).

## Repository layout

| Path | Contents |
| --- | --- |
| `src/main.rs`, `src/lib.rs` | CLI and library root. |
| `src/index/` | Coordinator: scheduler, shares, state directory, watching, store, validation. |
| `src/worker/` | Sandboxed workers: scan, extract, watch, sandbox setup. |
| `src/extract/` | Parsers per file type. |
| `src/mcp.rs`, `src/rank.rs`, `src/status.rs` | MCP server, search, status. |
| `src/auth.rs`, `src/deny.rs`, `src/sanitize.rs` | Visibility rules, denylist, text sanitizing. |
| `src/db.rs`, `src/embed.rs`, `src/chunk.rs` | Schema, embedding, chunking. |
| `nix/` | Package, shared module options (`config.nix`), the two modules, checks. |
| `share/AGENTS.snippet.md` | Agent instructions shipped in the package and as `agentInstructions`. |
| `tests/` | Integration tests, fixtures, MCP test client. |
| `bench/` | Benchmark corpus and runner. |
| `docs/` | This documentation. |
| `intent/`, `spec/`, `plan/` | Design records, one set per task. |

## Workflow

Work that touches more than one file goes through three files under
`intent/`, `spec/` and `plan/`, one slug per task. Each carries
`status: draft | approved` and links its predecessor. Implementation starts
once the plan is approved. The pull request links all three, and review checks
the diff against the plan.

## Keep the docs in step

A change to an option, a flag, a tool or its parameters updates
[reference](reference.md) and [installation](installation.md) in the same pull
request. A change to the agent snippet updates `share/AGENTS.snippet.md` and
the tool descriptions in `src/mcp.rs` together.
