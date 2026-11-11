# Benchmark Specification and Results

## Reference Hardware

All release performance and resource bounds for `llm-index` v0.1 are defined
against the reference laptop:

- **Architecture:** `x86_64-linux`
- **CPU:** 4 cores
- **Memory:** 8 GB RAM
- **Accelerator:** None (no GPU)

## Corpus (`packages.x86_64-linux.bench-corpus`)

Built deterministically by `bench/corpus.nix` from the pinned `nixpkgs` input
tree and ten SHA-256-pinned plain-text UTF-8 Project Gutenberg editions, using
build-only converters (`pandoc`, `typst`, `python3` with `openpyxl`, `odfpy`,
and `csv`, plus `b3sum`):

- **Total files:** 10,000
  - **40% source code (4,000 files):** selected deterministically by sorted
    relative path from the pinned `nixpkgs` tree.
  - **20% Markdown and plain text (2,000 files):** 1,000 `.md` and 1,000 `.txt`
    files converted with `pandoc`.
  - **10% HTML (1,000 files):** 1,000 standalone `.html` files converted with
    `pandoc`.
  - **15% PDF (1,500 files):** 1,500 10-page `.pdf` documents (15,000 pages
    total) compiled with `typst`.
  - **10% Office documents (1,000 files):** 500 `.docx` and 500 `.odt` files
    converted with `pandoc`.
  - **5% Spreadsheets (500 files):** 200 `.xlsx` (`openpyxl`), 150 `.ods`
    (`odfpy`), and 150 `.csv` (`csv`) files.
- **Manifest (`$out/MANIFEST`):** records `total_files`, `total_bytes`,
  `total_pdf_pages`, per-format counts, and the sorted per-file BLAKE3 digest
  of all 10,000 files.

Verify deterministic corpus generation before running the benchmark:

```bash
nix build .#bench-corpus --out-link result-corpus-1
nix build .#bench-corpus --rebuild --out-link result-corpus-2
cmp result-corpus-1/MANIFEST result-corpus-2/MANIFEST
```

## Procedure

Run `bench/run.sh` on the reference laptop:

```bash
./bench/run.sh "$(nix build .#bench-corpus --no-link --print-out-paths)"
```

`bench/run.sh` executes the following steps:

1. Copies the 10,000 corpus files into a writable temporary directory and
   writes a temporary `config.toml` with one public root (`corpus`) and a fresh
   `state_dir`.
2. Launches `llm-index index` inside a systemd user scope limited with
   `systemd-run --user --scope -p MemoryMax=1G -p MemoryHigh=768M`.
3. Samples `llm-index status` every 30 seconds until all 10,000 files have been
   scanned and indexed (`done` monotonically increasing to 10,000 and
   `queued == 0`), recording total wall time and the scope cgroup's
   `memory.peak`.
4. Spawns `llm-index mcp --tier public` over stdio, initializes the MCP
   session, and records `VmRSS` from `/proc/<pid>/status` when idle.
5. Sends 1,000 distinct hybrid `search` queries drawn from the corpus
   vocabulary over stdio and records `VmRSS` after all 1,000 responses complete.
6. Appends unique tokens to 100 files across the corpus while `llm-index index`
   is watching the root and polls MCP `search` until all 100 edits are
   searchable, recording the end-to-end latency.

## Product Bounds

| Metric | Bound |
| --- | --- |
| Initial index time (10,000 mixed files) | `<= 2 h` (`7,200 s`) |
| Indexer cgroup `memory.peak` | `<= 1 GB` (`1,024 MB`) |
| `llm-index mcp` VmRSS (idle) | `<= 100 MB` |
| `llm-index mcp` VmRSS (warm, after 1,000 distinct queries) | `<= 100 MB` |
| 100 local edits searchable | `<= 5 min` (`300 s`) |

## Recorded Results

To be filled by the owner after running `bench/run.sh` on the reference laptop:

| Metric | Bound | Measured |
| --- | --- | --- |
| Host / CPU / RAM | x86_64, 4 cores, 8 GB, no GPU | |
| Corpus files | 10,000 | |
| Corpus size (bytes) | - | |
| Corpus PDF pages | 15,000 | |
| Initial index wall time | <= 2 h (7,200 s) | |
| Indexer cgroup `memory.peak` | <= 1 GB (1,024 MB) | |
| `mcp --tier public` idle VmRSS | <= 100 MB | |
| `mcp --tier public` warm VmRSS (1,000 queries) | <= 100 MB | |
| 100 local edits searchable latency | <= 5 min (300 s) | |

## Smoke run on p620 (not the reference laptop)

Run on 2026-11-11 to prove the procedure works; p620 has 128 cores and 251 GB
RAM, so the time is not evidence for the laptop bound. Memory figures do not
depend on core count and are indicative. Default limits (`workers = 2`,
`MemoryMax=1G`).

| Metric | Bound | Measured |
| --- | --- | --- |
| Initial index, 10,000 files (99 MB, 15,000 PDF pages) | ≤ 2 h | 93 s |
| Indexer cgroup `memory.peak` | ≤ 1 GB | 229 MB |
| `mcp --tier public` idle RSS | ≤ 100 MB | 14.3 MB |
| `mcp --tier public` RSS after 1,000 distinct queries | ≤ 100 MB | 34.7 MB |
| 100 local edits searchable | ≤ 5 min | 3.3 s |

Second smoke run pinned to 4 cores (`taskset -c 0-3`, same Threadripper
3995WX, default limits): initial index 83 s, cgroup `memory.peak` 223 MB
(includes page cache of the files read, reclaimable under `MemoryHigh`),
`mcp` 14.4 MB idle and 34.8 MB after 1,000 queries, 100 edits searchable in
2.9 s. With `workers = 2` the core count barely matters; disk and per-core
speed on the reference laptop decide the real figure.
