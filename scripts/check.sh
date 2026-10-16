#!/usr/bin/env bash
#
# Every check, in one place. CI runs *this script*, not its own copy of the
# steps — so a green run here means a green run there, and adding a check means
# editing one file.
#
#   scripts/check.sh                 tests, and the checks that pass today
#   scripts/check.sh --strict        ...plus rustfmt and clippy (not clean yet)
#   scripts/check.sh --version v0.2.0    ...plus: the tag matches the manifests
#
# `scripts/check-parallel.sh` runs the same checks side by side, and the
# release workflow's smoke test with them: what to run before a push.
#
# Runs from anywhere; paths are resolved against the repository root.

set -euo pipefail

cd "$(dirname "$0")/.."

STRICT=0
VERSION=""
while [ $# -gt 0 ]; do
  case "$1" in
    --strict) STRICT=1 ;;
    --version) VERSION="${2:-}"; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

# The tree is five independent Cargo workspaces, so there is no one
# `cargo test --workspace` that covers it. Missing one is the whole reason this
# script exists.
WORKSPACES="compiler eval glade buildtools installer"

step() { printf '\n\033[1m== %s\033[0m\n' "$1"; }

# --- version -----------------------------------------------------------------
#
# Release assets carry no version, so nothing downstream would notice a tag that
# disagrees with the manifests. Check it while we still can.

if [ -n "$VERSION" ]; then
  step "version"
  bash scripts/version-check.sh "$VERSION"
fi

# --- tests -------------------------------------------------------------------

for ws in $WORKSPACES; do
  step "test: $ws"
  (cd "$ws" && cargo test --quiet)
done

# --- the language itself -----------------------------------------------------
#
# `cargo test` in `buildtools` already runs the standard library's own tests
# through the library API. Running them again through the CLI is what proves the
# `meadow test` command works, which nothing else covers.
#
# Both engines, because there are two: the bytecode VM (the default) and the CEK
# machine behind `--cek`. The CEK is the specification, so a disagreement is a VM
# bug — and the cheapest place to notice one is here.

step "meadow test --std (bytecode VM)"
cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- test --std

step "meadow test --std (CEK machine)"
cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- test --std --cek

# The collector again, under pressure: a nursery of 64 slots, so nearly
# everything is promoted, and a marking cycle every few hundred slots, each one
# checked against a whole trace (`MEADOW_GC_VERIFY`). Once with marking on its
# own threads, once on the program's. A missed barrier or a race with the marker
# shows up here as a panic naming the object it lost, long before it would show
# up as a wrong answer.
for mark_threads in 2 0; do
  step "collector stress (marking threads: $mark_threads)"
  export MEADOW_GC_NURSERY=64 MEADOW_GC_TRIGGER=256 MEADOW_GC_VERIFY=1 MEADOW_GC_MARK_THREADS=$mark_threads
  (cd glade && cargo test --quiet)
  cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- test --std
  cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- test examples/Concurrency
  cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- test examples/Stm
  unset MEADOW_GC_NURSERY MEADOW_GC_TRIGGER MEADOW_GC_VERIFY MEADOW_GC_MARK_THREADS
done

# Every example package, on the VM: they are what someone new runs first, so
# they have to keep compiling and their own tests have to keep passing.
for example in examples/*/; do
  step "meadow test ${example%/}"
  cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- test "$example"
done

step "meadow fmt --check"
cargo run --quiet --manifest-path buildtools/Cargo.toml -p meadow -- fmt --check lib/Std examples

# --- the editor extension ----------------------------------------------------
#
# The `.vsix` is only built by `release.yml`, so until this was here a broken
# extension build was discovered when cutting a release — which is how it went
# unnoticed that `npx @vscode/vsce` follows the latest version and that vsce 3
# requires a newer Node than this repository targets.
#
# Skipped rather than failed without npm: the extension is not needed to work on
# the compiler, and CI installs Node so the check still gates a release.

step "vscode extension"
if command -v npm >/dev/null 2>&1; then
  (cd editors/vscode && npm install --silent && npm test --silent >/dev/null && bash build.sh >/dev/null)
  echo "  packaged $(ls -1 editors/vscode/*.vsix | head -1)"
  bash editors/vscode-theme/build.sh >/dev/null
  echo "  packaged $(ls -1 editors/vscode-theme/*.vsix | head -1)"
else
  echo "  skipped: npm not found"
fi

# --- strict ------------------------------------------------------------------
#
# Not yet gating: rustfmt disagrees with the tree in ~220 places (mostly
# edition-2024 import ordering) and clippy has ~42 warnings. Both are worth
# fixing, and once they are, move these above and drop the flag.

if [ "$STRICT" = "1" ]; then
  for ws in $WORKSPACES; do
    step "fmt: $ws"
    (cd "$ws" && cargo fmt --check)
    step "clippy: $ws"
    (cd "$ws" && cargo clippy --all-targets -- -D warnings)
  done
fi

printf '\n\033[1;32mall checks passed\033[0m\n'
