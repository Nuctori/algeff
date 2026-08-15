#!/usr/bin/env bash
# Algeff 发布脚本（A8 DevOps & CI）
# 用法：scripts/release.sh [VERSION] [--allow-unpublished-deps] [--publish]
#   VERSION                         默认 0.1.0
#   --allow-unpublished-deps        允许依赖「未发布工作区成员」的 crate 通过 dry-run
#                                   预检（当前即 algeff-std → algeff-core）：
#                                   附加 --config patch.crates-io.<name>.path=...
#                                   用本地成员代偿 registry 存在性校验，仅验证
#                                   打包+编译面。真实发布时不可用——发布 std 前
#                                   core 必须已在 crates.io 可解析（先发 core）。
#   --publish                       执行真实发布序列（core → 等镜像 → std → macro）。
#                                   默认只做检查与 dry-run 预览，不真实发布。
# 职责：
#   1. 发布前检查（tag 未存在 + 工作区干净）
#   2. 全 crate dry-run 预览（默认；--allow-unpublished-deps 可预检 std 打包面）
#   3. --publish：按依赖方向依次真实发布，core 与 std 之间等待镜像同步，
#      每个 crate 发布后用独立工程 cargo add 自测，结束后给回滚（yank）提示。
# 注意：默认不实际执行 cargo publish；仅当显式传 --publish 才会真实发布。
# 完整发布清单（步骤/验证点/回滚/已知风险）见 spec/release-checklist.md。
set -uo pipefail

VERSION="0.1.0"
ALLOW_UNPUBLISHED_DEPS=0
DO_PUBLISH=0
WAIT_SECS="${ALGEFF_WAIT_SECS:-300}"

for arg in "$@"; do
    case "$arg" in
        --allow-unpublished-deps) ALLOW_UNPUBLISHED_DEPS=1 ;;
        --publish) DO_PUBLISH=1 ;;
        -h|--help)
            echo "用法：scripts/release.sh [VERSION] [--allow-unpublished-deps] [--publish]"
            echo "  VERSION   默认 0.1.0"
            echo "  --allow-unpublished-deps  允许依赖未发布工作区成员的 crate 通过 dry-run 预检"
            echo "                            （当前为 algeff-std → algeff-core；真实发布不可用）"
            echo "  --publish  执行真实发布序列（core → 等待镜像 → std → macro），默认只预览"
            echo "  环境变量：ALGEFF_WAIT_SECS 覆盖镜像同步等待秒数（默认 300）"
            exit 0
            ;;
        *)
            if [[ "$arg" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                VERSION="$arg"
            else
                echo "错误：未知参数 '$arg'（用法：scripts/release.sh [VERSION] [--allow-unpublished-deps] [--publish]）" >&2
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
#    的发布依赖校验拒绝（"no matching package named `algeff-core` found ... index"）
#    ——这是 cargo 固有行为：带 version 的 path 依赖必须能在 registry index 解析到。
#    algeff-core 0.1.0 真实发布后自然解除（先 core 后 std 的发布顺序）。如需预览
#    std 自身打包/编译面，加 --allow-unpublished-deps（见下方注释与发布清单）。
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

if [ "${failed}" = 1 ]; then
    echo
    echo "==> 存在失败项（见上）。"
    echo "    如需预览 algeff-std 的打包/编译面：scripts/release.sh --allow-unpublished-deps"
    echo "    （真实发布 std 前 core 必须先发布，失败项不会因 --publish 被绕过。）"
    exit 1
fi

# ---- 以下仅在 --publish 时执行 ----
if [ "${DO_PUBLISH}" = 0 ]; then
    echo
    echo "==> 预览完成。真实发布请显式加 --publish："
    echo "  scripts/release.sh ${VERSION} --publish"
    echo "  （先 git tag ${TAG} && git push origin --tags，确保 crates.io 凭据已登录：cargo login）"
    echo "  完整步骤/验证点/回滚/已知风险：spec/release-checklist.md"
    exit 0
fi

# 5. 真实发布序列（core → 等待镜像 → std → macro）
publish_one() {
    local c="$1"
    echo
    echo "==> cargo publish -p ${c} --registry crates-io（真实发布）"
    if cargo publish -p "$c" --registry crates-io; then
        echo "---- ${c} ${VERSION} 发布成功 ----"
        return 0
    fi
    echo "---- ${c} 发布失败（原因见上） ----" >&2
    return 1
}

# 等待某 crate 在 crates.io/镜像索引可见（轮询 cargo search，最多 WAIT_SECS 秒）。
# 发布与索引同步是异步的（rsproxy 等镜像通常数分钟），std 的依赖解析必须等到
# core 在所用索引可见后才能成功。
wait_for_index() {
    local name="$1"
    local elapsed=0 interval=15
    while [ "${elapsed}" -lt "${WAIT_SECS}" ]; do
        if cargo search "${name}" --limit 1 2>/dev/null | grep -q "^${name} = \"${VERSION}\""; then
            echo "OK：${name} ${VERSION} 已在索引可见（等待约 ${elapsed}s）"
            return 0
        fi
        sleep "${interval}"
        elapsed=$((elapsed + interval))
    done
    echo "错误：等待 ${WAIT_SECS}s 后 ${name} ${VERSION} 仍未在索引可见。" >&2
    echo "      可能原因：镜像同步慢 / 网络问题。可设 ALGEFF_WAIT_SECS 加大等待，或" >&2
    echo "      临时移除 ~/.cargo/config.toml 中 [source.crates-io] 的 replace-with" >&2
    echo "      直连 crates.io 索引后重试。" >&2
    return 1
}

# 发布后自测：独立临时工程 cargo add + cargo check，验证真实 registry 依赖可解析可编译。
verify_published() {
    local name="$1"
    local tmp
    tmp="$(mktemp -d)"
    trap 'rm -rf "${tmp}"' RETURN
    if ! ( cd "${tmp}" && cargo init --name verify_algeff -q \
        && cargo add "${name}@${VERSION}" -q \
        && cargo check -q ); then
        echo "验证失败：${name} ${VERSION} 无法被独立工程解析/编译（检查索引同步与网络）" >&2
        return 1
    fi
    echo "OK：独立工程 cargo add ${name}@${VERSION} + cargo check 通过"
}

echo
echo "==> 开始真实发布序列（core → 等待镜像 → std → macro），依赖方向见 spec/release-checklist.md"
publish_one algeff-core || exit 1
wait_for_index algeff-core || { echo "中止发布：core 索引不可见，后续依赖方无法解析。" >&2; exit 1; }
verify_published algeff-core || { echo "中止发布：core 自测失败。" >&2; exit 1; }
publish_one algeff-std || exit 1
wait_for_index algeff-std || true
verify_published algeff-std || true
publish_one algeff-macro || exit 1
verify_published algeff-macro || true

# 6. 发布后验证与回滚提示
echo
echo "==> 全部发布完成。建议验证："
echo "  - docs.rs 构建：https://docs.rs/algeff-core / algeff-std / algeff-macro（触发后约数分钟）"
echo "  - 独立工程 cargo add 已在上方逐 crate 自测（cargo add + cargo check）"
echo "  - crates.io 页面核对 description/license（MIT OR Apache-2.0）/README 实验性声明"
echo
echo "==> 回滚（crates.io 版本不可变，无法删除/覆盖，只能 yank）："
echo "  cargo yank --version ${VERSION} algeff-core   # 阻止新依赖，已依赖的不受影响"
echo "  cargo yank --version ${VERSION} algeff-std"
echo "  cargo yank --version ${VERSION} algeff-macro"
echo "  yank 后如需修复请 bump 版本（如 0.1.1）重新走本脚本；已 yank 版本不可恢复为可用。"
exit 0
