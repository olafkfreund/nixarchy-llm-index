# Indexes the fixture tree in a sandboxed build (no network, empty cache) and checks recall
# and what must stay out of the index.
{
  runCommand,
  python3,
  coreutils,
  gnugrep,
  evalPackage,
  src, # the repository (tests/ and its fixtures)
}:
runCommand "llm-index-fixture"
  {
    nativeBuildInputs = [
      python3
      coreutils
      gnugrep
      evalPackage
    ];
  }
  ''
    export HOME=$TMPDIR
    python3 -I ${src}/tests/fixtures/make_fixture.py ${src}/tests/fixtures/tree $TMPDIR/fx
    cat > $TMPDIR/config.toml <<EOF
    state_dir = "$TMPDIR/state"
    [[root]]
    name = "fx"
    path = "$TMPDIR/fx"
    tier = "public"
    EOF
    llm-index --config $TMPDIR/config.toml index &
    pid=$!
    for i in $(seq 1 240); do
      if llm-index --config $TMPDIR/config.toml status 2>/dev/null | grep -Eq ' ok .*queued/done: 0/[1-9]'; then break; fi
      sleep 1
    done
    sleep 3
    llm-index --config $TMPDIR/config.toml status
    llm-index --config $TMPDIR/config.toml eval-recall --queries ${src}/tests/queries.toml
    python3 -I ${src}/tests/check_fixture.py $(command -v llm-index) $TMPDIR/config.toml $TMPDIR/fx
    kill $pid
    touch $out
  ''
