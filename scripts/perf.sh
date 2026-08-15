#!/usr/bin/env bash
# A7 性能基线脚本（contracts.md §任务 A7 / pdr.md §19.4 工具链：criterion）。
# 用法：scripts/perf.sh
#   1) 冒烟：cargo bench --bench echo -- --test（--test 只跑一次，快）
#   2) 正式：cargo bench 全量运行，输出到 perf/baseline-<date>.txt
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

echo "== [A7 perf] 冒烟：cargo bench --bench echo -- --test"
cargo bench --bench echo -- --test

echo "== [A7 perf] 正式全量基准：cargo bench"
mkdir -p perf
OUT="perf/baseline-$(date +%F).txt"
cargo bench 2>&1 | tee "$OUT"

echo "== [A7 perf] 完成，基线已记录到 $OUT"
