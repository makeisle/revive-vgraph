#!/usr/bin/env bash
# 도커로 리눅스 배포 바이너리 빌드 — 호스트(윈도우/맥)에 리눅스 툴체인 불필요.
#
# 왜 도커인가: rusqlite `bundled`가 SQLite를 C로 함께 컴파일하므로 타깃마다
# C 컴파일러가 필요하다. 컨테이너 안에서 빌드하면 그걸 그대로 얻는다.
# macOS는 이 방법이 통하지 않는다(Apple SDK 필요) — .github/workflows/release.yml 참고.
#
# 사용법:
#   scripts/build-linux.sh              # x86_64-unknown-linux-gnu (기본)
#   scripts/build-linux.sh musl         # x86_64-unknown-linux-musl (정적 링크)
#
# 산출물: dist/linux-<variant>/{vgraph,vgraph-mcp}
# target/은 호스트 빌드와 섞이지 않게 컨테이너 전용 디렉터리를 쓴다.
set -euo pipefail

variant="${1:-gnu}"
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out="$repo/dist/linux-$variant"

case "$variant" in
  gnu)
    image="rust:bookworm"
    target="x86_64-unknown-linux-gnu"
    setup="true"
    ;;
  musl)
    image="rust:alpine"
    target="x86_64-unknown-linux-musl"
    # alpine 이미지에는 C 툴체인이 없다 — bundled sqlite 컴파일에 필요
    setup="apk add --no-cache musl-dev gcc make"
    ;;
  *)
    echo "unknown variant: $variant (gnu|musl)" >&2
    exit 2
    ;;
esac

mkdir -p "$out"

# MSYS_NO_PATHCONV: git-bash가 컨테이너 경로(/src)를 윈도우 경로로 바꾸지 못하게
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$repo":/src \
  -w /src \
  -e CARGO_TARGET_DIR=/tmp/target \
  "$image" \
  sh -euc "$setup && cargo build --release --bins --target $target && \
           cp /tmp/target/$target/release/vgraph /tmp/target/$target/release/vgraph-mcp /src/dist/linux-$variant/ && \
           echo '--- 연막 테스트 ---' && \
           /src/dist/linux-$variant/vgraph --root /tmp/vg init smoke/local && \
           /src/dist/linux-$variant/vgraph --root /tmp/vg add-node smoke/local --label Cap --slug c1 --prop name=hello && \
           /src/dist/linux-$variant/vgraph --root /tmp/vg commit smoke/local -m local && \
           /src/dist/linux-$variant/vgraph --root /tmp/vg log smoke/local && \
           echo '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\",\"params\":{}}' \
             | /src/dist/linux-$variant/vgraph-mcp --root /tmp/vgm | grep -q vgraph_commit && \
           echo 'mcp tools/list OK' && \
           file /src/dist/linux-$variant/vgraph 2>/dev/null || true"

echo
echo "산출물: $out"
ls -la "$out"
