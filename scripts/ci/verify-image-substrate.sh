#!/usr/bin/env bash
# Verify that a built or published AgentWechat image actually carries the
# pinned WeChat substrate and the hardened agent-server (RB-001).
#
# Usage:
#   verify-image-substrate.sh <image-ref>
#
# Optional env overrides (defaults are the RB-001 verified values):
#   EXPECTED_PLATFORM              (default linux/amd64)
#   EXPECTED_WECHAT_VERSION        (default 4.1.1.4)
#   EXPECTED_WECHAT_BINARY_SHA256  (default 921391fa...)
#   EXPECTED_BASE_DIGEST           (optional; label com.wechat-hub.upstream-base-digest)
#   EXPECTED_REVISION              (optional; label org.opencontainers.image.revision)
#
# Runtime checks run in a transient `docker run --rm --network none` container
# (auto-removed, no volumes, no network, no host ports). Exit 0 = verified.

set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 <image-ref>" >&2; exit 2; }
IMAGE_REF="$1"

EXPECTED_PLATFORM="${EXPECTED_PLATFORM:-linux/amd64}"
EXPECTED_WECHAT_VERSION="${EXPECTED_WECHAT_VERSION:-4.1.1.4}"
EXPECTED_WECHAT_BINARY_SHA256="${EXPECTED_WECHAT_BINARY_SHA256:-921391facbec202f2f1487e8a3c8e7790d037e4dc966f65901064f3ab4c4ea54}"

command -v docker >/dev/null 2>&1 || { echo "FATAL(RB-001): docker not available" >&2; exit 2; }

fail() {
  echo "FATAL(RB-001): $1" >&2
  exit 1
}

# --- platform ---------------------------------------------------------------
PLATFORM="$(docker image inspect "$IMAGE_REF" --format '{{.Os}}/{{.Architecture}}')"
echo "platform: ${PLATFORM}"
[ "$PLATFORM" = "$EXPECTED_PLATFORM" ] || fail "platform mismatch: expected ${EXPECTED_PLATFORM}, got ${PLATFORM}"

# --- labels -----------------------------------------------------------------
LABEL_JSON="$(docker image inspect "$IMAGE_REF" --format '{{json .Config.Labels}}')"
echo "labels: ${LABEL_JSON}"
if [ -n "${EXPECTED_REVISION:-}" ]; then
  REVISION="$(printf '%s' "$LABEL_JSON" | sed -n 's/.*"org.opencontainers.image.revision":"\([0-9a-f]\{40\}\)".*/\1/p')"
  [ "$REVISION" = "$EXPECTED_REVISION" ] || fail "OCI revision label mismatch: expected ${EXPECTED_REVISION}, got '${REVISION}'"
fi
if [ -n "${EXPECTED_BASE_DIGEST:-}" ]; then
  BASE_DIGEST="$(printf '%s' "$LABEL_JSON" | sed -n 's/.*"com.wechat-hub.upstream-base-digest":"\(sha256:[0-9a-f]\{64\}\)".*/\1/p')"
  [ "$BASE_DIGEST" = "$EXPECTED_BASE_DIGEST" ] || fail "upstream-base-digest label mismatch: expected ${EXPECTED_BASE_DIGEST}, got '${BASE_DIGEST}'"
fi

# --- in-image substrate + hardened binary (transient container) -------------
# The dpkg format strings are passed via env so no shell layer can expand
# the ${Version}/${Status} fields before dpkg-query sees them.
docker run --rm --network none --entrypoint /bin/sh \
  -e EXPECTED_WECHAT_VERSION="$EXPECTED_WECHAT_VERSION" \
  -e EXPECTED_WECHAT_BINARY_SHA256="$EXPECTED_WECHAT_BINARY_SHA256" \
  -e DPKG_VERSION_FMT='${Version}' \
  -e DPKG_STATUS_FMT='${Status}' \
  "$IMAGE_REF" -c '
    set -eu
    V="$(dpkg-query -W -f="$DPKG_VERSION_FMT" wechat)"
    S="$(dpkg-query -W -f="$DPKG_STATUS_FMT" wechat)"
    echo "wechat package: ${V} (${S})"
    [ "$V" = "$EXPECTED_WECHAT_VERSION" ] || { echo "FATAL: wechat version mismatch: expected ${EXPECTED_WECHAT_VERSION}, got ${V}" >&2; exit 1; }
    [ "$S" = "install ok installed" ] || { echo "FATAL: wechat package status unexpected: ${S}" >&2; exit 1; }
    H="$(sha256sum /opt/wechat/wechat | cut -d " " -f1)"
    echo "/opt/wechat/wechat sha256: ${H}"
    [ "$H" = "$EXPECTED_WECHAT_BINARY_SHA256" ] || { echo "FATAL: wechat binary hash mismatch" >&2; exit 1; }
    test -x /opt/agent-server/agent-server || { echo "FATAL: /opt/agent-server/agent-server missing or not executable" >&2; exit 1; }
    MARKERS="$(grep -ac "Enter Weixin" /opt/agent-server/agent-server)"
    echo "hardened binary marker hits (Enter Weixin): ${MARKERS}"
    [ "${MARKERS:-0}" -ge 1 ] || { echo "FATAL: hardened agent-server markers missing (upstream binary not replaced?)" >&2; exit 1; }
    test -x /entrypoint.sh || { echo "FATAL: /entrypoint.sh missing" >&2; exit 1; }
    id wechat >/dev/null 2>&1 || { echo "FATAL: wechat user missing" >&2; exit 1; }
    test -d /opt/novnc || { echo "FATAL: /opt/novnc missing" >&2; exit 1; }
    command -v sqlcipher >/dev/null 2>&1 || { echo "FATAL: sqlcipher missing" >&2; exit 1; }
    echo "in-image substrate checks: OK"
  '

echo "OK(RB-001): image substrate verified: ${IMAGE_REF}"
