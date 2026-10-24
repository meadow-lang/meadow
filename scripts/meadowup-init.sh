#!/bin/sh
# Installs Meadow on macOS, Linux and Android (in Termux), from a release.
#
#   curl -fsSL https://raw.githubusercontent.com/meadow-lang/meadow/master/scripts/meadowup-init.sh | sh
#
# All this does is download `meadowup` for this machine and run
# `meadowup install`. meadowup does the rest: it copies itself into
# ~/.meadow/bin, fetches the toolchain, and adds that directory to your PATH.
# It is the same program the Windows `.exe` is, and the one that later runs
# `meadowup update`. Like `rustup-init`, this installer is just the tool itself.
#
# Nothing is built. To build from source, use `scripts/install.sh` from a
# checkout.
#
# Options go to `meadowup install`: with `sh -s -- <options>` after a pipe,
#
#   curl -fsSL …/meadowup-init.sh | sh -s -- --version v0.2.0
#
# Run with --help for the list.
#
# Where `GITHUB_TOKEN` is set, as a CI job can set it to `${{ github.token }}`,
# the requests to GitHub carry it -- here and in meadowup -- and are held to
# that token's rate limit rather than to the one every unnamed request from
# the runner's address shares, which a busy CI provider's runners exhaust.

set -eu

REPO="meadow-lang/meadow"
VERSION="latest"
PASS_THROUGH=""

main() {
    parse_args "$@"

    target="$(detect_target)"
    asset="meadowup-$target"
    if [ "$VERSION" = "latest" ]; then
        url="https://github.com/$REPO/releases/latest/download/$asset"
    else
        url="https://github.com/$REPO/releases/download/$VERSION/$asset"
        PASS_THROUGH="--version $VERSION $PASS_THROUGH"
    fi

    tmp="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf \"$tmp\"" EXIT

    say "downloading meadowup for $target"
    download "$url" "$tmp/meadowup" || err "could not download $url
       There may be no release build for $target, or no release $VERSION.
       To build from source instead, clone the repository and run
       scripts/install.sh from it."
    chmod 755 "$tmp/meadowup"

    # Not `exec`: the trap has to run afterwards to remove the download, which
    # meadowup has already copied into place by then.
    # shellcheck disable=SC2086
    "$tmp/meadowup" install $PASS_THROUGH
}

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)
                [ $# -ge 2 ] || err "--version needs a tag, e.g. --version v0.2.0"
                VERSION="$2"
                shift 2
                ;;
            --no-modify-path) PASS_THROUGH="$PASS_THROUGH --no-modify-path"; shift ;;
            --force) PASS_THROUGH="$PASS_THROUGH --force"; shift ;;
            --help|-h) usage; exit 0 ;;
            *) err "unknown option: $1 (try --help)" ;;
        esac
    done
}

usage() {
    cat <<'EOF'
Downloads meadowup and runs `meadowup install`, which installs Meadow.

    meadowup-init.sh [options]

    --version <TAG>    a particular release, not the newest
    --no-modify-path   do not touch your shell profiles
    --force            install again even if it is already here

Afterwards, meadowup manages the installation:

    meadowup update       bring the toolchain up to date
    meadowup show         what is installed, and where
    meadowup uninstall

MEADOW_HOME overrides where everything goes. To build from source, run
scripts/install.sh from a checkout.
EOF
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"

    # Android says it is Linux, and its kernel is; its C library is not
    # glibc, and the Linux build will not run on it. Termux's `uname -o` is
    # the one that says so.
    if [ "$os" = "Linux" ] && [ "$(uname -o 2>/dev/null)" = "Android" ]; then
        case "$arch" in
            aarch64|arm64) echo "aarch64-linux-android"; return ;;
            *) err "no release build for Android on $arch; build from source with scripts/install.sh" ;;
        esac
    fi

    case "$os" in
        Darwin) os_part="apple-darwin" ;;
        Linux) os_part="unknown-linux-gnu" ;;
        *) err "no release build for $os; build from source with scripts/install.sh" ;;
    esac

    # Rosetta reports x86_64 on Apple silicon; the native build is the one
    # to take.
    if [ "$os" = "Darwin" ] && [ "$arch" = "x86_64" ] \
        && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
        arch="arm64"
    fi

    case "$arch" in
        x86_64|amd64) arch_part="x86_64" ;;
        arm64|aarch64) arch_part="aarch64" ;;
        *) err "no release build for $arch; build from source with scripts/install.sh" ;;
    esac

    echo "${arch_part}-${os_part}"
}

download() {
    if command -v curl >/dev/null 2>&1; then
        # curl sends a header given with -H to the host asked, and not on to
        # the host a release's download redirects to.
        if [ -n "${GITHUB_TOKEN:-}" ]; then
            curl --proto '=https' --tlsv1.2 -fsSL \
                -H "Authorization: Bearer $GITHUB_TOKEN" "$1" -o "$2"
        else
            curl --proto '=https' --tlsv1.2 -fsSL "$1" -o "$2"
        fi
    elif command -v wget >/dev/null 2>&1; then
        wget --https-only -q "$1" -O "$2"
    else
        err "need curl or wget"
    fi
}

say() {
    echo "$1"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
