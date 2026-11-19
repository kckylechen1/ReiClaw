#!/usr/bin/env bash
# Install the mold linker from its pinned GitHub release, verified by checksum.
# apt pulls mold's dependencies from security.ubuntu.com, which repeatedly
# timed out on hosted runners; the release tarball is self-contained.
set -euo pipefail

MOLD_VERSION="2.40.4"
MOLD_SHA256_X86_64="4c999e19ffa31afa5aa429c679b665d5e2ca5a6b6832ad4b79668e8dcf3d8ec1"

if [[ "$(uname -s)-$(uname -m)" != "Linux-x86_64" ]]; then
  echo "install_mold.sh supports Linux x86_64 only (got $(uname -s)-$(uname -m))" >&2
  exit 2
fi

name="mold-${MOLD_VERSION}-x86_64-linux"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

bash "$(dirname "$0")/download_tool.sh" \
  "https://github.com/rui314/mold/releases/download/v${MOLD_VERSION}/${name}.tar.gz" \
  "$work/mold.tar.gz"
echo "${MOLD_SHA256_X86_64}  $work/mold.tar.gz" | sha256sum --check --strict
tar -xzf "$work/mold.tar.gz" -C "$work"
sudo cp -R "$work/$name/." /usr/local/
mold --version
