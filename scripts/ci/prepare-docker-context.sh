#!/usr/bin/env bash
# Assemble the docker build context consumed by docker/Dockerfile.
#
# The publish workflow (publish-agent-wechat.yml) builds the AgentWechat image
# from this version-controlled recipe — never from an ad-hoc NAS-local
# Dockerfile that copies a prebuilt binary.
#
# Context layout expected by docker/Dockerfile:
#   docker/agent-server-rust/Cargo.toml
#   docker/agent-server-rust/Cargo.lock
#   docker/agent-server-rust/src/
#   docker/agent-server-rust/migrations/
#   docker/tools/, docker/entrypoint.sh  (already in the repo)
#
# Run from anywhere; cd's to the repository root.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO_ROOT"

SRC="packages/agent-server-rust"
DST="docker/agent-server-rust"

for required in "$SRC/Cargo.toml" "$SRC/Cargo.lock" "$SRC/src" "$SRC/migrations" docker/Dockerfile docker/entrypoint.sh; do
  if [ ! -e "$required" ]; then
    echo "ERROR: required build input missing: $required" >&2
    exit 1
  fi
done

rm -rf "$DST"
mkdir -p "$DST"
cp "$SRC/Cargo.toml" "$DST/"
cp "$SRC/Cargo.lock" "$DST/"
cp -r "$SRC/src" "$DST/"
cp -r "$SRC/migrations" "$DST/"

echo "docker build context prepared at $DST (from $SRC)"
