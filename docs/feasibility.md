# Feasibility record (plan step 1)

Checked on 2026-11-11 for `plan/2026-11-11-1-llm-index.md` step 1. No
blockers.

## Pins

| Input | Pin |
| --- | --- |
| `nixpkgs` | `github:NixOS/nixpkgs/f4a03c008967841f2a0b3cb7c436fc6fc31671f6` (`nixos-unstable`, lib `26.11.20261107.f4a03c0`) |
| `home-manager` (checks only) | `github:nix-community/home-manager/82ce401aa350502897c3f2e85f384d61cd3b0b6d` |

## (a) Landlock by default

- Minimal `lib.nixosSystem` at the pin: `security.lsm = [ "landlock" "yama"
  "bpf" ]`, kernel parameter `lsm=landlock,yama,bpf`, default kernel 6.18.62.
- `pkgs/os-specific/linux/kernel/common-config.nix:811` sets
  `SECURITY_LANDLOCK = yes`.
- Kernel 6.18 gives Landlock ABI 6 (TCP rules need ABI 4). The `landlock`
  crate builds with `ABI::V6`.
- Reference host: `/sys/kernel/security/lsm` =
  `lockdown,capability,landlock,yama,bpf,ima`.

Result: **ok**.

## (b) pdfium

- Pinned `pdfium` 8110 (`pkgs/by-name/pd/pdfium/package.nix`) sets
  `pdf_enable_v8=false` and `pdf_use_partition_alloc=false`. XFA is off by
  default and requires V8, so it cannot be on. No override needed.
- Output has `lib/libpdfium.so`; closure 137.7 MiB.
- The package must bind `pdfium-render` to that store path at build time
  (for example a compile-time path to `libpdfium.so`), so the library is a
  real runtime dependency and counted in the closure.

Result: **ok**.

## (c) Model

`minishlab/potion-retrieval-32M`, revision
`6fc8051fab2a1e0ee76689cf08c853792ac285e7` (2026-03-27), licence MIT
(model card). Base URL
`https://huggingface.co/minishlab/potion-retrieval-32M/resolve/6fc8051fab2a1e0ee76689cf08c853792ac285e7/`.

| File | Bytes | Nix hash |
| --- | --- | --- |
| `model.safetensors` | 129,210,456 | `sha256-B2CeW9M6rTeQCz/WL07Jb22uyIyk1Gudi5KL+rq/bqA=` (matches LFS SHA-256 `07609e5b…6ea0`) |
| `tokenizer.json` | 1,493,150 | `sha256-fXXLxUMYE4gHxAGw8MlyERfGKLOd6Ojg7bbLF+DufRg=` |
| `config.json` | 202 | `sha256-Y8ANkIJMgywE7B0CtqmD+5BIm/BJ8p+/8VukgbikMu4=` |

`config.json`: `hidden_dim 512`, `normalize true`, tokenizer from
`baai/bge-base-en-v1.5`.

Result: **ok**.

## (d) Crates

Versions current on crates.io on 2026-11-11. All built together in a scratch
crate (`cargo run` printed `ok`), with all 13 grammars accepted by
`tree-sitter` 0.27 and an FTS5 `unicode61 remove_diacritics 2` table created.

| Crate | Version | Licence | Notes |
| --- | --- | --- | --- |
| `rmcp` | 3.9.0 | Apache-2.0 | features `server`, `transport-io` |
| `rusqlite` | 0.42.0 | MIT | features `bundled`, `functions` |
| `tokenizers` | 0.23.2 | Apache-2.0 | `default-features = false, features = ["onig"]` builds (no download code) |
| `safetensors` | 0.8.0 | Apache-2.0 | |
| `memmap2` | 0.9.12 | MIT OR Apache-2.0 | |
| `landlock` | 0.4.7 | MIT OR Apache-2.0 | |
| `inotify` | 0.11.5 | ISC | `default-features = false` |
| `ignore` | 0.4.34 | Unlicense OR MIT | |
| `tree-sitter` | 0.27.2 | MIT | |
| grammars | rust 0.24, python 0.25, go 0.25, javascript 0.25, typescript 0.23, nix 0.3, c 0.24, cpp 0.23, java 0.23, bash 0.25, toml-ng 0.7, yaml 0.7, json 0.24 | MIT | |
| `text-splitter` | 0.33.2 | MIT | features `markdown`, `code` |
| `pdfium-render` | 0.9.5 | MIT OR Apache-2.0 | `default-features = false`, `pdfium_latest`, `thread_safe` |
| `calamine` | 0.38.0 | MIT | |
| `csv` | 1.4.0 | Unlicense/MIT | |
| `zip` | 9.5.1 | MIT | `default-features = false`, `deflate` |
| `quick-xml` | 0.42.0 | MIT | |
| `scraper` | 0.29.0 | ISC | chosen, see (f) |
| `blake3` | 1.8.7 | CC0-1.0 OR Apache-2.0 | |
| `rustix` | 1.1.6 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | `fs`, `process` |
| `clap` | 4.6.7 | MIT OR Apache-2.0 | |
| `serde_json` | 1.0.154 | MIT OR Apache-2.0 | |
| `toml` | 1.2.1 | MIT OR Apache-2.0 | |

Transitive licences (`cargo metadata`, about 300 packages): MIT, Apache-2.0,
ISC, Unlicense, Zlib, BSD-2/3-Clause, BSL-1.0, CC0, Unicode-3.0 in various
`OR` combinations. The only copyleft mention is `r-efi`, licensed `MIT OR
Apache-2.0 OR LGPL-2.1-or-later`, used under MIT. No GPL.

Result: **ok**.

## (e) systemd `@sandbox`

Pinned systemd is 262, the same as the reference host, where
`systemd-analyze syscall-filter @sandbox` lists `landlock_add_rule`,
`landlock_create_ruleset`, `landlock_restrict_self` and `seccomp`. `@sandbox`
is not part of `@system-service`, so the units need
`SystemCallFilter=@system-service @sandbox`, as the plan says.

Result: **ok**.

## (f) HTML crate

Both are pure Rust on `html5ever` and link statically, so neither changes
the Nix closure; only binary size differs slightly. **Choice: `scraper`**,
one crate for both the `<title>` and body text (walk text nodes, skip
`script`, `style`, `noscript`), instead of `html2text` plus a second parser
for the title.

## Closure estimate

pdfium closure 137.7 MiB + model 123.2 MiB + tokenizer 1.4 MiB + binary
(estimate under 40 MiB) ≈ 300 MiB, under the 500 MB bound. Step 2 measures
it.
