#!/usr/bin/env bash
#
# Build `meadow-theme-<version>.vsix`.
#
# A theme is JSON and has no dependencies of its own, so this borrows the `vsce`
# pinned in `../vscode` rather than keeping a second `package-lock.json` whose
# only entry would be the packager. See `../vscode/build.sh` for why it is
# pinned at all.

set -euo pipefail
cd "$(dirname "$0")"

(cd ../vscode && npm install --silent)

# As in `../vscode/build.sh`: `release.yml` uploads every `.vsix` it finds here.
rm -f ./*.vsix

node ../vscode/node_modules/@vscode/vsce/vsce package --allow-missing-repository --skip-license

echo
ls -1 ./*.vsix
