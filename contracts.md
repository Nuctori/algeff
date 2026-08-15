# Algeff 契约冻结（阶段 0）

> 状态：**已冻结**（阶段 0 完成）。契约变更必须走 RFC → CTO 裁决，禁止擅自改动。
> 源规范：`pdr.md` v3.2。本文件是 8 Agent 并行开发的唯一接口事实来源。

## 1. 文件所有权表（禁止越界修改）

| 路径 | 拥有者 | 说明 |
| --- | --- | --- |
| `contracts.md` | A1（Spec Guardian）审计，CTO 裁决 | 契约本身 |
| `spec/`（axioms/proofs/audit） | A1 | 形式化规范文档 |
| `pdr.md` | A1 建议修订，CTO 裁决 | 源规范（基线只读） |
| `crates/algeff-core/src/action.rs` | **冻结**（CTO） | Action/DataOp/Value 等类型 |
| `crates/algeff-core/src/error.rs` | **冻结**（CTO） | SysError |
| `crates/algeff-core/src/syscall.rs` | **冻结**（CTO） | SyscallExecutor 契约 |
| `crates/algeff-core/src/resource.rs` | A3（基础骨架已冻结） | 冲突检测、typestate、registry 演进 |
| `crates/algeff-core/src/coeffects.rs` | A3（feature `coeffects`） | 依赖表 + notify |
| `crates/algeff-core/src/runtime.rs` | A2 | 解释器、UndoStack、Runtime |
| `crates/algeff-core/src/virtual_clock.rs` | A2（feature `virtual-clock`） | 逻辑时钟 |
| `crates/algeff-core/src/lib.rs` | **冻结**（CTO） | mod 声明与 re-export |
| `crates/algeff-core/tests/` | A2（interpreter 测试）、A6（公理属性测试） | 各自独立文件 |
| `crates/algeff-std/src/executor.rs` | A5 | TokioExecutor |
| `crates/algeff-std/src/adapters.rs` | A5 | 预包装适配器 |
| `crates/algeff-std/src/lib.rs` | **冻结**（CTO） | |
| `crates/algeff-std/Cargo.toml` | CTO（benches 条目可由 A7 追加） | |
| `crates/algeff-std/benches/` | A7 | criterion 基准 |
| `crates/algeff-macro/src/lib.rs` | A4 | plan!/fork!/scope!/choose! |
| `tla/scheduler.tla` | A6 | 调度器模型 |
| `.github/workflows/` | A8 | CI |
| `docs/`、`scripts/`、`README.md` | A8 | 文档与发布 |

规则：**Cargo.toml 一律不许改**（缺依赖 → 报告 CTO）；新文件只能建在自己的目录。
基线测试 `cargo test --workspace` 必须保持绿色（`todo!()` 允许，但不得让别的 crate 编译失败）。

## 2. 冻结类型（代码为准，此处只列要点）

- `Fd = u64`：运行时分配全局唯一句柄（单调、永不复用）——决策 D1。
- `Action` 全部节点 CPS；递归字段一律 `Box<Action>`——决策 D2（pdr.md 伪代码为裸 `Action`，Rust 需装箱）。
- `SendFile { out, input, offset, len }`：`in` 为保留字改名 `input`——决策 D8。
- 类型状态包装命名为 `TypedResource<M>`（与 `Resource` 枚举同模块冲突）——决策 D7。
- `Value`：Unit/Bool/U64/I64/Bytes/Str/Fd/Pid/Addr/List。
- `DataOp`：pdr.md §2.2 全部 36 个变体（含 `SendFile` 改名；审计修正：非 39）。
- `SysError`：14 种 POSIX + `Other(i32)`，含 `from_errno`/`code`/`From<io::Error>`。
- `SyscallExecutor`：dyn 兼容 trait（方法返回 `BoxFuture`，非 async fn）——决策 D3。
- `UndoOp = Pin<Box<dyn Future<Output=()> + Send>>`：异步逆操作——决策 D4。

## 3. 契约决策（D1–D19）

| # | 决策 | 理由 |
| --- | --- | --- |
| D1 | `Fd = u64` 全局唯一句柄 | pdr.md §2.3「Fd 由运行时分配全局唯一句柄，避免重用冲突」；i32 只是示意。**边界注（R2 数学审计/RFC-06）**：承诺范围为不溢出前缀——Fork 右分支分配使 next_fd 二次增长，~362 轮后 u64 溢出（release 回绕 = fd 复用，违反本决策；修复优先级阶段 3+ 中优先） |
| D2 | Action 递归字段装箱 | E0072 无限大小；spec §14 示例本身也 `Box::new` |
| D3 | SyscallExecutor 返回 BoxFuture | async fn 不 dyn 兼容，Runtime 需 `Box<dyn>` |
| D4 | UndoOp 为异步 future | tokio IO 撤销必然异步（恢复文件内容、关闭 fd） |
| D5 | PipeOpen 用 tokio duplex 实现 | 跨平台（Windows 无 OS pipe 的 tokio 封装）；语义：内存管道 |
| D6 | `can_parallel` 保守：Append∥Append 默认串行 | pdr.md §9.1「仅当结果顺序无关时并行」，调用方显式 opt-in |
| D7 | typestate 包装命名 `TypedResource<M>` | 与 `Resource` 枚举同名冲突 |
| D8 | `SendFile.in` → `input` | Rust 保留字 |
| D9 | Runtime 自持 tokio reactor；`Runtime::new` 须在 tokio 上下文外调用 | pdr.md §12.3 `reactor` 字段 |
| D10 | `Replace` 语义：先 recover 再执行 target | 安全默认（资源不泄漏） |
| D11 | `Alloc` 返回 `Value::Bytes(vec![0; len])` | 确定性；COW 优化留给 A2 自选 |
| D12 | 路径规范化：词法（绝对化+消除 `.`/`..`），不碰真实 FS | 确定性；符号链接解析属物理层 |
| D13 | `ResourceRegistry` 实现 `Clone` | Fork 并行时子任务隔离状态，完成后合并回父（A1 审计补录） |
| D14 | Fork 阶段 1 语义：静态冲突检测 + 顺序执行（left→right→combine）；并行化由 A7 基准驱动（阶段 3） | A3 交换律是「可并行」而非「必须并行」；顺序执行保持 combine 语义且零状态共享风险 |
| D15 | undo 闭包只能捕获物理资源数据（Arc 句柄/原内容/路径），禁止捕获 registry 引用 | execute 只拿到 &mut registry，闭包是 'static（审计补录） |
| D16 | `ResourceArbiter`：动态资源仲裁原语（原子占坑+失败回滚，A7 工程载体）——仲裁分层：静态 can_parallel 管 Fork 级；动态 arbiter 管 MutexLock 级（**已接入**：A5 批 7 254eaf3，`op_mutex_lock` 经 `try_claim` + 8×1ms 有限重试 + WouldBlock 快速失败；语义变更：竞争从阻塞等待改为有限重试后失败回滚，见 D-030） | 审计补录；资源仲裁分层无循环等待 |
| D17 | Fork 并行路径：executor 经 `Arc<Mutex<Box<dyn SyscallExecutor>>>` 共享；子任务隔离 registry/undo/context，完成后合并回父（handles/consumed/owned_consumed 并入，next_fd 取 max；undo 按 right-left 合并保持 LIFO）；无法满足 Send 边界时回退顺序 | D13 的完整落地（审计 blocker-1 已修复，A2 批 4） |
| D18 | action.rs 四个闭包类型别名（NextFn/CondFn/CombineFn/HandlerFn）加 `+ Send`，Action 变为 Send | Fork 线程级并行（pdr §19.2 tokio::spawn）前提；捕获约束为 Send 数据；否决 unsafe impl Send（非健全） |
| D19 | `SyscallExecutor: Send` 超 trait；`Runtime::new(Box<dyn SyscallExecutor + Send>)`；删除 unsafe 包装 | 消除 unsafe impl Send 健全性风险（审查 blocker-3）；编译期强制执行器 Send（A2 批 5） |

## 4. 阶段门禁（CTO 执行）

- **G0**：workspace `cargo check` 绿 + 基线提交（已完成）。
- **G1**（阶段 1）：8 分支合并回 `main` + `cargo test --workspace` 绿。
- **G2**（阶段 2）：A1 审计报告闭环（spec/contracts-audit.md）+ `cargo clippy --workspace` 无 error。
- **G3**（阶段 3）：bench 可运行 + CI yaml 通过校验 + 文档齐备。
- **G4**（阶段 4）：发布 0.1.0 + 契约冻结复审。

## 5. 阶段 1 任务定义（各 Agent 的交付物）

- **A1 Spec Guardian**：`spec/axioms.md`（A1–A7 形式化+工程映射表）、`spec/proofs.md`（P1–P5 证明，附 Rust 测试映射）、`spec/contracts-audit.md`（审计 contracts.md 与 pdr.md 一致性，列出偏差与建议；**只读 contracts.md**）。
- **A2 Core Runtime**：实现 `runtime.rs::interpret`（trampoline 解释器，全部 13 个 Action 节点）、`Runtime::run`/`recover`、virtual clock 接入 Sleep；测试 `crates/algeff-core/tests/interpreter.rs`（Pure 单位元、Sequential、Choose、Fork 冲突调度、Scope、Timeout、Catch、Replace、Sleep）。
- **A3 Resource & Coeffects**：完成 `coeffects.rs`（Component 回调语义、store 集成测试）；`resource.rs` 边界打磨 + 冲突矩阵穷举测试；registry 并发安全说明文档（`crates/algeff-core/src/resource.rs` 内注释或 `spec/resource-notes.md`）。
- **A4 DSL & Macro**：实现 `plan!`/`fork!`/`scope!`/`choose!`（syn/quote，纯 AST 构造，不参与类型系统）；测试 `crates/algeff-macro/tests/macros.rs`（展开后动作可达、资源自动收集）。
- **A5 Std Adapters**：实现 `TokioExecutor`（全部 DataOp，Full 撤销策略：写前读 + 恢复逆操作；Open→undo Close；Dup 共享 Arc；不可逆操作返回 None）；`adapters.rs` 预包装函数；集成测试 `crates/algeff-std/tests/`（文件往返、TCP echo、撤销往返）。
- **A6 Verification**：`tla/scheduler.tla`（原子占坑+回滚重试，无循环等待）；`crates/algeff-core/tests/axioms.rs` 属性测试（A1 结合律、A2 单位元、A3 冲突矩阵穷举、A4 线性、A6 撤销往返），proptest 优先。
- **A7 Integration & Perf**：`crates/algeff-std/benches/`（criterion：echo 对比原生 tokio、10 文件并行读、同文件共享读、并行追加）；`scripts/perf.sh`（运行+记录基线）。
- **A8 DevOps & CI**：`.github/workflows/ci.yml`（ubuntu+windows：check/test/clippy/fmt）、mdBook `docs/`（book.toml+SUMMARY+引用 pdr.md）、`scripts/release.sh`（tag→cargo publish 流程）。

## 6. 工作流

- 每个 Agent 一个 git worktree：`.wt/aN`，分支 `s1/aN`（阶段 1）/ `s2/aN`（阶段 2）…。
- 完成后：`cargo test`（自己的 crate 全绿）→ `git add`（仅自己文件）→ `git commit` → 报告 CTO。
- CTO 合并 → 全量验证 → 审计 → 下一批。冲突由 CTO 裁决，不跨分支互相等待。
- 跨 crate 需求（如 std 需要 core 新 API）：先自行绕开或本地实现，报告里写明「RFC 建议」，CTO 批量裁决，禁止等待。
