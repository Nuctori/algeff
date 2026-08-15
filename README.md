# Algeff

将 Unix 效应代数化的跨平台确定性运行时框架（Rust，仅依赖 tokio）。

- 完整设计规范：`pdr.md`（v3.2）
- 阶段 0 冻结契约：`contracts.md`
- 工作区成员：`algeff-core`（Action AST + Runtime 内核）/ `algeff-std`（tokio 物理后端）/ `algeff-macro`（可选语法糖）

```bash
cargo check        # 全工作区
cargo test         # 全工作区
```

## 项目简介

Algeff（Algebraic Effects）是一份独立于宿主语言的理论规范与工程实现指南：所有系统交互被编码为不可变的代数数据类型（Action），副作用从「指令（动词）」转变为「数据（名词）」，使控制流可以被自由组合、缓存、重放。可逆性（`trackΓ`/`recoverΓ`）与反应性（`notify`）由运行时模型保证；Rust 编译器负责内存安全，运行时模型负责业务语义安全，两者分层、互不干扰（pdr.md §0–§1）。实现仅依赖 tokio，编译时间秒级。

## 三层 crate 结构（pdr.md §15）

| Crate | 职责 | 代码量估算 | 稳定性 |
| --- | --- | --- | --- |
| `algeff-core` | Action、ResourceSet、Resource\<M\>、Runtime 内核 | ~2000 行 | 永久冻结 |
| `algeff-std` | 预包装适配层（open_tcp、read 等） | ~1200 行 | 永久稳定 |
| `algeff-macro` | 极简语法糖宏（可选） | ~300 行 | 极少修改 |

发布顺序（依赖方向）：`algeff-core` → `algeff-std` → `algeff-macro`，见 `scripts/release.sh`。

## 当前实现状态（2026-08，G3 复核）

| 模块 | 状态 | 测试数 | 待办 |
| --- | --- | --- | --- |
| `algeff-core` 解释器（13 种 Action 节点 + UndoStack + Runtime + ResourceArbiter + Fork 并行（D17）+ 深度守卫（RFC-11）；coeffects/virtual-clock 为可选特性） | 已实现并合并（A2/A3/A6） | 141（实测 `cargo test -p algeff-core`） | 无（契约冻结） |
| `algeff-std`（TokioExecutor 全 DataOp + 预包装适配器 + 值流组合器 + 错误路径句柄恢复 + 深度守卫测试） | 已实现并合并（A5） | 132（实测 `cargo test -p algeff-std`） | 无 |
| `algeff-macro`（plan!/fork!/scope!/choose!） | 已实现并合并（A4） | 19 + 8 doc-test | 无（可选语法糖） |
| 基准 benches（echo/parallel_reads/shared_read/append + algeff 对比臂） | 已合并（A7），`scripts/perf.sh` 可跑基线 | — | 并行读对比列受 executor 锁串行化限制（pdr §17 已知局限，阶段 3+ 重构） |
| CI（`.github/workflows/ci.yml`） | ubuntu + windows：fmt/clippy/test + feature 测试 + mdBook 构建 | — | — |
| 文档（`docs/` mdBook + `spec/` 形式化） | 已齐备（G3 门禁） | — | — |

- 测试合计：`cargo test --workspace` 300 个测试函数全绿（约 292 个 `#[test]`/`#[tokio::test]` + 8 条 doc-test 断言；40 个测试二进制 + 3 个 doc-test 运行）。
- 特性测试：`crates/algeff-core/tests/runtime_features.rs` 的 7 个测试由 `--features coeffects,virtual-clock` 门控，默认测试不含；CI 双平台补跑 `cargo test --workspace --features coeffects,virtual-clock` 覆盖。
- 性能基线：`perf/baseline-2026-08-15.txt`（A7 批 2-4），含原生 tokio 参照列与 Algeff 对比列（D17 并行 Fork 后复测：echo 103.1%、parallel_reads 366.2%、shared_read 570.9%、append 24.3%；批 3 D14 顺序基线保留为历史对照），接入说明见 `crates/algeff-std/benches/README.md`。
- 发布准备（G4 终验）：三个 crate 的 `cargo publish --dry-run --registry crates-io` 全部通过（RFC-1 已落地：`algeff-std` 的 path 依赖补 `version = 0.1.0`）。`algeff-std` 因依赖尚未真实发布的 `algeff-core`，需 `scripts/release.sh --allow-unpublished-deps`（以本地成员代偿 registry 存在性校验）——属 cargo 固有的发布顺序约束，先真实发布 core、镜像同步后 std 自然解除。

## 快速开始

最小**可运行**蓝图（已用独立项目验证可编译执行；完整示例见 `docs/src/example.md` 与 `pdr.md` §14）：

```toml
[dependencies]
algeff-core  = { path = "crates/algeff-core" }
algeff-macro = { path = "crates/algeff-macro" }
algeff-std   = { path = "crates/algeff-std" }
tokio        = { version = "1", features = ["rt", "macros"] }
```

```rust
use algeff_core::prelude::*;
use algeff_macro::plan;
use algeff_std::TokioExecutor;

fn main() {
    // D9：Runtime::new 须在 tokio 上下文之外调用（普通 fn main 即可）
    let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));

    // plan! 构造 Sequential 链；纯蓝图与含 Syscall 的物理 IO 蓝图均可运行
    let blueprint = plan! {
        Action::Pure(Value::U64(1));
        Action::Pure(Value::U64(2));
    };

    // run_blocking 即 interpret 的阻塞驱动（也可 rt.block_on(runtime.run(..))）
    let result = runtime.run_blocking(blueprint);
    println!("result = {result:?}"); // 期望 Ok(Unit)：plan! 忽略中间值，链收敛于 Unit
}
```

> 说明：
>
> - `Action` 全部节点 CPS，递归字段一律 `Box<Action>`（契约 D2）；蓝图只依赖 `algeff-core`，可自由组合、缓存、重放，物理执行由 `algeff-std` 的 `TokioExecutor` 提供。
> - `TokioExecutor::execute` 已实现全部 DataOp（A5 已合并）：含 `Syscall` 节点的蓝图直接由 tokio 后端驱动，`Runtime::run_blocking` 即可运行。
> - pdr.md §14 示例风格为 `use algeff::prelude::*`（统一 façade crate，随发布提供）；当前工作区以 `algeff-core` / `algeff-macro` 名义发布。

## 文档入口

| 入口 | 内容 |
| --- | --- |
| `docs/` | mdBook 文档（概述/架构/示例/路线图）：`mdbook build docs` 后打开 `docs/book/index.html` |
| `spec/` | 形式化规范（axioms/proofs/proof-obligations/contracts-audit/resource-notes/verification-plan） |
| `docs/src/proof-obligations.md` | 证明义务登记表（A1–A7 × P1–P5 证据链闭环，源文件 `spec/proof-obligations.md`） |
| `pdr.md` | 完整设计规范 v3.2（权威源） |
| `contracts.md` | 阶段 0 冻结契约、文件所有权表、决策 D1–D19 |
| `scripts/release.sh` | 发布预览脚本（tag 检查 → dry-run → 发布顺序提示） |
