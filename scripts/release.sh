#!/usr/bin/env bash
# Algeff 发布脚本（A8 DevOps & CI）
# 用法：scripts/release.sh [VERSION]   （默认 VERSION=0.1.0）
# 职责：发布前检查 + 全 crate dry-run 预览 + 发布顺序提示。
# 注意：本脚本只做预览与提示，不实际执行 cargo publish。
set -euo pipefail

VERSION="${1:-0.1.0}"
TAG="v${VERSION}"

# 三个 crate（依赖方向：core → std → macro）
CRATES=(algeff-core algeff-std algeff-macro)

# 1. 检查 git tag v$VERSION 是否已存在
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1; then
    echo "错误：tag ${TAG} 已存在（可能已发布过），如需重新发布请先删除 tag。" >&2
    exit 1
fi
echo "OK：tag ${TAG} 尚不存在。"

# 2. 检查 git status 是否干净
# 注：刻意不加 --allow-dirty——cargo publish 自身的干净检查是第二道防线。
if [ -n "$(git status --porcelain)" ]; then
    echo "错误：工作区不干净，请先提交或 stash 所有改动再发布。" >&2
    git status --porcelain >&2
    exit 1
fi
echo "OK：工作区干净。"

# 3. 本地校验发布面（仅列出包内文件，不编译、不联网）
#    确认 lib.rs / README / tests / benches 都在包内、无意外文件。
echo
echo "==> 本地发布面检查（cargo package --list）"
for c in "${CRATES[@]}"; do
    echo "---- ${c} ----"
    cargo package -p "${c}" --list
done

# 4. 全 crate dry-run 预览（依赖方向：core → std → macro）
#    --registry crates-io：显式指定 crates.io，避免本地 source 替换
#    （如 rsproxy 镜像）导致 "crates-io is replaced with non-remote-registry
#    source" 报错。需要网络（更新 crates.io index）。
#    已知阻塞：algeff-std 的 Cargo.toml 中 path 依赖 algeff-core 未写 version，
#    cargo publish 会拒绝（G4 前置项，须 CTO 批准补版本号）。
echo
echo "==> cargo publish --dry-run（仅打包校验，不实际发布；需要网络）"
for c in "${CRATES[@]}"; do
    echo "---- cargo publish -p ${c} --dry-run --registry crates-io ----"
    cargo publish -p "${c}" --dry-run --registry crates-io
done

# 5. 输出发布顺序提示
echo
echo "==> 发布顺序（依赖方向：core → std → macro）"
echo "  1. cargo publish -p algeff-core --registry crates-io   # 核心：Action/Resource/Runtime，永久冻结"
echo "  2. cargo publish -p algeff-std --registry crates-io    # 预包装适配层，依赖 algeff-core"
echo "  3. cargo publish -p algeff-macro --registry crates-io  # 可选语法糖宏，依赖 algeff-core"
echo
echo "==> 发布前请依次执行："
echo "  git tag ${TAG}"
echo "  git push origin main --tags"
echo "  然后按上述顺序逐个 cargo publish。"
