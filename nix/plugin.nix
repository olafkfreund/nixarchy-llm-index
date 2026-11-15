# The Omarchy shell plugin (manifest.json at $out), without its tests.
{ lib, runCommandLocal }:
let
  src = lib.fileset.toSource {
    root = ../plugin;
    fileset = lib.fileset.difference ../plugin ../plugin/tests;
  };
in
runCommandLocal "llm-index-shell-plugin" { inherit src; } "cp -r --no-preserve=mode $src $out"
