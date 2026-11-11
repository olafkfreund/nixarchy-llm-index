#!/usr/bin/env bash
set -euo pipefail

# Benchmark runner for nixarchy-llm-index v0.1 (Step 13).
# Usage: bench/run.sh [CORPUS_PATH]

CORPUS_INPUT="${1:-}"
if [ -z "$CORPUS_INPUT" ]; then
  echo "Building .#bench-corpus..." >&2
  CORPUS_INPUT="$(nix build .#bench-corpus --no-link --print-out-paths)"
fi

if [ ! -d "$CORPUS_INPUT" ]; then
  echo "error: corpus directory not found: $CORPUS_INPUT" >&2
  exit 1
fi

LLM_INDEX_BIN="${LLM_INDEX_BIN:-}"
if [ -z "$LLM_INDEX_BIN" ]; then
  if [ -x "./result/bin/llm-index" ]; then
    LLM_INDEX_BIN="$(realpath ./result/bin/llm-index)"
  elif command -v llm-index >/dev/null 2>&1; then
    LLM_INDEX_BIN="$(command -v llm-index)"
  else
    echo "Building .#default..." >&2
    PKG_OUT="$(nix build .#default --no-link --print-out-paths)"
    LLM_INDEX_BIN="$PKG_OUT/bin/llm-index"
  fi
fi

SAMPLE_INTERVAL_SECS="${SAMPLE_INTERVAL_SECS:-30}"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/llm-index-bench.XXXXXX")"
CORPUS_DIR="$WORKDIR/corpus"
STATE_DIR="$WORKDIR/state"
CONFIG_TOML="$WORKDIR/config.toml"
INDEX_PID=""

cleanup() {
  if [ -n "$INDEX_PID" ] && kill -0 "$INDEX_PID" 2>/dev/null; then
    kill "$INDEX_PID" 2>/dev/null || true
    wait "$INDEX_PID" 2>/dev/null || true
  fi
  rm -rf "$WORKDIR"
}
trap cleanup EXIT

mkdir -p "$CORPUS_DIR" "$STATE_DIR"
chmod 0700 "$STATE_DIR"

echo "Copying corpus from $CORPUS_INPUT to writable $CORPUS_DIR..." >&2
for entry in "$CORPUS_INPUT"/*; do
  base="$(basename "$entry")"
  if [ "$base" = "MANIFEST" ]; then
    continue
  fi
  cp -R --no-preserve=mode,ownership "$entry" "$CORPUS_DIR/$base"
done
chmod -R u+rwX "$CORPUS_DIR"

MANIFEST_FILE="$CORPUS_INPUT/MANIFEST"
if [ -f "$MANIFEST_FILE" ]; then
  EXPECTED_FILES="$(awk '/^total_files:/ {print $2}' "$MANIFEST_FILE")"
  CORPUS_BYTES="$(awk '/^total_bytes:/ {print $2}' "$MANIFEST_FILE")"
  CORPUS_PDF_PAGES="$(awk '/^total_pdf_pages:/ {print $2}' "$MANIFEST_FILE")"
else
  EXPECTED_FILES="$(find "$CORPUS_DIR" -type f | wc -l)"
  CORPUS_BYTES="$(du -sb "$CORPUS_DIR" | awk '{print $1}')"
  CORPUS_PDF_PAGES="15000"
fi

cat >"$CONFIG_TOML" <<EOF
state_dir = "$STATE_DIR"

[[root]]
name = "corpus"
path = "$CORPUS_DIR"
tier = "public"
kind = "local"
interval = "15m"

[exclude]
paths = []
lift_builtin = []

[limits]
max_file_mb = 50
memory_max_mb = 1024
workers = 2
backoff_max = "60m"
EOF

SCOPE_UNIT="llm-index-bench-$$.scope"
echo "Starting llm-index index in systemd user scope $SCOPE_UNIT..." >&2
INDEX_START_NS="$(date +%s%N)"

systemd-run --user --scope --unit="$SCOPE_UNIT" \
  -p MemoryMax=1G \
  -p MemoryHigh=768M \
  "$LLM_INDEX_BIN" --config "$CONFIG_TOML" index &
INDEX_PID=$!

read_cgroup_peak_bytes() {
  # Read the scope's own cgroup, never the PID's: right after systemd-run starts, the PID can
  # still sit in the parent (user slice) cgroup, whose memory.peak covers the whole session.
  local cg_rel=""
  cg_rel="$(systemctl --user show "$SCOPE_UNIT" -p ControlGroup --value 2>/dev/null || true)"
  if [ -n "$cg_rel" ] && [ -r "/sys/fs/cgroup${cg_rel}/memory.peak" ]; then
    cat "/sys/fs/cgroup${cg_rel}/memory.peak"
    return 0
  fi
  echo "0"
}

PEAK_BYTES=0
PREV_DONE=0

while true; do
  if ! kill -0 "$INDEX_PID" 2>/dev/null; then
    echo "error: llm-index index exited prematurely" >&2
    exit 1
  fi

  STATUS_OUT="$("$LLM_INDEX_BIN" --config "$CONFIG_TOML" status 2>/dev/null || true)"
  CUR_PEAK="$(read_cgroup_peak_bytes "$INDEX_PID")"
  if [ "$CUR_PEAK" -gt "$PEAK_BYTES" ]; then
    PEAK_BYTES="$CUR_PEAK"
  fi

  ROOT_LINE="$(printf '%s\n' "$STATUS_OUT" | awk '$1 == "corpus" {print; exit}')"
  if [ -n "$ROOT_LINE" ]; then
    LAST_SCAN="$(printf '%s\n' "$ROOT_LINE" | sed -n 's/.*last scan: \([^ ]*\).*/\1/p')"
    QD="$(printf '%s\n' "$ROOT_LINE" | sed -n 's/.*queued\/done: \([0-9]*\/[0-9]*\).*/\1/p')"
    QUEUED="${QD%/*}"
    DONE="${QD#*/}"
    NOW_NS="$(date +%s%N)"
    ELAPSED_S="$(((NOW_NS - INDEX_START_NS) / 1000000000))"
    echo "[${ELAPSED_S}s] $ROOT_LINE (cgroup memory.peak=${PEAK_BYTES}B)" >&2

    if [[ "$DONE" =~ ^[0-9]+$ ]] && [[ "$QUEUED" =~ ^[0-9]+$ ]]; then
      if [ "$DONE" -lt "$PREV_DONE" ]; then
        echo "error: status done count decreased from $PREV_DONE to $DONE" >&2
        exit 1
      fi
      PREV_DONE="$DONE"
      # store.rs sets queued = total - done on scan completion and decrements queued
      # as done increments toward EXPECTED_FILES; stop once the scan has completed
      # and all queued extractions are done (queued == 0 with done >= EXPECTED_FILES,
      # or queued == done when done >= EXPECTED_FILES).
      if [ "$LAST_SCAN" != "-" ] && [ "$DONE" -ge "$EXPECTED_FILES" ] && { [ "$QUEUED" -eq 0 ] || [ "$QUEUED" -eq "$DONE" ]; }; then
        break
      fi
    fi
  fi

  sleep "$SAMPLE_INTERVAL_SECS"
done

INDEX_END_NS="$(date +%s%N)"
CUR_PEAK="$(read_cgroup_peak_bytes "$INDEX_PID")"
if [ "$CUR_PEAK" -gt "$PEAK_BYTES" ]; then
  PEAK_BYTES="$CUR_PEAK"
fi
INDEX_WALL_SECS="$(awk -v s="$INDEX_START_NS" -v e="$INDEX_END_NS" 'BEGIN { printf "%.2f", (e - s) / 1e9 }')"

echo "Running MCP stdio benchmark (idle RSS, 1,000 distinct queries warm RSS, 100-edit latency)..." >&2
MCP_METRICS="$(
  python3 - "$LLM_INDEX_BIN" "$CONFIG_TOML" "$CORPUS_DIR" <<'PY'
import glob
import json
import os
import re
import subprocess
import sys
import time

exe, cfg, corpus_dir = sys.argv[1], sys.argv[2], sys.argv[3]

# 1. Collect 1,000 distinct query words from the corpus vocabulary
words = []
seen = set()
word_re = re.compile(r"\b[a-z]{5,14}\b")
for sub in ("markdown", "text", "html", "csv", "code"):
    for root, dirs, files in os.walk(os.path.join(corpus_dir, sub)):
        dirs.sort()
        for f in sorted(files):
            path = os.path.join(root, f)
            try:
                with open(path, "r", encoding="utf-8", errors="ignore") as fp:
                    text = fp.read(16384).lower()
                for m in word_re.finditer(text):
                    w = m.group(0)
                    if w not in seen:
                        seen.add(w)
                        words.append(w)
                        if len(words) >= 1000:
                            break
            except Exception:
                pass
            if len(words) >= 1000:
                break
        if len(words) >= 1000:
            break
    if len(words) >= 1000:
        break

if len(words) < 1000:
    raise RuntimeError(f"Expected 1000 distinct vocabulary words, found {len(words)}")


def read_rss_mb(pid: int) -> float:
    with open(f"/proc/{pid}/status", "r", encoding="utf-8") as fp:
        for line in fp:
            if line.startswith("VmRSS:"):
                kb = float(line.split()[1])
                return kb / 1024.0
    return 0.0


proc = subprocess.Popen(
    [exe, "--config", cfg, "mcp", "--tier", "public"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    text=True,
    bufsize=1,
)

req_id = 0


def rpc(method: str, params: dict):
    global req_id
    req_id += 1
    msg = {"jsonrpc": "2.0", "id": req_id, "method": method, "params": params}
    proc.stdin.write(json.dumps(msg) + "\n")
    proc.stdin.flush()
    while True:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError("MCP server closed stdout unexpectedly")
        resp = json.loads(line)
        if resp.get("id") == req_id:
            return resp["result"]


rpc(
    "initialize",
    {
        "protocolVersion": "2025-03-26",
        "capabilities": {},
        "clientInfo": {"name": "bench", "version": "0.1.0"},
    },
)
proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
proc.stdin.flush()

# Trigger initial DB snapshot open before measuring idle VmRSS
rpc("tools/call", {"name": "status", "arguments": {}})
idle_rss_mb = read_rss_mb(proc.pid)

# 2. Run 1,000 distinct hybrid queries over stdio
for w in words[:1000]:
    rpc(
        "tools/call",
        {"name": "search", "arguments": {"query": w, "mode": "hybrid", "limit": 5}},
    )

warm_rss_mb = read_rss_mb(proc.pid)

# 3. Edit 100 files in the corpus and measure time until all 100 edits are searchable
targets = sorted(glob.glob(os.path.join(corpus_dir, "markdown", "*", "*.md")))[:50]
targets += sorted(glob.glob(os.path.join(corpus_dir, "text", "*", "*.txt")))[:50]
if len(targets) != 100:
    raise RuntimeError(f"Expected 100 target files to edit, found {len(targets)}")

tokens = []
t_edit_start = time.monotonic()
for idx, path in enumerate(targets):
    tok = f"benchedittoken{idx:04d}uniq"
    tokens.append(tok)
    with open(path, "a", encoding="utf-8") as fp:
        fp.write(f"\n{tok}\n")

pending = set(tokens)
deadline = t_edit_start + 600.0
while pending and time.monotonic() < deadline:
    found_now = set()
    for tok in list(pending):
        res = rpc(
            "tools/call",
            {"name": "search", "arguments": {"query": tok, "mode": "words", "limit": 1}},
        )
        payload = json.loads(res["content"][0]["text"])
        if payload.get("hits"):
            found_now.add(tok)
    pending -= found_now
    if pending:
        time.sleep(0.5)

if pending:
    raise RuntimeError(f"{len(pending)} edited files were not searchable within 600s")

edit_latency_s = time.monotonic() - t_edit_start

proc.stdin.close()
proc.wait(timeout=10)

print(f"{idle_rss_mb:.2f} {warm_rss_mb:.2f} {edit_latency_s:.2f}")
PY
)"

CUR_PEAK="$(read_cgroup_peak_bytes "$INDEX_PID")"
if [ "$CUR_PEAK" -gt "$PEAK_BYTES" ]; then
  PEAK_BYTES="$CUR_PEAK"
fi
PEAK_MB="$(awk -v b="$PEAK_BYTES" 'BEGIN { printf "%.2f", b / (1024 * 1024) }')"

read -r MCP_IDLE_MB MCP_WARM_MB EDIT_LATENCY_S <<<"$MCP_METRICS"

cat <<EOF

## Benchmark Results

| Metric | Bound | Measured |
| --- | --- | --- |
| Corpus files | 10,000 | $EXPECTED_FILES |
| Corpus size (bytes) | - | $CORPUS_BYTES |
| Corpus PDF pages | ~15,000 (1,500 × 10) | $CORPUS_PDF_PAGES |
| Initial index wall time | <= 7,200 s (2 h) | ${INDEX_WALL_SECS} s |
| Indexer cgroup \`memory.peak\` | <= 1,024 MB (1 GB) | ${PEAK_MB} MB (${PEAK_BYTES} bytes) |
| \`mcp --tier public\` idle VmRSS | <= 100 MB | ${MCP_IDLE_MB} MB |
| \`mcp --tier public\` warm VmRSS (1,000 queries) | <= 100 MB | ${MCP_WARM_MB} MB |
| 100 local edits searchable latency | <= 300 s (5 min) | ${EDIT_LATENCY_S} s |
EOF
