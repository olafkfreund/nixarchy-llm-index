# Syntax gate, privacy lint (plus its self-test) and the Model.js QtTest suite.
{
  lib,
  runCommand,
  qt6,
}:
let
  src = lib.fileset.toSource {
    root = ../../plugin;
    fileset = ../../plugin;
  };
  broken = builtins.toFile "Broken.qml" ''
    import QtQuick
    Item { property int n: 1; Text { text: "n=" + parent.n }} }
  '';
  rules = [
    "plaintext"
    "console"
    "shell"
    "openurl"
  ];
in
runCommand "llm-index-plugin-check"
  {
    nativeBuildInputs = [ qt6.qtdeclarative ];
    inherit src;
  }
  ''
    set -o pipefail
    export HOME=$TMPDIR QT_QPA_PLATFORM=offscreen QML_XHR_ALLOW_FILE_READ=1
    cd $src

    lint() { qmllint --bare "$1" 2>&1 || true; }

    echo "== syntax self-test"
    lint ${broken} | grep -q '\[syntax\]' || { echo "FAIL: qmllint missed a broken file"; exit 1; }
    for f in *.qml; do
      report=$(lint "$f")
      if [[ $report == *'[syntax]'* ]]; then
        echo "FAIL: $f"; printf '%s\n' "$report" | grep -A2 '\[syntax\]'; exit 1
      fi
    done

    echo "== privacy lint"
    bash tests/lint.sh *.qml *.js

    echo "== lint self-test"
    for r in ${lib.concatStringsSep " " rules}; do
      if diag=$(bash tests/lint.sh tests/bad/$r.qml); then
        echo "FAIL: lint.sh passed tests/bad/$r.qml"; exit 1
      fi
      want=$(sed -n 's|^// expect: ||p' tests/bad/$r.qml)
      got=$(grep -c "^RULE $r: " <<<"$diag" || true)
      [ "$got" = "$want" ] || { echo "FAIL: RULE $r reported $got of $want cases"; echo "$diag"; exit 1; }
    done

    echo "== qmltestrunner"
    qmltestrunner -import ${qt6.qtdeclarative}/lib/qt-6/qml -input tests/tst_model.qml
    touch $out
  ''
