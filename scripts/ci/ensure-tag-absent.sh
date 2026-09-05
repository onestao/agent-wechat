#!/usr/bin/env bash
# Fail-closed GHCR tag immutability check for WeChat Hub publish pipelines.
#
# Usage:
#   ensure-tag-absent.sh <image-ref> [--creds USER:TOKEN]
#
# Exit codes:
#   0  tag is absent — safe to publish
#   1  tag ALREADY EXISTS — immutable enforcement: refuse to publish/overwrite
#   2  undetermined (registry/auth/network error or bad invocation) —
#      fail-closed: refuse to publish
#
# The check deliberately fails closed: if the registry cannot be queried
# reliably, we refuse to publish instead of assuming the tag is free and
# silently overwriting an existing release.

set -euo pipefail

usage() {
  echo "usage: $0 <image-ref> [--creds USER:TOKEN]" >&2
  exit 2
}

[ "$#" -ge 1 ] && [ "$#" -le 3 ] || usage
IMAGE_REF="$1"
shift
CREDS=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --creds)
      [ "$#" -ge 2 ] || usage
      CREDS="$2"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

[ -n "$IMAGE_REF" ] || usage
case "$IMAGE_REF" in
  docker://*)
    echo "ERROR: pass the image reference without the docker:// prefix" >&2
    exit 2
    ;;
esac

SKOPEO_BIN="${SKOPEO_BIN:-skopeo}"
command -v "$SKOPEO_BIN" >/dev/null 2>&1 || {
  echo "ERROR: skopeo not found (set SKOPEO_BIN or install skopeo)" >&2
  exit 2
}

AUTH_ARGS=()
[ -n "$CREDS" ] && AUTH_ARGS=(--creds "$CREDS")

ERR_FILE="$(mktemp)"
trap 'rm -f "$ERR_FILE"' EXIT

if "$SKOPEO_BIN" inspect --raw ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} "docker://${IMAGE_REF}" >/dev/null 2>"$ERR_FILE"; then
  echo "IMMUTABILITY VIOLATION: tag already exists, refusing to publish: ${IMAGE_REF}" >&2
  echo "existing manifest:" >&2
  "$SKOPEO_BIN" inspect ${AUTH_ARGS[@]+"${AUTH_ARGS[@]}"} "docker://${IMAGE_REF}" 2>/dev/null | sed -n '1,12p' >&2 || true
  exit 1
fi

# "Inspect failed" has two meanings: the tag does not exist (OK) or we could
# not talk to the registry (must fail closed). Only explicit not-found error
# classes from the registry count as "absent".
ERR="$(tr '[:upper:]' '[:lower:]' <"$ERR_FILE")"
case "$ERR" in
  *"manifest unknown"*|*"name unknown"*|*"not found"*)
    echo "OK: tag is absent, publish may proceed: ${IMAGE_REF}" >&2
    exit 0
    ;;
  *)
    echo "FAIL-CLOSED: could not reliably determine whether ${IMAGE_REF} exists:" >&2
    printf '%s\n' "$ERR" >&2
    exit 2
    ;;
esac
