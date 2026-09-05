#!/usr/bin/env bash
# Verify that the checked-out HEAD is exactly the requested source commit.
#
# Usage:
#   verify-source-commit.sh <expected-commit>
#
# Guards the publish pipeline against publishing anything other than the
# explicitly requested release source: branch names and short SHAs are
# rejected, and HEAD must resolve to the exact commit.
#
# Exit 0 = HEAD matches, 1 = mismatch or invalid input.

set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <expected-commit>" >&2
  exit 1
fi

EXPECTED="$1"

if ! [[ "$EXPECTED" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "ERROR: expected a full 40-char commit SHA, got: '$EXPECTED'" >&2
  exit 1
fi

EXPECTED_LOWER="$(printf '%s' "$EXPECTED" | tr 'A-Z' 'a-z')"
ACTUAL="$(git rev-parse 'HEAD^{commit}')"

if [ "$ACTUAL" != "$EXPECTED_LOWER" ]; then
  echo "ERROR: source commit mismatch: checked-out HEAD is $ACTUAL, requested $EXPECTED_LOWER" >&2
  exit 1
fi

echo "source commit verified: $ACTUAL"
