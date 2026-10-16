#!/bin/sh
# Builds Meadow from source and installs it, on macOS and Linux.
#
#   scripts/install.sh
#
# This is for working on Meadow, or for a machine with no release build. To
# install a release, use scripts/meadowup-init.sh instead:
#
#   curl -fsSL https://raw.githubusercontent.com/meadow-lang/meadow/master/scripts/meadowup-init.sh | sh
#
# Builds the whole toolchain from source -- `meadow` (the compiler, the build
# system and the language server) and `meadowup`, which looks after which
# version of Meadow you have -- and installs both. It needs a Rust toolchain and
# takes a few minutes.
#
# The source is the checkout this script is in, when it is in one. Otherwise --
# piped from curl, or asked for a `--version` -- it is cloned into
# ~/.meadow/src, and that is all that is downloaded. `--local <path>` builds a
# checkout somewhere else.
#
# What ends up on disk is exactly what `meadowup install` would have put there:
# the binaries in ~/.meadow/bin, and your PATH edited to find them. That is
# because meadowup does the installing -- the one just built is handed the
# binaries beside it rather than fetching any -- so there is one implementation
# of where things go, whichever way they were made. And since meadowup is
# installed too, `meadowup update` moves a source build on to the next release
# like any other install.
#
# Working on Meadow itself? `--with-extension` builds and installs the VS Code
# extension alongside it.
#
# Run with --help for the options.

set -eu

REPO="meadow-lang/meadow"
MEADOW_HOME="${MEADOW_HOME:-$HOME/.meadow}"
BIN_DIR="$MEADOW_HOME/bin"

VERSION="latest"
LOCAL=""
UNINSTALL=0
EXTENSION=0
# Passed through to meadowup, which is what actually edits the PATH.
PASS_THROUGH=""

main() {
    parse_args "$@"

    # Uninstalling needs meadowup and nothing else, so do not build a toolchain
    # only to delete it.
    if [ "$UNINSTALL" -eq 1 ]; then
        [ -x "$BIN_DIR/meadowup" ] || err "meadow is not installed at $MEADOW_HOME"
        exec "$BIN_DIR/meadowup" uninstall $PASS_THROUGH
    fi

    build_everything
}

# --- argument parsing -------------------------------------------------------

parse_args() {
    while [ $# -gt 0 ]; do
        case "$1" in
            --version)
                [ $# -ge 2 ] || err "--version needs a tag, e.g. --version v0.2.0"
                VERSION="$2"
                shift 2
                ;;
            --local)
                [ $# -ge 2 ] || err "--local needs the path to a meadow checkout"
                LOCAL="$2"
                shift 2
                ;;
            # Building from source is all this does now; the flag that used to
            # ask for it is still accepted, so a script that passes it keeps
            # working.
            --from-source) shift ;;
            --from-release)
                err "this script only builds from source. To install a release:
       curl -fsSL https://raw.githubusercontent.com/$REPO/master/scripts/meadowup-init.sh | sh"
                ;;
            --with-extension) EXTENSION=1; shift ;;
            --no-modify-path) PASS_THROUGH="$PASS_THROUGH --no-modify-path"; shift ;;
            # Meaningless for a build, which always installs; accepted so that
            # a script passing it keeps working.
            --force) shift ;;
            --uninstall) UNINSTALL=1; shift ;;
            --help|-h) usage; exit 0 ;;
            *) err "unknown option: $1 (try --help)" ;;
        esac
    done
}

usage() {
    cat <<'EOF'
Builds the Meadow toolchain from source and installs it.

    scripts/install.sh [options]

    --version <TAG>    fetch and build that tag, rather than this checkout
    --local <PATH>     build the checkout at PATH, rather than this one
    --with-extension   also build and install the VS Code extension and theme
    --no-modify-path   do not touch your shell profiles
    --uninstall        remove Meadow and undo the PATH entry

Both `meadow` and `meadowup` are built and installed, laid out exactly as
`meadowup install` would lay them out. Building needs a Rust toolchain and
takes a few minutes.

Afterwards, `meadowup` looks after the installation:

    meadowup update       move on to the latest release
    meadowup show         what is installed, and where
    meadowup uninstall

MEADOW_HOME overrides where everything goes. To install a release rather
than build one, use scripts/meadowup-init.sh.
EOF
}

# --- building ---------------------------------------------------------------

# Build the whole toolchain and install it.
#
# Both binaries are built and staged in one directory, and the meadowup just
# built is told to install from there. `--from` means it fetches nothing: it only
# does what it does with a downloaded release once it has one -- put each binary
# in place, itself included, and edit the PATH.
build_everything() {
    need_cmd cargo
    src="$(toolchain_source)"

    say "building meadowup"
    cargo build --release --manifest-path "$src/installer/Cargo.toml" --bin meadowup \
        || err "could not build meadowup"

    say "building the toolchain (this takes a few minutes)"
    cargo build --release --manifest-path "$src/buildtools/Cargo.toml" -p meadow \
        || err "could not build meadow"

    built="$src/installer/target/release/meadowup"

    # A meadowup older than this script would not know `--from`, which happens
    # when the two come from different commits -- an old `--version`, say. Say
    # that, rather than letting it fail on an option it has never heard of.
    if ! "$built" help 2>/dev/null | grep -q -- "--from"; then
        err "the meadowup built from $src predates installing from a directory,
       so this script cannot install it. Build a newer version, or a checkout
       of this one:  scripts/install.sh --local <path-to-this-checkout>"
    fi

    staging="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf \"$staging\"" EXIT
    cp "$built" "$staging/meadowup"
    cp "$src/buildtools/target/release/meadow" "$staging/meadow"

    # Always installed, even over a release with the same version number:
    # `--from` never skips what is already there, since what was just built is
    # not that release.
    "$built" install --from "$staging" $PASS_THROUGH \
        || err "could not install what was built"

    rm -rf "$staging"
    trap - EXIT

    [ "$EXTENSION" -eq 1 ] && install_extension "$src"
    return 0
}

# Build the VS Code extensions from `$1` and install them, for someone working
# on Meadow who wants the editor to follow what they just built: the language
# extension, and the colour theme beside it.
install_extension() {
    src="$1"
    installed=0
    for dir in editors/vscode editors/vscode-theme; do
        [ -x "$src/$dir/build.sh" ] || err "$src has no $dir/build.sh"
        say "building $dir"
        (cd "$src" && "./$dir/build.sh") || err "could not build $dir"

        vsix="$(ls -1 "$src/$dir"/*.vsix 2>/dev/null | head -1)"
        [ -n "$vsix" ] || err "$dir built but produced no .vsix"

        if command -v code >/dev/null 2>&1; then
            say "installing $(basename "$vsix")"
            # `--force` because the version usually has not changed between
            # two local builds, and without it VS Code declines to reinstall it.
            code --install-extension "$vsix" --force >/dev/null \
                || err "could not install $(basename "$vsix")"
            installed=1
        else
            say ""
            say "\`code\` is not on PATH, so it was built but not installed:"
            say "  code --install-extension $vsix --force"
        fi
    done

    if [ "$installed" -eq 1 ]; then
        say ""
        say "Reload the editor window (Developer: Reload Window) so the language"
        say "server restarts on the binary just built. The theme is chosen with"
        say "Preferences: Color Theme (Meadow Dark, Meadow Light)."
    fi
}

# Where to build from: a checkout named with --local, the checkout this script
# is in, or one fetched into $MEADOW_HOME/src. Fetching into the same place each
# time means a second run is an incremental build rather than a clean one.
toolchain_source() {
    if [ -n "$LOCAL" ]; then
        [ -f "$LOCAL/installer/Cargo.toml" ] \
            || err "$LOCAL does not look like a meadow checkout (no installer/)"
        echo "$LOCAL"
        return
    fi

    # Run from inside a checkout, build that checkout: "from source" should mean
    # the source in front of you, not a fresh clone of master. It also keeps this
    # script and the meadowup it builds at the same version, which they must be
    # -- one passes options the other has to understand.
    #
    # `[ -f "$0" ]` is what tells a checkout apart from `curl … | sh`, where the
    # script is not a file on disk at all and the directory above it means
    # nothing.
    if [ "$VERSION" = "latest" ] && [ -f "$0" ]; then
        here="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)" || here=""
        if [ -n "$here" ] && [ -f "$here/installer/Cargo.toml" ] \
            && [ -f "$here/buildtools/Cargo.toml" ]; then
            say "building this checkout: $here" >&2
            echo "$here"
            return
        fi
    fi

    need_cmd git
    src="$MEADOW_HOME/src"
    if [ "$VERSION" = "latest" ]; then
        ref="HEAD"
        say "fetching the latest source into $src" >&2
    else
        ref="$VERSION"
        say "fetching $VERSION into $src" >&2
    fi
    if [ -d "$src/.git" ]; then
        # The ref asked for, not whatever the clone last had: a second run with
        # a different `--version` must build that version.
        git -C "$src" fetch --depth 1 origin "$ref" >/dev/null 2>&1 \
            || err "git fetch failed (no such tag: $VERSION?)"
        git -C "$src" reset --hard FETCH_HEAD >/dev/null 2>&1 || err "git reset failed"
    else
        rm -rf "$src"
        if [ "$VERSION" = "latest" ]; then
            git clone --depth 1 "https://github.com/$REPO.git" "$src" >/dev/null 2>&1 \
                || err "git clone failed"
        else
            git clone --depth 1 --branch "$VERSION" "https://github.com/$REPO.git" "$src" \
                >/dev/null 2>&1 || err "git clone failed (no such tag: $VERSION?)"
        fi
    fi
    echo "$src"
}

# --- helpers ----------------------------------------------------------------

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || err "need $1"
}

say() {
    echo "$1"
}

err() {
    echo "error: $1" >&2
    exit 1
}

main "$@"
