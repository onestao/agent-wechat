#!/usr/bin/env bash
# RB-001 fail-closed static verification: the AgentWechat release image must
# build on the digest-pinned upstream runtime substrate (WeChat 4.1.1.4) and
# must never download WeChat at build time.
#
# Usage:
#   verify-pinned-runtime.sh                    # full contract check (CI/preflight)
#   verify-pinned-runtime.sh --print-base-digest
#                                               # print the pinned base digest
#                                               # (used by publish workflow labels)
#
# Contract enforced (against docker/Dockerfile and the build-relevant tree):
#   C1  The final runtime stage is FROM the exact pinned upstream digest
#       (never a mutable tag, never ubuntu + .deb download).
#   C2  No unversioned WeChat download input exists anywhere in the build
#       path (dldir1v6.qq.com / WeChatLinux_*.deb / local wechat.deb COPY).
#   C3  In-build substrate assertions exist and pin the verified WeChat
#       version + /opt/wechat/wechat binary sha256 (regression guard against
#       silent WeChat runtime upgrades).
#   C4  The hardened agent-server built from source replaces the upstream
#       binary (multi-stage COPY --from=builder).
#   C5  The publish pipeline actually runs this verifier.
#
# Exit 0 = contract holds; 1 = contract violated (fail-closed).

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
DOCKERFILE="$REPO_ROOT/docker/Dockerfile"

# Verified working substrate (RB-001) — see docs/FLASH_A_WH3_PINNED_RUNTIME_REPORT.md
EXPECTED_BASE_IMAGE="ghcr.io/thisnick/agent-wechat@sha256:31a4e351c191bcbfc75e5c10be51e207d22a3eedd97f3ff56ad579fcce717b24"
EXPECTED_WECHAT_VERSION="4.1.1.4"
EXPECTED_WECHAT_BINARY_SHA256="921391facbec202f2f1487e8a3c8e7790d037e4dc966f65901064f3ab4c4ea54"

fail() {
  echo "FATAL(RB-001): $1" >&2
  exit 1
}

if [ "${1:-}" = "--print-base-digest" ]; then
  [ -f "$DOCKERFILE" ] || fail "docker/Dockerfile missing"
  DIGEST="$(grep -E '^FROM ghcr\.io/thisnick/agent-wechat@sha256:[0-9a-f]{64}$' "$DOCKERFILE" | tail -1 | sed 's/^FROM .*@//')"
  [ -n "$DIGEST" ] || fail "runtime stage does not pin the upstream base by digest; cannot print base digest"
  echo "$DIGEST"
  exit 0
fi

[ "${1:-}" = "" ] || { echo "usage: $0 [--print-base-digest]" >&2; exit 2; }

[ -f "$DOCKERFILE" ] || fail "docker/Dockerfile missing"

# --- C1: runtime stage FROM pins the exact digest -------------------------
FROM_LINE="$(grep -E '^FROM ghcr\.io/thisnick/agent-wechat@' "$DOCKERFILE" | tail -1 || true)"
[ -n "$FROM_LINE" ] || fail "no runtime stage FROM of ghcr.io/thisnick/agent-wechat found in docker/Dockerfile"
EXPECTED_FROM_LINE="FROM ${EXPECTED_BASE_IMAGE}"
[ "$FROM_LINE" = "$EXPECTED_FROM_LINE" ] \
  || fail "runtime stage FROM does not match the verified substrate: expected '${EXPECTED_FROM_LINE}', got '${FROM_LINE}'"

# A tag-based base reference of the upstream image is never acceptable.
if grep -Eq '^FROM ghcr\.io/thisnick/agent-wechat:[^@]' "$DOCKERFILE"; then
  fail "tag-based base image reference found; the substrate must be pinned by digest"
fi

# --- C2: no unversioned WeChat download input in the build path ------------
# (this script is excluded from its own scan: it must contain the forbidden
# pattern as its grep needle)
SCAN_FILES="$(find "$REPO_ROOT/docker" "$REPO_ROOT/scripts" "$REPO_ROOT/.github/workflows" \
  -type f \( -name '*.sh' -o -name '*.yml' -o -name '*.yaml' -o -name 'Dockerfile' -o -name '.dockerignore' \) 2>/dev/null \
  | grep -v "$REPO_ROOT/scripts/ci/verify-pinned-runtime.sh" || true)"
if [ -f "$REPO_ROOT/package.json" ]; then
  SCAN_FILES="$SCAN_FILES
$REPO_ROOT/package.json"
fi
URL_HITS="$(printf '%s\n' "$SCAN_FILES" | xargs -r grep -InE 'dldir1v6\.qq\.com|WeChatLinux' 2>/dev/null || true)"
if [ -n "$URL_HITS" ]; then
  echo "$URL_HITS" >&2
  fail "unversioned WeChat download URL/path found in the build tree (see hits above)"
fi
if grep -Eq 'wechat\.deb|dpkg -i|COPY wechat\.de' "$DOCKERFILE"; then
  fail "docker/Dockerfile still handles a WeChat .deb build input; the substrate must come from the pinned base image only"
fi
if [ -f "$REPO_ROOT/scripts/download-wechat.sh" ]; then
  fail "scripts/download-wechat.sh exists; the unversioned WeChat .deb download helper must not exist"
fi

# --- C3: in-build substrate assertions pin version + binary hash -----------
grep -Fq "dpkg-query -W -f='\${Version}' wechat" "$DOCKERFILE" \
  || fail "in-build WeChat package version assertion missing from docker/Dockerfile"
grep -Fq "\"${EXPECTED_WECHAT_VERSION}\"" "$DOCKERFILE" \
  || fail "expected WeChat version constant ${EXPECTED_WECHAT_VERSION} not asserted in docker/Dockerfile"
grep -Fq "sha256sum /opt/wechat/wechat" "$DOCKERFILE" \
  || fail "in-build WeChat binary hash assertion missing from docker/Dockerfile"
grep -Fq "${EXPECTED_WECHAT_BINARY_SHA256}" "$DOCKERFILE" \
  || fail "expected WeChat binary sha256 constant not asserted in docker/Dockerfile"

# --- C4: hardened agent-server replaces the upstream binary ----------------
grep -Fq 'COPY --from=builder /output/agent-server /opt/agent-server/agent-server' "$DOCKERFILE" \
  || fail "multi-stage hardened agent-server replacement missing from docker/Dockerfile"

# --- C5: publish pipeline runs this verifier -------------------------------
PUBLISH_WF="$REPO_ROOT/.github/workflows/publish-agent-wechat.yml"
[ -f "$PUBLISH_WF" ] || fail "publish workflow missing"
grep -Fq 'scripts/ci/verify-pinned-runtime.sh' "$PUBLISH_WF" \
  || fail "publish workflow does not run verify-pinned-runtime.sh"

echo "OK(RB-001): pinned runtime substrate contract verified"
echo "  base image   : ${EXPECTED_BASE_IMAGE}"
echo "  wechat       : ${EXPECTED_WECHAT_VERSION} (dpkg), binary sha256 ${EXPECTED_WECHAT_BINARY_SHA256}"
