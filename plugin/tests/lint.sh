#!/usr/bin/env bash
# Privacy lint for the shell plugin. Usage: lint.sh FILE...
# Prints "RULE <name>: <file>:<line>" per violation; exits 1 if any.
#
# Each file is tokenised whole: '...', "..." and `...` strings, // and /* */
# comments are recognised, so neither a string nor a comment can open, close
# or satisfy a block. A text block (Text, PanelSectionHeader, Label, TextEdit,
# TextInput, also when qualified as Q.Text) passes only if its own level, not
# a child's or a sibling's, assigns textFormat: Text.PlainText.
set -u
rc=0
for f in "$@"; do
  out=$(awk -v F="$f" '
    function report(rule, n) { printf "RULE %s: %s:%d\n", rule, F, n }
    function shellish(s) {
      return s == "-c" || s ~ /^(\/[^ ]*\/)?(sh|bash|dash|zsh|env)$/
    }
    { src = src $0 "\n" }
    END {
      n = length(src); line = 1; depth = 0; prev = ""; prev2 = ""; ident = ""
      for (i = 1; i <= n; i++) {
        c = substr(src, i, 1)
        if (c == "\n") { line++; continue }
        if (c == "/" && substr(src, i + 1, 1) == "/") {
          while (i <= n && substr(src, i, 1) != "\n") i++
          line++; continue
        }
        if (c == "/" && substr(src, i + 1, 1) == "*") {
          i += 2
          while (i <= n && !(substr(src, i, 1) == "*" && substr(src, i + 1, 1) == "/")) {
            if (substr(src, i, 1) == "\n") line++
            i++
          }
          i++; continue
        }
        if (c == "\"" || c == "\x27" || c == "`") {
          q = c; s = ""; i++
          while (i <= n && substr(src, i, 1) != q) {
            d = substr(src, i, 1)
            if (d == "\\") { i++; d = substr(src, i, 1) }
            if (d == "\n") line++
            s = s d; i++
          }
          if (shellish(s)) report("shell", line)
          prev2 = prev; prev = "STR"; continue
        }
        if (c ~ /[A-Za-z_]/) {
          w = c
          while (i < n && substr(src, i + 1, 1) ~ /[A-Za-z_0-9.]/) { i++; w = w substr(src, i, 1) }
          if (w ~ /(^|\.)console\./ || w == "console") report("console", line)
          if (w == "print" || w ~ /\.print$/) {
            j = i + 1; while (substr(src, j, 1) ~ /[ \t]/) j++
            if (substr(src, j, 1) == "(") report("console", line)
          }
          if (w ~ /openUrlExternally$/) report("openurl", line)
          if (depth > 0 && prev == ":" && prev2 == "textFormat" && w == "Text.PlainText") ok[depth] = 1
          prev2 = prev; prev = w; continue
        }
        if (c == "{") {
          t = prev; sub(/^.*\./, "", t)
          depth++; ok[depth] = 0; start[depth] = line
          istext[depth] = (t ~ /^(Text|PanelSectionHeader|Label|TextEdit|TextInput)$/)
          prev2 = prev; prev = "{"; continue
        }
        if (c == "}") {
          if (depth > 0) {
            if (istext[depth] && !ok[depth]) report("plaintext", start[depth])
            depth--
          }
          prev2 = prev; prev = "}"; continue
        }
        if (c ~ /[ \t\r]/) continue
        prev2 = prev; prev = c
      }
    }' "$f")
  [ -n "$out" ] && { printf '%s\n' "$out"; rc=1; }
done
exit $rc
