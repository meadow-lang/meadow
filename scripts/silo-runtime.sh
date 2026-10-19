#!/usr/bin/env bash
# Build Silo's runtime library for another system, where `meadow` finds it:
# `silo/target/<triple>/release/libmeadow_silo.a` in a checkout.
#
#   scripts/silo-runtime.sh aarch64-linux-android
#   scripts/silo-runtime.sh x86_64-linux-android
#
# A program built for that system with `--runtime silo --target <triple>`
# links against it. It must be built from the same checkout as the `meadow`
# that compiles the program: the two agree on a fingerprint of the sources.
#
# Android needs its NDK, at ANDROID_NDK_HOME (or ANDROID_NDK_ROOT,
# ANDROID_NDK_LATEST_HOME, ANDROID_NDK), and the Rust target, which this adds.
# The API level is MEADOW_ANDROID_API, 24 by default: what `meadow` links for.
set -euo pipefail

triple=${1:?usage: scripts/silo-runtime.sh <target triple>}
here=$(cd "$(dirname "$0")/.." && pwd)

case "$triple" in
  *-linux-android)
    ndk=${ANDROID_NDK_HOME:-${ANDROID_NDK_ROOT:-${ANDROID_NDK_LATEST_HOME:-${ANDROID_NDK:-}}}}
    [ -n "$ndk" ] || { echo "the Android NDK is needed: set ANDROID_NDK_HOME" >&2; exit 1; }
    api=${MEADOW_ANDROID_API:-24}
    case "$(uname -s)" in
      Linux*) host=linux-x86_64 ;;
      Darwin*) host=darwin-x86_64 ;;
      *) host=windows-x86_64 ;;
    esac
    bin="$ndk/toolchains/llvm/prebuilt/$host/bin"
    # The C the runtime's crates compile (ring's), with the NDK's compiler for
    # the API level, and its archiver. A library links nothing itself, but the
    # linker is named all the same, for anything cargo builds and runs.
    under=$(echo "$triple" | tr 'a-z-' 'A-Z_')
    lower=$(echo "$triple" | tr '-' '_')
    cc="$bin/$triple$api-clang"
    [ -e "$cc" ] || cc="$cc.cmd"
    export "CC_$lower=$cc" "AR_$lower=$bin/llvm-ar" "CARGO_TARGET_${under}_LINKER=$cc"
    ;;
esac

rustup target add "$triple" >/dev/null
cargo build --release --target "$triple" --manifest-path "$here/silo/Cargo.toml"
echo "$here/silo/target/$triple/release/libmeadow_silo.a"
