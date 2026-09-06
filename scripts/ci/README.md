# scripts/ci — WeChat Hub publish pipeline helpers

Helpers used by `.github/workflows/publish-agent-wechat.yml`, the formal
reproducible publish pipeline for the WeChat Hub AgentWechat image.

| script | purpose |
| --- | --- |
| `verify-source-commit.sh` | Assert the checked-out HEAD is exactly the requested 40-char commit SHA. Rejects branch names / short SHAs. |
| `validate-release-tag.sh` | Whitelist-validate the release tag; anchors it to the upstream base version (e.g. base `v0.11.15` → `0.11.15-wh.<n>`); forbids mutable tags like `latest`. |
| `ensure-tag-absent.sh` | Fail-closed GHCR tag immutability check via skopeo. Exit 0 = absent, 1 = already exists, 2 = undetermined (auth/network) — both non-zero outcomes block publishing. Override the binary with `SKOPEO_BIN` for testing. |
| `prepare-docker-context.sh` | Assemble the docker build context from `packages/agent-server-rust` so `docker/Dockerfile` builds the binary from source. |
| `verify-pinned-runtime.sh` | RB-001 fail-closed static contract check: `docker/Dockerfile` must FROM the digest-pinned verified upstream substrate (WeChat 4.1.1.4), must contain in-build version/binary-hash assertions, and no unversioned WeChat download input may exist in the build tree. `--print-base-digest` emits the pinned digest for workflow labels. |
| `verify-image-substrate.sh` | RB-001 runtime verification of a built/published image: platform, labels (revision / upstream-base-digest), WeChat package version + `/opt/wechat/wechat` sha256, hardened agent-server markers — in a transient `--rm --network none` container. |

## Publishing a release (Integration agent)

The source commit being published must contain this pipeline (scripts +
`docker/Dockerfile`), i.e. build from the merge of the R1 code branch and this
R2 pipeline branch.

```bash
# 1. (Recommended) validate the pipeline first — builds the image, pushes nothing:
gh workflow run publish-agent-wechat.yml \
  --ref <publish-branch> \
  -f release_tag=0.11.15-wh.2 \
  -f source_commit=<40-char-SHA> \
  -f dry_run=true

# 2. Real publish (tag must NOT already exist on GHCR; never `latest`):
gh workflow run publish-agent-wechat.yml \
  --ref <publish-branch> \
  -f release_tag=0.11.15-wh.2 \
  -f source_commit=<40-char-SHA>
```

Defaults applied unless overridden: `upstream_base_tag=v0.11.15`,
`upstream_base_commit=3b7de890eb1fd3a16cf9cf26dbe1c20e0f88a616`.

The workflow output/summary reports: source commit, tag, image, platform,
manifest digest, and the OCI `org.opencontainers.image.revision` label.

## Failure modes (all fail-closed)

- `source_commit` not a 40-char SHA, or HEAD ≠ requested commit → abort.
- Tag already exists on GHCR → abort (never overwrite, never delete tags).
- Registry auth/network error during the absence check → abort. If this is a
  permissions problem, link the `wechat-hub-agent-wechat` package to
  `onestao/agent-wechat` — do not weaken the check.
- `cargo fmt --check` / `cargo check --locked` / `cargo test --locked` failure → abort.
- `upstream_base_commit` does not match the commit resolved from
  `upstream_base_tag` → abort.
- `verify-pinned-runtime.sh` contract violation (base not digest-pinned,
  unversioned WeChat download input, missing substrate assertions) → abort.
- Built/published image fails `verify-image-substrate.sh` (wrong platform,
  label mismatch, WeChat version/hash drift, hardened binary missing) → abort.

## Local testing of the scripts

```bash
bash -n scripts/ci/*.sh
# mock-based immutability check test (see R2 report for the harness)
SKOPEO_BIN=/path/to/mock-skopeo scripts/ci/ensure-tag-absent.sh ghcr.io/example/image:test
```
