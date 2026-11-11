{ runCommand, closureInfo, package }:
let
  info = closureInfo { rootPaths = [ package ]; };
  limit = 500 * 1000 * 1000;
in
runCommand "llm-index-closure-size" { } ''
  total=$(cat ${info}/total-nar-size)
  echo "closure NAR bytes: $total (limit ${toString limit})"
  [ "$total" -le ${toString limit} ] || { echo "closure too large"; exit 1; }
  # pdfium is only referenced by path string; fail if it ever drops out of the closure.
  grep -qx ${package.passthru.pdfium} ${info}/store-paths || { echo "pdfium missing from closure"; exit 1; }
  grep -qx ${package.passthru.modelDir} ${info}/store-paths || { echo "model missing from closure"; exit 1; }
  echo "$total" > $out
''
