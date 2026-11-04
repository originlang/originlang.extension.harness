#!/usr/bin/env bash
# 发布 nightly：tag 就是包版本（<cargo 版本>-nightly.<yyyymmdd>.<n>），
# 每次构建一个 tag，与 release-please 的 vX.Y.Z 正式 tag 互不干扰。
# 所有输入走环境变量，便于在本地用假 gh 验证而不触碰真实发布。
set -euo pipefail

: "${GH_REPO:?GH_REPO is required}"
: "${PACKAGE:?PACKAGE is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"
: "${GITHUB_RUN_ID:?GITHUB_RUN_ID is required}"
TAG="${TAG:-$PACKAGE}"
CARGO_VERSION="${CARGO_VERSION:-}"

cd "${DIST:-dist}"

# 校验和与包一一对应：数量不对说明矩阵少了一个平台，宁可失败也不要发半套
shopt -s nullglob
checksums=(*.sha256)
[[ ${#checksums[@]} -eq 2 ]] || { echo "expected 2 checksums, got ${#checksums[@]}" >&2; exit 1; }
sha256sum --check "${checksums[@]}"

notes=$(mktemp)
trap 'rm -f "$notes" || true' EXIT
cat > "$notes" <<EOF
Development build from feature/dev.

Package version: ${PACKAGE}
Cargo version:   ${CARGO_VERSION}
Commit:          ${GITHUB_SHA}
Build:           ${GITHUB_SERVER_URL:-https://github.com}/${GH_REPO}/actions/runs/${GITHUB_RUN_ID}

Includes the harness plugin for Windows x64 and Linux x64, with SHA-256 checksums.
The archive carries extension.json (version rewritten to the package version) and build-info.json.
EOF

if gh release view "$TAG" --json tagName >/dev/null 2>&1; then
  # 同一 tag 重跑：只替换资产与说明，不重建 release
  gh release upload "$TAG" ./* --clobber
  gh release edit "$TAG" --title "$PACKAGE" --notes-file "$notes" --prerelease --latest=false
else
  gh release create "$TAG" ./* \
    --target "$GITHUB_SHA" --title "$PACKAGE" --notes-file "$notes" \
    --prerelease --latest=false
fi
