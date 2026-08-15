#!/usr/bin/env bash
# A7 性能基线脚本（contracts.md §任务 A7 / pdr.md §19.4 工具链：criterion）。
# 用法：scripts/perf.sh
#   1) 冒烟：8 个 bench 各跑一次 --test（--test 只跑一次，快）
#   2) 正式：8 个 bench 逐个完整运行，输出到 perf/baseline-<date>.txt
# 说明：逐个运行而非 cargo bench 全量，便于单 bench 失败时保留其余结果
#       （失败仅 WARN 不中断）；环境信息写入基线文件头部。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# 原生 tokio 参照列（批 2）+ Algeff 对比列（批 3）
BENCHES=(echo parallel_reads shared_read append \
  algeff_echo algeff_parallel_reads algeff_shared_read algeff_append)
DATE="$(date +%F)"
OUT="perf/baseline-$DATE.txt"
mkdir -p perf

{
  echo "========================================================================"
  echo "Algeff A7 性能基线（原生 tokio 参照列 + Algeff 对比列）— scripts/perf.sh 自动生成"
  echo "========================================================================"
  echo "date:   $DATE $(date +%T)"
  echo "cargo:  $(cargo --version)"
  echo "rustc:  $(rustc --version)"
  echo "os:     $(uname -a)"
  echo
} | tee "$OUT"

echo "== [A7 perf] 冒烟：各 bench --test"
for bench in "${BENCHES[@]}"; do
  echo "-- smoke: $bench"
  cargo bench --bench "$bench" -- --test
done

echo "== [A7 perf] 正式基线：逐 bench 完整运行"
for bench in "${BENCHES[@]}"; do
  echo "-- bench: $bench"
  # 单 bench 失败不中断其余（基线文件仍保留已跑结果）
  if ! cargo bench --bench "$bench" 2>&1 | tee -a "$OUT"; then
    echo "!! [A7 perf] WARN: bench '$bench' 失败，继续其余基准（原因见上方输出）"
  fi
done

echo "== [A7 perf] 完成，基线已记录到 $OUT"
