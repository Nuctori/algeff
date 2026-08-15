#!/usr/bin/env bash
# Algeff 发布脚本（A8 DevOps & CI）
# 用法：scripts/release.sh [VERSION] [--allow-unpublished-deps]
#   VERSION                         默认 0.1.0
#   --allow-unpublished-deps        允许依赖「未发布工作区成员」的 crate 通过
#                                   dry-run（当前即 algeff-std 依赖 algeff-core）：
#                                   附加 --config patch.crates-io.<name>.path=...
#                                   用本地成员代偿 registry 存在性校验，仅验证
#                                   打包+编译面；registry 存在性由真实发布顺序保证。
# 职责：发布前检查 + 全 crate dry-run 预览 + 发布顺序提示。
# 注意：本脚本只做预览与提示，不实际执行 cargo publish。
set -uo pipefail

VERSION="0.1.0"
ALLOW_UNPUBLISHED_DEPS=0
for arg in "$@"; do
    case "$arg" in
        --allow-unpublished-deps) ALLOW_UNPUBLISHED_DEPS=1 ;;
        -h|--help)
            echo "用法：scripts/release.sh [VERSION] [--allow-unpublished-deps]"
            echo "  VERSION                 默认 0.1.0"
            echo "  --allow-unpublished-deps  允许依赖未发布工作区成员的 crate 通过 dry-run"
            echo "                            （当前为 algeff-std → algeff-core，仅验证打包+编译面）"
            exit 0
            ;;
        *)
            if [[ "$arg" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                VERSION="$arg"
            else
                echo "错误：未知参数 '$arg'（用法：scripts/release.sh [VERSION] [--allow-unpublished-deps]）" >&2
                exit 2
            fi
            ;;
    esac
done
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
#    --registry crates-io：显式指定 crates.io，避免本地 source 替换（如 rsproxy
#    镜像）导致 "crates-io is replaced with non-remote-registry source" 报错；
#    需要网络（更新 crates.io index）。
#    已知约束（RFC-1 落地 version=0.1.0 后）：algeff-std 的 dry-run 仍会被 cargo
#    的发布依赖校验拒绝（"no matching package named `algeff-core` found ...
#    crates.io index"）——这是 cargo 固有行为：带 version 的 path 依赖必须能在
#    registry index 解析到。algeff-core 0.1.0 真实发布后自然解除（先 core 后 std
#    的发布顺序）。如需预览 std 自身打包/编译面，加 --allow-unpublished-deps。
echo
echo "==> cargo publish --dry-run（仅打包校验，不实际发布；需要网络）"
failed=0
for c in "${CRATES[@]}"; do
    echo "---- cargo publish -p ${c} --dry-run --registry crates-io ----"
    args=(publish -p "$c" --dry-run --registry crates-io)
    if [ "${ALLOW_UNPUBLISHED_DEPS}" = 1 ] && [ "$c" = "algeff-std" ]; then
        args+=(--config 'patch.crates-io.algeff-core.path="crates/algeff-core"')
        echo "      [附加 patch.crates-io.algeff-core → 本地成员：仅验证打包+编译面]"
    fi
    if cargo "${args[@]}"; then
        echo "---- ${c}: PASS ----"
    else
        echo "---- ${c}: FAIL（原因见上） ----"
        failed=1
    fi
done

# 5. 输出发布顺序提示
echo
echo "==> 发布顺序（依赖方向：core → std → macro）"
echo "  1. cargo publish -p algeff-core --registry crates-io   # 核心：Action/Resource/Runtime，永久冻结"
echo "  2. cargo publish -p algeff-std --registry crates-io    # 预包装适配层，依赖 algeff-core"
echo "  3. cargo publish -p algeff-macro --registry crates-io  # 可选语法糖宏，依赖 algeff-core"
echo
echo "==> 注意事项："
echo "  - algeff-std 的 dry-run/publish 要求 algeff-core（同版本）已在 crates.io 可解析："
echo "    先真实发布 core，等镜像同步（rsproxy 通常数分钟）再发布 std；"
echo "    若报 'no matching package named algeff-core'，等待同步，或临时移除"
echo "    ~/.cargo/config.toml 中 [source.crates-io] 的 replace-with 以直连 crates.io 索引。"
echo
echo "==> 发布前请依次执行："
echo "  git tag ${TAG}"
echo "  git push origin main --tags"
echo "  然后按上述顺序逐个 cargo publish。"

if [ "${failed}" = 1 ]; then
    echo
    echo "==> 存在失败项（见上）。"
    echo "    如需预览 algeff-std 的打包/编译面：scripts/release.sh --allow-unpublished-deps"
    exit 1
fi
exit 0
