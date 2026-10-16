#!/usr/bin/env bash
#
# Everything `scripts/check.sh` runs, and the release workflow's smoke test,
# side by side: what to run on a commit before it is pushed.
#
#   scripts/check-parallel.sh                     every check, at once
#   scripts/check-parallel.sh --version v0.2.0    ...plus: the tag matches
#   scripts/check-parallel.sh --root DIR          ...of the checkout at DIR
#
# `check.sh` runs its steps one after another, which is what CI wants: one
# log, read top to bottom. On a machine with cores to spare that is most of
# an hour where this is about ten minutes. Each job writes a log of its own,
# and at the end there is one line for each: `ok` or `FAIL`.
#
# It also runs what `check.sh` does not and `release.yml` does: a release
# build as a stable `meadow`, with the standard library compiled into it, and
# the smoke test of that build. A tag whose release cannot pass that is found
# here rather than after it is pushed.
#
# The jobs share nothing they write. Each `cargo` has a target directory of
# its own under `target-check/`, the collector's stress runs test copies of
# the examples, and the standard library is compiled once before anything
# reads it. Run one of these at a time in a checkout: two would be two builds
# of MeadowBoot in one directory. It wants about 40 GB of disk for a
# checkout, most of it under `target-check/`, which can be deleted after.
#
# `--root` is for checking a checkout that does not have this script, an
# older commit say, with this one.

set -uo pipefail

VERSION=""
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HERE="$ROOT"
while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift ;;
    --root) ROOT="$(cd "${2:?--root needs a directory}" && pwd)"; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done
cd "$ROOT"

WORK="$ROOT/target-check"
LOGS="$WORK/logs"
rm -rf "$LOGS" "$WORK/home" "$WORK/smokepkg" "$WORK/examples"
mkdir -p "$LOGS"
HOST="$(rustc -vV | sed -n 's/^host: //p')"
STARTED=$(date +%s)

NAMES=()
PIDS=()
job() {
  local name=$1
  shift
  (
    began=$(date +%s)
    "$@"
    code=$?
    echo $(($(date +%s) - began)) >"$LOGS/$name.seconds"
    exit $code
  ) >"$LOGS/$name.log" 2>&1 &
  NAMES+=("$name")
  PIDS+=($!)
}

# --- at once: every workspace's tests, the release build, the extension -------

if [ -n "$VERSION" ]; then
  job version bash "$HERE/scripts/version-check.sh" "$VERSION" "$ROOT"
fi

for ws in compiler eval glade buildtools installer; do
  # Every test binary, though one fails: a run is long, and what else is
  # wrong is worth knowing from the same one.
  job "test-$ws" bash -c "cd $ws && cargo test --quiet --no-fail-fast"
done

# As `release.yml` builds and smoke-tests a target it can run: a `meadow` for
# this machine compiles the standard library, the release build has it built
# in, and that build runs a program in an empty home and builds a package to
# a native executable.
job smoke bash -c '
  set -euo pipefail
  work="$1"; host="$2"
  export CARGO_TARGET_DIR="$work/release" MEADOW_CHANNEL=stable
  cd buildtools
  MEADOW_EMBED_RUNTIME=0 cargo build --quiet --release -p meadow
  "$CARGO_TARGET_DIR/release/meadow" __precompile-std "$work/std.bin" --target "$host"
  export MEADOW_PRECOMPILED_STD="$work/std.bin"
  cargo build --quiet --release --target "$host"
  bin="$CARGO_TARGET_DIR/$host/release/meadow"
  "$bin" --version
  echo "fun main () = println (show (6 * 7))" > "$work/smoke.mw"
  MEADOW_HOME="$work/home" "$bin" run "$work/smoke.mw" | tail -1 | grep -qx 42
  if [ -d "$work/home/lib/std" ]; then
    echo "Std was compiled: the precompiled one is not built in" >&2
    exit 1
  fi
  "$bin" init "$work/smokepkg"
  (cd "$work/smokepkg" && "$bin" run --release --aot . | tail -1 | grep -q "Hello, world!")
  echo "smoke test passed"
' smoke "$WORK" "$HOST"

if command -v npm >/dev/null 2>&1; then
  job vscode bash -c 'cd editors/vscode && npm install --silent && npm test --silent >/dev/null && bash build.sh >/dev/null && ls -1 *.vsix && bash ../vscode-theme/build.sh >/dev/null && ls -1 ../vscode-theme/*.vsix'
fi

# --- then: the language's own checks, which need a `meadow` -------------------
#
# Built apart from the test build, so that neither waits for the other.

M="$WORK/cli/debug/meadow"
if (cd buildtools && CARGO_TARGET_DIR="$WORK/cli" cargo build --quiet -p meadow) >"$LOGS/build-cli.log" 2>&1; then
  # The standard library is compiled once and kept: have that happen before
  # anything shares it.
  echo 'fun main () = println "warm"' >"$WORK/warm.mw"
  "$M" run "$WORK/warm.mw" >"$LOGS/warm.log" 2>&1

  job std-vm "$M" test --std
  job std-cek "$M" test --std --cek
  for example in examples/*/; do
    job "example-$(basename "$example")" "$M" test "$example"
  done
  job fmt "$M" fmt --check lib/Std examples

  # The collector under pressure, as `check.sh` has it: once with marking on
  # its own threads, once on the program's. The examples are copies, since
  # the plain runs above are building the originals.
  for mark_threads in 2 0; do
    job "gc-stress-$mark_threads" bash -c '
      set -e
      work="$1"; m="$2"; threads="$3"
      export MEADOW_GC_NURSERY=64 MEADOW_GC_TRIGGER=256 MEADOW_GC_VERIFY=1 MEADOW_GC_MARK_THREADS="$threads"
      (cd glade && CARGO_TARGET_DIR="$work/gc-$threads" cargo test --quiet)
      "$m" test --std
      mkdir -p "$work/examples/gc-$threads"
      cp -r examples/Concurrency examples/Stm "$work/examples/gc-$threads/"
      "$m" test "$work/examples/gc-$threads/Concurrency"
      "$m" test "$work/examples/gc-$threads/Stm"
    ' gc "$WORK" "$M" "$mark_threads"
  done
else
  echo "FAIL build-cli" >"$LOGS/unsorted.txt"
fi

# --- what each came to --------------------------------------------------------

# Counted here and not read back from the summary: a disk that fills up
# loses the summary, and that must not read as nothing having failed.
failed=0
[ -f "$LOGS/unsorted.txt" ] && failed=1
touch "$LOGS/unsorted.txt" || failed=1
for i in "${!PIDS[@]}"; do
  if wait "${PIDS[$i]}"; then
    result="ok  "
  else
    failed=1
    result="FAIL"
  fi
  # How long each took, which is what says where a slow run's time went.
  took=$(cat "$LOGS/${NAMES[$i]}.seconds" 2>/dev/null || echo "?")
  printf '%s %-28s %5ss\n' "$result" "${NAMES[$i]}" "$took" >>"$LOGS/unsorted.txt" || failed=1
done
sort "$LOGS/unsorted.txt" >"$LOGS/summary.txt" || failed=1
rm -f "$LOGS/unsorted.txt"

cat "$LOGS/summary.txt"
for name in $(sed -n 's/^FAIL \([^ ]*\).*/\1/p' "$LOGS/summary.txt"); do
  printf '\n\033[1m== %s\033[0m (%s)\n' "$name" "$LOGS/$name.log"
  grep -E "^failures:|^    [a-z_0-9]+$|test result: FAILED|error: test failed" "$LOGS/$name.log" | tail -60
  tail -20 "$LOGS/$name.log"
done
echo
echo "$(($(date +%s) - STARTED))s, logs in $LOGS"
if [ "$failed" = 0 ]; then
  printf '\033[1;32mall checks passed\033[0m\n'
else
  printf '\033[1;31msome checks failed\033[0m\n'
fi
echo "exit=$failed" >>"$LOGS/summary.txt"
exit "$failed"
