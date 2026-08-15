#!/usr/bin/env bash
# Algeff 发布脚本（A8 DevOps & CI）
# 用法：scripts/release.sh [VERSION]   （默认 VERSION=0.1.0）
# 职责：发布前检查 + dry-run 预览 + 发布顺序提示。
# 注意：本脚本只做预览与提示，不实际执行 cargo publish。
set -euo pipefail

VERSION="${1:-0.1.0}"
TAG="v${VERSION}"

# 1. 检查 git tag v$VERSION 是否已存在
if git rev-parse -q --verify "refs/tags/${TAG}" >/dev/null 2>&1; then
    echo "错误：tag ${TAG} 已存在（可能已发布过），如需重新发布请先删除 tag。" >&2
    exit 1
fi
echo "OK：tag ${TAG} 尚不存在。"

# 2. 检查 git status 是否干净
if [ -n "$(git status --porcelain)" ]; then
    echo "错误：工作区不干净，请先提交或 stash 所有改动再发布。" >&2
    git status --porcelain >&2
    exit 1
fi
echo "OK：工作区干净。"

# 3. cargo publish -p algeff-core --dry-run 预览
echo "==> cargo publish -p algeff-core --dry-run（仅打包校验，不实际发布）"
cargo publish -p algeff-core --dry-run

# 4. 输出发布顺序提示
echo
echo "==> 发布顺序（依赖方向：core → std → macro）"
echo "  1. cargo publish -p algeff-core    # 核心：Action/Resource/Runtime，永久冻结"
echo "  2. cargo publish -p algeff-std     # 预包装适配层，依赖 algeff-core"
echo "  3. cargo publish -p algeff-macro   # 可选语法糖宏，依赖 algeff-core"
echo
echo "==> 发布前请依次执行："
echo "  git tag ${TAG}"
echo "  git push origin main --tags"
echo "  然后按上述顺序逐个 cargo publish。"
