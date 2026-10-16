#!/usr/bin/env bash
#
# Does a release's tag agree with everything that carries the version?
#
#   scripts/version-check.sh v0.2.0 [ROOT]
#
# Release assets carry no version, so nothing downstream would notice a tag
# that disagrees with the manifests. Every manifest that carries the
# toolchain's version is held to it: the workspaces, Silo's runtime, the
# standard library -- whose version is the toolchain's -- and the editor
# extension; and the changelog has to have a section for it.
#
# `ROOT` is the checkout to look at, this one unless it is given.

set -euo pipefail

want="${1:?usage: version-check.sh vX.Y.Z [ROOT]}"
want="${want#v}"
cd "${2:-$(dirname "$0")/..}"

for manifest in compiler/Cargo.toml eval/Cargo.toml glade/Cargo.toml \
  silo/Cargo.toml buildtools/Cargo.toml installer/Cargo.toml \
  lib/Std/Meadow.toml; do
  got=$(grep -m1 '^version = ' "$manifest" | sed 's/.*"\(.*\)".*/\1/')
  if [ "$got" != "$want" ]; then
    echo "  $manifest says $got, tag says $want" >&2
    exit 1
  fi
  echo "  $manifest $got"
done
for manifest in editors/vscode/package.json editors/vscode-theme/package.json; do
  got=$(grep -m1 '"version"' "$manifest" | sed 's/.*: *"\(.*\)".*/\1/')
  if [ "$got" != "$want" ]; then
    echo "  $manifest says $got, tag says $want" >&2
    exit 1
  fi
  echo "  $manifest $got"
done
if ! grep -q "^## $want " CHANGELOG.md; then
  echo "  CHANGELOG.md has no section for $want" >&2
  exit 1
fi
echo "  CHANGELOG.md has $want"
