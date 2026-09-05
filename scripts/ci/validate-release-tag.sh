#!/usr/bin/env bash
# Validate a WeChat Hub AgentWechat release image tag.
#
# Usage:
#   validate-release-tag.sh <release_tag> [upstream_base_tag]
#
# Rules:
#   1. Non-empty, <= 128 chars, docker tag charset [A-Za-z0-9][A-Za-z0-9._-]*.
#   2. Mutable convenience tags are forbidden: latest, main, master, stable,
#      dev, test, prod.
#   3. The tag must be anchored to the upstream base version so a release can
#      never be published from the wrong upstream base:
#        version = upstream_base_tag minus the leading 'v'
#        tag must match: ^<version>-wh\.[0-9]+([._-][A-Za-z0-9]+)?$
#      Example for base v0.11.15: 0.11.15-wh.2, 0.11.15-wh.2-rc1
#
# Exit 0 = valid, 1 = invalid.

set -euo pipefail

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
  echo "usage: $0 <release_tag> [upstream_base_tag]" >&2
  exit 1
fi

TAG="$1"
BASE_TAG="${2:-}"

fail() {
  echo "ERROR: invalid release tag '${TAG}': $1" >&2
  exit 1
}

[ -n "$TAG" ] || fail "empty tag"
[ "${#TAG}" -le 128 ] || fail "longer than 128 characters"
case "$TAG" in
  *[!A-Za-z0-9._-]*) fail "contains characters outside [A-Za-z0-9._-]" ;;
esac
case "$TAG" in
  [A-Za-z0-9]*) : ;;
  *) fail "must start with an alphanumeric character" ;;
esac
case "$TAG" in
  latest|main|master|stable|dev|test|prod) fail "mutable convenience tags are forbidden" ;;
esac

if [ -z "$BASE_TAG" ]; then
  echo "OK: release tag '${TAG}' (no upstream base tag supplied, charset rules only)"
  exit 0
fi

case "$BASE_TAG" in
  v[0-9]*) VERSION="${BASE_TAG#v}" ;;
  [0-9]*)  VERSION="$BASE_TAG" ;;
  *) echo "ERROR: invalid upstream base tag '${BASE_TAG}': expected v<semver>" >&2; exit 1 ;;
esac

VERSION_ESCAPED="${VERSION//./\\.}"
if ! [[ "$TAG" =~ ^${VERSION_ESCAPED}-wh\.[0-9]+([._-][A-Za-z0-9]+)?$ ]]; then
  fail "tag must be anchored to upstream base version '${VERSION}' (^${VERSION}-wh.<n>[suffix])"
fi

echo "OK: release tag '${TAG}' anchored to upstream base '${VERSION}'"
