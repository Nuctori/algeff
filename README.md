# Algeff

> ⚠️ **实验性项目（Research-grade / Experimental）**
>
> Algeff 是一个**研究型/实验性框架**：其**语义正确性**已经过 5 轮「对抗审计（E2E）× 形式逻辑审计」验证（公理 A1–A7、命题 P1–P5 证据链闭环，P4 部分收敛），但**并行性能与跨平台错误语义仍有已知缺口**（executor 锁串行化、Windows 错误码映射等）。
> - **版本**：`0.1.0`，**尚未发布**（`cargo publish --dry-run` 已通过，处于发布前准备态）。
> - **稳定性承诺**：不提供。契约（`contracts.md`）虽已冻结，契约之外的实现层（runtime/executor）仍在演进，API 可能变化。
> - **生产采用门槛**：需先完成阶段 3+ 已知缺口——RFC-05（Replace 句柄活性）、RFC-06（fd 区间溢出）、RFC-08（Timeout 孤儿副作用）、RFC-09（Timeout 锁饥饿）、RFC-10（Windows 错误码映射）与 **R-6 锁重构**（executor 互斥锁串行化，pdr.md §17 已知局限），完整清单见 `spec/resource-notes.md` §10。

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
| CI（`.github/workflows/ci.yml`） | ubuntu + windows + macos：fmt/clippy/test + release 编译验证 + 特性测试 + mdBook 构建 | — | — |
| 文档（`docs/` mdBook + `spec/` 形式化） | 已齐备（G3 门禁） | — | — |

- 测试合计：`cargo test --workspace` 300 个测试函数全绿（约 292 个 `#[test]`/`#[tokio::test]` + 8 条 doc-test 断言；40 个测试二进制 + 3 个 doc-test 运行）。
- 特性测试：`crates/algeff-core/tests/runtime_features.rs` 的 7 个测试由 `--features coeffects,virtual-clock` 门控，默认测试不含；CI 三平台补跑 `cargo test --workspace --features coeffects,virtual-clock` 覆盖。
- 性能基线：`perf/baseline-2026-08-15.txt`（A7 批 2-4），含原生 tokio 参照列与 Algeff 对比列（D17 并行 Fork 后复测：echo 103.1%、parallel_reads 366.2%、shared_read 570.9%、append 24.3%；批 3 D14 顺序基线保留为历史对照），接入说明见 `crates/algeff-std/benches/README.md`。
- 发布准备（G4 终验）：三个 crate 的 `cargo publish --dry-run --registry crates-io` 全部通过（RFC-1 已落地：`algeff-std` 的 path 依赖补 `version = 0.1.0`）。`algeff-std` 因依赖尚未真实发布的 `algeff-core`，需 `scripts/release.sh --allow-unpublished-deps`（以本地成员代偿 registry 存在性校验）——属 cargo 固有的发布顺序约束，先真实发布 core、镜像同步后 std 自然解除。

## 设计推导（工程论文式摘要）

一条从动机到验证的推导链（完整长文见 `docs/src/derivation.md`；证据以 spec/ 与决策链 D- 编号为准）：

1. **动机**：Unix 效应是「指令（动词）」而非「数据（名词）」——无法组合/缓存/重放 → 代数化为不可变 Action 蓝图。→ 详见 `pdr.md` §0–§1
2. **公理化**：A1–A7 七条公理（结合律/单位元/交换律/资源线性/分支隔离/撤销双态/无死锁）奠定语义地基。→ 详见 `spec/axioms.md`
3. **命题**：P1–P5 五条可证明性质（幺半群/并行交换律/写隔离/撤销双态/无死锁），证明建立在公理之上。→ 详见 `spec/proofs.md`
4. **契约冻结**：D1–D19 决策表——冻结面即正确性承诺边界（8 Agent 并行开发的唯一接口事实来源）。→ 详见 `contracts.md` §3
5. **关键决策推导**：Fd=u64 单调不复用（D1）；Fork 并行 = 静态冲突判定 + 并行/顺序双路径（D14/D17）；Replace = recover + clear（D10）；深度守卫阈值 96 = 实测 2MB 栈崩溃边界 ~104–108 → 留 ~8% 余量（D-052）。→ 详见 `contracts.md` §3 与决策链 D-052
6. **实现**：三层 crate——core 解释器（13 种 Action 节点）/ std tokio 执行器 / macro 语法糖。→ 详见上文「三层 crate 结构」与 `pdr.md` §15
7. **验证分层**：300 个测试函数（约 292 个二进制测试 + 8 条 doc-test 断言），40 个测试二进制。→ 详见 `spec/verification-plan.md`
8. **对抗审计 5 轮**：120 个 E2E 测试，每轮独立发现——R1 游标/句柄活性、R2 fd 区间/管道/Timeout 孤儿、R3 盲区/锁饥饿、R4 Windows errno/栈溢出、R5 收敛终判。→ 详见 `spec/proof-obligations.md` 轮次日志
9. **数学审计 5 轮**：P1/P2/P3/P5 收敛为「有效（附声明前提）」，P4 部分收敛（差距 = RFC-05，阶段 3+ 已裁决）。→ 详见 `spec/proof-obligations.md` 义务明细
10. **缺陷库**：RFC-05~11 全部登记（`spec/resource-notes.md` §10）；审计期内 3 项缺陷已修复——RFC-11（递归栈溢出 → 深度守卫可捕获错误）、R1 游标撤销（D-031）、R3 SendFile flush（D-046）——其余为阶段 3+ 已裁决。→ 详见 `spec/resource-notes.md` §10
11. **性能推导**：echo 103.1%（顺序 ≈ 原生 tokio）、parallel_reads 366.2% / shared_read 570.9%（D17 并行已触发但被 executor 锁串行化）、append 24.3%（串行默认的诚实数据）。→ 详见 `perf/baseline-2026-08-15.txt`
12. **结论**：实验性交付——语义正确性定案（5 轮审计收官），并行性能与跨平台错误语义为已知开放面，生产采用需先完成阶段 3+ 缺口。

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
