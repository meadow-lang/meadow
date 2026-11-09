#!/usr/bin/env bash
# MeadowBoot builds itself, and what it builds builds the same thing again.
#
#   scripts/meadowboot-selfbuild.sh MEADOWBOOT MEADOW [WORK]
#
# MEADOWBOOT is stage 1: MeadowBoot as the Rust compiler built it,
# `bootstrap/target/release/native/silo/MeadowBoot` after `meadow build
# --release bootstrap`, which also fetches what it depends on. MEADOW is the
# `meadow` that hands MeadowBoot's Cut to the back end.
#
# Stage 1 compiles `bootstrap/` to Cut, and Silo makes that native: stage 2,
# MeadowBoot compiled by itself. Stage 2 compiles `bootstrap/` again, and its
# Cut must be the one it was made from, byte for byte -- a compiler that
# reproduces itself, so stage 3 is stage 2. Then stage 2 runs a program and
# builds a library, as the release's smoke test does.
#
# A miscompilation in MeadowBoot's own passes, or in the runtime it runs on,
# shows here when nothing else does: stage 2 is the largest program MeadowBoot
# compiles, and the only one that compiles a compiler. Stage 2 overflowing its
# stack on Windows was seen this way and no other.
#
# Each stage is timed, with its peak memory where `/usr/bin/time` says it.
set -euo pipefail

stage1=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
export MEADOW=$(cd "$(dirname "$2")" && pwd)/$(basename "$2")
root=$(cd "$(dirname "$0")/.." && pwd)
work=${3:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/meadowboot-selfbuild}
export MEADOWBOOT_STD="$root/lib/Std"
exe=""
case "$(uname -s)" in MINGW* | MSYS* | CYGWIN*) exe=".exe" ;; esac

rm -rf "$work"
mkdir -p "$work"

# A fresh copy of MeadowBoot's sources, for stage `$1` to compile.
fresh() {
  mkdir -p "$work/$1/MeadowBoot"
  cp -r "$root/bootstrap/src" "$root/bootstrap/Meadow.toml" "$root/bootstrap/Meadow.lock" \
    "$work/$1/MeadowBoot/"
}
cut_of() { echo "$work/$1/MeadowBoot/target/release/link/program.cut"; }
exe_of() { echo "$work/$1/MeadowBoot/target/release/link/target/release/native/silo/program$exe"; }

# Stage `$2` compiles MeadowBoot into `$work/$1`, natively on Silo.
build() {
  fresh "$1"
  local start=$SECONDS
  local timed=()
  if /usr/bin/time -f "" true 2> /dev/null; then
    timed=(/usr/bin/time -f "  peak %M KB")
  elif [ "$(uname -s)" = Darwin ]; then
    timed=(/usr/bin/time -l)
  fi
  echo "== stage $2 builds MeadowBoot"
  if ! ${timed[@]+"${timed[@]}"} "$3" build --release --runtime silo --offline "$work/$1/MeadowBoot" \
    > "$work/$1.log" 2>&1; then
    tail -n 40 "$work/$1.log"
    echo "stage $2 failed to build MeadowBoot"
    exit 1
  fi
  grep -E "peak|maximum resident" "$work/$1.log" || true
  echo "  $((SECONDS - start)) s"
}

build stage2 1 "$stage1"
build stage3 2 "$(exe_of stage2)"

a=$(cut_of stage2)
b=$(cut_of stage3)
if ! cmp -s "$a" "$b"; then
  echo "stage 2 does not reproduce itself: its Cut differs from the Cut it was made from"
  cmp "$a" "$b" || true
  exit 1
fi
echo "stage 2 reproduces itself: $(wc -c < "$a") bytes of Cut, identical"
if cmp -s "$(exe_of stage2)" "$(exe_of stage3)"; then
  echo "and stage 3's executable is stage 2's"
fi

# The smoke test, by stage 2: a program runs, and a package that exports to C
# builds to a library named for it.
s2=$(exe_of stage2)
smoke="$work/smoke"
mkdir -p "$smoke/Hello/src" "$smoke/Lib/src"
printf '[package]\nname = "Hello"\nversion = "0.1.0"\n' > "$smoke/Hello/Meadow.toml"
printf 'use Std.Num.Float as F\n\nfun main () = println (show (F.round (F.sqrt 2.0 * 100.0)))\n' \
  > "$smoke/Hello/src/Main.mw"
printf '[package]\nname = "Lib"\nversion = "0.1.0"\n' > "$smoke/Lib/Meadow.toml"
printf '@export\nfun twice (x : Int) : Int = x * 2\n' > "$smoke/Lib/src/Main.mw"
cd "$smoke"
"$s2" run Hello | tail -n 1 | grep -qx '141'
echo "stage 2 runs a program"
"$s2" build Lib > /dev/null
ls Lib/target/debug/native/silo/ | grep -E '^(Lib\.dll|libLib\.so|libLib\.dylib)$'
echo "stage 2 builds a library"
