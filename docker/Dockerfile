# ============================================
# WeChat Hub AgentWechat — pinned-runtime release image (RB-001 remediation)
#
# Reproducibility contract (enforced by scripts/ci/verify-pinned-runtime.sh):
#   Stage 1 compiles the hardened agent-server from the exact source in this
#   repo (Cargo.lock-pinned, multi-stage).
#   Stage 2 starts FROM a digest-pinned upstream runtime image carrying the
#   verified WeChat GUI substrate (WeChat 4.1.1.4 + X11/AT-SPI/noVNC stack).
#   The WeChat .deb is NEVER downloaded at build time: the unversioned QQ
#   download URL is forbidden in this recipe and anywhere else in the build
#   path (enforced literally by scripts/ci/verify-pinned-runtime.sh).
#
# Verified working substrate (RB-001):
#   ghcr.io/thisnick/agent-wechat:0.11.15
#   = sha256:31a4e351c191bcbfc75e5c10be51e207d22a3eedd97f3ff56ad579fcce717b24
#   WeChat package: wechat 4.1.1.4 (dpkg "install ok installed")
#   /opt/wechat/wechat sha256:
#     921391facbec202f2f1487e8a3c8e7790d037e4dc966f65901064f3ab4c4ea54
# Do NOT change the base to a mutable tag or to `ubuntu` + .deb download.
# A substrate bump means changing the FROM digest AND the expected
# version/hash constants below in the same reviewed commit.
# ============================================

# ============================================
# Stage 1: Build the Rust server binary
# ============================================
FROM rust:1.93-bookworm AS builder

ARG BUILD_MODE=release

WORKDIR /build

# Install build dependencies for bundled sqlcipher + openssl
RUN apt-get update && apt-get install -y \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first for dependency caching
COPY agent-server-rust/Cargo.toml agent-server-rust/Cargo.lock* ./
# Create dummy src for dependency build
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs
RUN if [ "$BUILD_MODE" = "debug" ]; then cargo build 2>/dev/null || true; else cargo build --release 2>/dev/null || true; fi
# Remove dummy build artifacts
RUN rm -rf src target/release/deps/agent_server* target/debug/deps/agent_server*

# Copy actual source and migrations, then build
COPY agent-server-rust/src ./src
COPY agent-server-rust/migrations ./migrations
RUN if [ "$BUILD_MODE" = "debug" ]; then cargo build; else cargo build --release; fi

# Copy binary to a known location regardless of build mode
RUN mkdir -p /output && cp target/${BUILD_MODE}/agent-server /output/agent-server

# ============================================
# Stage 2: Final runtime image — digest-pinned upstream substrate
# ============================================
FROM ghcr.io/thisnick/agent-wechat@sha256:31a4e351c191bcbfc75e5c10be51e207d22a3eedd97f3ff56ad579fcce717b24

# ============================================
# RB-001 fail-closed substrate assertions.
# The build FAILS unless the base image carries exactly the verified WeChat
# 4.1.1.4 runtime. This is the regression guard against silent WeChat
# runtime upgrades: any change of the base image content (rebuild of the
# substrate with a newer WeChat, tag-based base, .deb download) breaks the
# version check, the binary hash check, or both.
# ============================================
RUN set -eu; \
    BASE_WECHAT_VERSION="$(dpkg-query -W -f='${Version}' wechat)"; \
    BASE_WECHAT_STATUS="$(dpkg-query -W -f='${Status}' wechat)"; \
    echo "substrate wechat package: ${BASE_WECHAT_VERSION} (${BASE_WECHAT_STATUS})"; \
    if [ "${BASE_WECHAT_VERSION}" != "4.1.1.4" ]; then \
      echo "FATAL(RB-001): WeChat substrate version drift: expected '4.1.1.4', got '${BASE_WECHAT_VERSION}'" >&2; \
      exit 1; \
    fi; \
    if [ "${BASE_WECHAT_STATUS}" != "install ok installed" ]; then \
      echo "FATAL(RB-001): WeChat substrate package status unexpected: '${BASE_WECHAT_STATUS}'" >&2; \
      exit 1; \
    fi; \
    BASE_WECHAT_SHA256="$(sha256sum /opt/wechat/wechat | awk '{print $1}')"; \
    echo "substrate /opt/wechat/wechat sha256: ${BASE_WECHAT_SHA256}"; \
    if [ "${BASE_WECHAT_SHA256}" != "921391facbec202f2f1487e8a3c8e7790d037e4dc966f65901064f3ab4c4ea54" ]; then \
      echo "FATAL(RB-001): WeChat binary hash drift: expected '921391facbec202f2f1487e8a3c8e7790d037e4dc966f65901064f3ab4c4ea54', got '${BASE_WECHAT_SHA256}'" >&2; \
      exit 1; \
    fi

# ============================================
# OCI labels for the pinned substrate (constants of this recipe).
# Dynamic labels (revision/version/created/source-commit/upstream base
# commit+tag) are applied by the publish workflow on top of these.
# ============================================
LABEL org.opencontainers.image.base.name="ghcr.io/thisnick/agent-wechat@sha256:31a4e351c191bcbfc75e5c10be51e207d22a3eedd97f3ff56ad579fcce717b24" \
      com.wechat-hub.upstream-base-image="ghcr.io/thisnick/agent-wechat:0.11.15" \
      com.wechat-hub.upstream-base-digest="sha256:31a4e351c191bcbfc75e5c10be51e207d22a3eedd97f3ff56ad579fcce717b24" \
      com.wechat-hub.wechat-version="4.1.1.4" \
      com.wechat-hub.wechat-binary-sha256="921391facbec202f2f1487e8a3c8e7790d037e4dc966f65901064f3ab4c4ea54"

# ============================================
# Hardened agent-server built from this exact source (Stage 1)
# ============================================
COPY --from=builder /output/agent-server /opt/agent-server/agent-server

# ============================================
# Version-controlled runtime scripts (tools + entrypoint).
# Byte-identical to the upstream substrate today; copied so the image always
# matches this repo even if the base image is re-assembled upstream.
# ============================================
COPY tools/ /opt/tools/
RUN chmod +x /opt/tools/*

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# ============================================
# Debug support (gdbserver for remote debugging)
# ============================================
ARG BUILD_MODE=release
RUN if [ "$BUILD_MODE" = "debug" ]; then \
      apt-get update && apt-get install -y gdbserver && rm -rf /var/lib/apt/lists/*; \
    fi

# ============================================
# Environment (inherited from the substrate; re-declared as contract)
# ============================================
ENV DISPLAY=:99
ENV PATH="/opt/tools:$PATH"
ENV AGENT_DB_PATH=/data/agent.db
ENV AGENT_PORT=6174

EXPOSE 6174 1234

ENTRYPOINT ["/entrypoint.sh"]
