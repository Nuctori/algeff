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
| `algeff-std` | 预包装适配层（open_tcp、read 等） | ~2500 行 | 永久稳定 |
| `algeff-macro` | 极简语法糖宏（可选） | ~300 行 | 极少修改 |

发布顺序（依赖方向）：`algeff-core` → `algeff-std` → `algeff-macro`，见 `scripts/release.sh`。

## 当前实现状态（2026-08，G3 复核）

| 模块 | 状态 | 测试数 | 待办 |
| --- | --- | --- | --- |
| `algeff-core` 解释器（15 节点 + UndoStack + Runtime） | 已实现并合并（A2/A3/A6） | 61（单元 15 + 集成 46） | 无（契约冻结） |
| `algeff-std`（TokioExecutor + 适配器） | A5 交付中：`execute` 为 `todo!()` 桩 | 0（4 个 doc-test） | 实现全部 DataOp、Full 撤销策略、集成测试 |
| `algeff-macro`（plan!/fork!/scope!/choose!） | 已实现并合并（A4） | 13（蓝图 5 + 展开 8） | 无（可选语法糖） |
| 基准 benches（echo/parallel_reads/shared_read/append） | 已合并（A7），`scripts/perf.sh` 可跑基线 | — | 物理执行器落地后刷新基线 |
| CI（`.github/workflows/ci.yml`） | ubuntu + windows：fmt/clippy/test + mdBook 构建 | — | — |
| 文档（`docs/` mdBook + `spec/` 形式化） | 已齐备（G3 门禁） | — | — |

- 测试合计：`cargo test --workspace` 78 个全绿（74 个测试函数 + 4 个 doc-test）。
- 发布准备（G4 前置）：三个 crate 发布面经 `cargo package --list` 校验通过；`algeff-std` 的 `cargo publish --dry-run` 因 `Cargo.toml` 中 path 依赖 `algeff-core` 缺 `version` 被 cargo 拒绝——需 CTO 批准补版本号后解除。

## 快速开始

最小**可运行**蓝图（已用独立项目验证可编译执行；完整示例见 `docs/example.md` 与 `pdr.md` §14）：

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

    // plan! 构造 Sequential 链；纯蓝图（不触物理 IO）当前即可运行
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
> - `Action` 全部节点 CPS，递归字段一律 `Box<Action>`（契约 D2）；蓝图只依赖 `algeff-core`，可自由组合、缓存、重放，物理执行由 `algeff-std` 的 `TokioExecutor` 提供。
> - ⚠️ `TokioExecutor::execute` 目前仍是 A5 交付中的 `todo!()` 桩：含 `Syscall` 节点的蓝图需待 A5 合并后方可运行；纯 `Pure`/`Alloc` 蓝图不受影响。
> - pdr.md §14 示例风格为 `use algeff::prelude::*`（统一 façade crate，随发布提供）；当前工作区以 `algeff-core` / `algeff-macro` 名义发布。

## 文档入口

| 入口 | 内容 |
| --- | --- |
| `docs/` | mdBook 文档（概述/架构/示例/路线图）：`mdbook build docs` 后打开 `docs/book/index.html` |
| `spec/` | 形式化规范（axioms/proofs/contracts-audit/resource-notes/verification-plan） |
| `pdr.md` | 完整设计规范 v3.2（权威源） |
| `contracts.md` | 阶段 0 冻结契约、文件所有权表、决策 D1–D14 |
| `scripts/release.sh` | 发布预览脚本（tag 检查 → dry-run → 发布顺序提示） |
