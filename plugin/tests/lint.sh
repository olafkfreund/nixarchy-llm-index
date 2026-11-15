#!/usr/bin/env bash
# Privacy lint for the shell plugin. Usage: lint.sh FILE...
# Prints "RULE <name>: <file>:<line>" per violation; exits 1 if any.
# shortcut: braces are counted after stripping "strings" and // comments; a
# brace inside a regex literal or block comment would confuse it.
set -u
rc=0
for f in "$@"; do
  out=$(awk -v F="$f" '
    function report(rule, n) { printf "RULE %s: %s:%d\n", rule, F, n }
    {
      line = $0
      gsub(/"([^"\\]|\\.)*"/, "\"\"", line)   # drop string contents
      sub(/\/\/.*/, "", line)                 # drop line comments
      raw = $0
      if (raw ~ /(^|[^A-Za-z_])console\./ || raw ~ /(^|[^A-Za-z_.])print\(/) report("console", NR)
      if (raw ~ /"(sh|bash|-c)"/ || raw ~ /(^|[^A-Za-z_])(sh|bash)[ \t]+-c/) report("shell", NR)
      if (raw ~ /Qt\.openUrlExternally/) report("openurl", NR)
      if (line ~ /textFormat:[ \t]*Text\.PlainText/ && depth > 0) ok[depth] = 1
      rest = line
      while (match(rest, /[{}]/)) {
        pos = RSTART
        c = substr(rest, pos, 1)
        if (c == "{") {
          pre = substr(rest, 1, pos - 1)
          depth++
          ok[depth] = 0; start[depth] = NR; istext[depth] = 0
          if (match(pre, /[A-Za-z_][A-Za-z_0-9.]*[ \t]*$/)) {
            t = substr(pre, RSTART, RLENGTH); sub(/[ \t]+$/, "", t)
            if (t ~ /^(Text|PanelSectionHeader|Label|TextEdit|TextInput)$/) istext[depth] = 1
          }
          # a textFormat later on the opening line belongs to this block
          if (substr(rest, pos + 1) ~ /textFormat:[ \t]*Text\.PlainText/ ) ok[depth] = 1
        } else if (depth > 0) {
          if (istext[depth] && !ok[depth]) report("plaintext", start[depth])
          depth--
        }
        rest = substr(rest, pos + 1)
      }
    }' "$f")
  [ -n "$out" ] && { printf '%s\n' "$out"; rc=1; }
done
exit $rc
