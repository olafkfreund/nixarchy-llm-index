{ runCommand, closureInfo, package }:
let
  info = closureInfo { rootPaths = [ package ]; };
  limit = 500 * 1000 * 1000;
in
runCommand "llm-index-closure-size" { } ''
  total=0
  while read -r p; do
    s=$(du -sb --apparent-size "$p" | cut -f1)
    total=$((total + s))
  done < ${info}/store-paths
  echo "closure bytes: $total (limit ${toString limit})"
  [ "$total" -le ${toString limit} ] || { echo "closure too large"; exit 1; }
  echo "$total" > $out
''
