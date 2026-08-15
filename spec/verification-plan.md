# G2 门禁验证计划（开发 → 审计 → 验收 → 验证 闭环）

> 拥有者：A1（Spec Guardian）。本文件是「开发 → 审计 → 验收 → 验证」闭环的跟踪载体：
> 开发（A2–A7 交付）→ 审计（`spec/contracts-audit.md`）→ 验收（§1/§2 验证矩阵逐项判据）→ 验证（§3 G1–G4 门禁放行）。
> 基准：`contracts.md`（D1–D13 决策表，含 `a16380f` 审计修正：D13 补录、DataOp 39→36、D5/D12 偏差记录）、
> `spec/axioms.md`、`spec/proofs.md`、代码 `crates/`（worktree `.wt/a1` = main @ `b2069a6`）。
> 状态词：**已验证** = 测试/模型检测通过且有记录；**待合并** = 实现或测试已交付，但依赖 A2/A5 合并后才能运行；**未验证** = 无实现载体或测试未落地。
> 修订流程：RFC → CTO 裁决（与 `spec/` 其余文件一致）。

## 0. 基线事实（A1 复核，main @ b2069a6）

| 载体 | 状态 |
| --- | --- |
| `runtime.rs::interpret`（Sequential/Pure/Fork/Choose/Scope/Replace/… 语义） | `todo!()`，A2 **未合并** |
| `algeff-std` `TokioExecutor::execute`（Full 撤销策略物理实现） | `todo!()`，A5 **未合并** |
| `resource.rs`：冲突矩阵 / 线性检查 / `clear()` / 词法规范化（D12） | A3 已合并，13 个单测 |
| `coeffects.rs`：Component 注册 + `sync` + `notify`（feature `coeffects`） | A3 已合并，5 个测试 |
| `crates/algeff-core/tests/axioms.rs`（A6 属性测试） | A6 已合并，22 项（1 项与冻结语义冲突，见 §5 issue-1） |
| `tla/scheduler.tla` + `tla/README.md`（TLC 记录） | A6 已合并，模型检测**通过**（4 不变式 + 1 时序属性） |
| `crates/algeff-macro` plan!/fork!/scope!/choose! | A4 已合并，8 个展开测试 |
| `crates/algeff-std/benches/`（echo/parallel_reads/shared_read/append，criterion） | A7 已合并，**待实际运行** |
| `.github/workflows/ci.yml`、`docs/`、`scripts/` | A8/A7 已合并 |
| `cargo clippy --workspace` | 0 error（core 1 warning：`reactor` never read，A2 合并后消除） |

## 1. 公理验证矩阵（A1–A7）

| 公理 | 工程实现位置（文件 : 符号） | 验证手段 | 验收标准 | 责任人 | 当前状态 |
| --- | --- | --- | --- | --- | --- |
| **A1 结合律** | `runtime.rs::interpret`（Sequential 展开，A2）；`action.rs::Action::Sequential`（载体，已冻结） | 属性（proptest：trace/状态等价） | 对 proptest 生成的记录型 DataOp 序列，`(a;b);c` 与 `a;(b;c)` 在 trace executor 上产生相同操作序列与最终 Γ；CI 绿 | A2（解释语义）+ A6（属性测试） | **未验证**（唯一载体为 interpret，A2 未合并） |
| **A2 单位元** | `action.rs::Action::Pure` / `unit()`（已冻结）；`runtime.rs::interpret` 的 Pure 分支（A2）；A6 静态部分已落地：`a2_empty_resource_set_parallel_always`、`a2_empty_set_is_parallel_identity`、`a2_empty_undo_stack_recover_noop` | 单元 + 属性（静态）；属性/集成（执行级） | 静态：空资源集 `can_parallel` 恒真、空 UndoStack `recover` 无副作用（**已满足**）；执行级：`run(Pure(());a)` 与 `run(a)` 值等价、`Pure` 不产生 UndoOp（待 A2） | A2 + A6 | **已验证**（静态层）；执行级**待合并** |
| **A3 交换律** | `resource.rs::ResourceRegistry::can_parallel` / `can_parallel_with`（A3，已实现）；`runtime.rs::interpret` Fork 调度（A2）；`resource.rs::allocate`（D1 唯一句柄） | 单元（resource.rs 矩阵测试）+ 属性（axioms.rs `a3_can_parallel_symmetric`）+ 模型（TLA 间接覆盖静态层） | §9.1 矩阵穷举（同资源 4×4 × 异资源全组合）输出正确；Append∥Append 默认串行 + `can_parallel_with(...,true)` 才 opt-in 并行（D6）；执行级：不相交资源 Fork 并行结果与顺序执行一致（待 A2） | A3（矩阵）+ A2（Fork 调度）+ A6（属性测试） | **已验证**（矩阵/静态层）；动态调度**待合并** |
| **A4 资源线性** | `resource.rs::ResourceRegistry::check_linear`（A3，已实现，consumed/owned_consumed 双集）；`runtime.rs::interpret` Syscall 节点接入 check_linear（A2）；`TypedResource<Owned>` 防降级 | 单元（resource.rs 5 项 linearity 测试）+ 属性（axioms.rs `a4_write_duplicate_rejected`/`a4_read_repeatable`/`a4_own_is_linear_too`/`a4_disjoint_writes_linear_sequence`/`a4_random_read_write_sequence`） | Write 重复 → `InvalidInput`；Own 后任何模式 → `InvalidInput`；Read/Append 可重复；**Write→Own 合法**（pdr §14 示例，`linearity_write_then_own_legal`）；执行级：解释器对每个 Syscall 调 check_linear（待 A2） | A3（registry）+ A2（接入）+ A6（属性测试） | **已验证**（registry 层，5 项单测 + 3 项属性）；执行级**待合并**；⚠️ A6 属性测试 1 项与冻结语义冲突（§5 issue-1） |
| **A5 分支隔离** | `action.rs::Action::Choose`（已冻结）；`resource.rs::ResourceRegistry: Clone`（D13，已实现）+ `ResourceHandle` 全 Arc（已实现）；`Arc::make_mut` 延迟复制（**未实现**，A2/A3）；`runtime.rs::interpret` Choose/Fork（A2） | 属性（`axiom_a5_choose_write_isolation` 等，未落地）+ 并发（loom，未交付） | Choose 左分支 Write 不影响右分支 Read；Fork 子任务 `make_mut` 写后兄弟分支数据不变；Own 独占转移（仅一个分支持有）；子任务 Close 共享句柄被拒绝 | A2（解释）+ A3（COW 载体）+ A6（测试） | **未验证**（make_mut 与 interpret 均未实现） |
| **A6 撤销双态** | `syscall.rs::UndoOp` / `SyscallExecutor::execute -> Option<UndoOp>`（已冻结）；`runtime.rs::UndoStack::push/recover`、`Runtime::recover`（已实现）；A5 `TokioExecutor` Full 策略（**未合并**） | 单元/属性（axioms.rs `a6_undo_lifo_order`/`a6_undo_restores_observable_state`/`a6_undo_multiple_restores_full_state`）+ 集成（algeff-std 文件往返，待 A5） | UndoStack LIFO 逆序执行、recover 后栈清空、状态复原 w;w̄=1（**已满足**）；端到端：Write 文件 → recover → 内容还原、Open → recover → fd 关闭（Full 策略）；不可逆操作（UdpSendTo/Kill/SendSignal）返回 `None` 不压栈（待 A5/A2） | A2（trackΓ/recoverΓ 接入）+ A5（端到端）+ A6（属性测试） | **已验证**（UndoStack 层）；端到端**待合并** |
| **A7 无死锁** | `tla/scheduler.tla`（A6，已交付，TLC 记录通过）；静态降级 `resource.rs::can_parallel_with`（A3，已实现）；动态原子占坑/回滚/有限重试 `runtime.rs::interpret`（A2，**未实现**）；`spec/resource-notes.md` §2（MutexLock try_lock 方案） | 模型（TLC/Apalache）+ 单元（静态矩阵，已覆盖）+ 压力（执行级，待 A2） | TLC 模型检测：NoCircularWait/ExactHold 等 4 不变式 + Progress 通过（**已满足**，tla/README §3）；执行级：N 任务争 M 资源（M<N）压力测试全部有限步完成或返回错误、重试次数 ≤ B、无永久挂起（待 A2） | A2（动态实现）+ A6（模型检测）+ A3（静态层） | **已验证**（模型层）；执行级**待合并** |

## 2. 命题验证矩阵（P1–P5，同格式一行表）

| 命题 | 工程实现位置（文件 : 符号） | 验证手段 | 验收标准 | 责任人 | 当前状态 |
| --- | --- | --- | --- | --- | --- |
| **P1 幺半群** | A1+A2 载体（interpret，A2）；宏 `plan!{a;b;c}` 展开（A4，已合并，`plan_three_elements_nested_sequential` 验 AST 形状） | 属性（结合律/单位元 trace 等价，待 A2）+ 单元（宏 AST 形状，已验） | `(a;b);c` ≡ `a;(b;c)`、`1;a`≡`a`≡`a;1` 执行 trace 相同（= A1/A2 执行级）；`plan!` 恒等变换（`Sequential(Pure,a)→a`）不改变语义 | A2 + A6（+A4 已交付） | **待合并**（宏 AST 形状**已验证**；执行级依赖 A2） |
| **P2 并行交换律** | A3 载体：`can_parallel`（A3）+ Fork 调度（A2）；对称 combine 前提（`proofs.md` P2 记录） | 属性（静态对称性 `a3_can_parallel_symmetric`，已验）+ 属性（执行级 commutes，待 A2） | 不相交资源 `left∥right` 与 `right∥left` 结果值与最终状态一致（对称 combine）；非对称 combine 反例被记录 | A3 + A2 + A6 | **已验证**（静态对称性）；执行级**待合并** |
| **P3 分支写隔离** | A5 载体：Choose 分支（interpret，A2）+ registry Clone（D13，已实现）+ `Arc::make_mut`（未实现） | 属性（未落地）+ 并发（loom，未交付） | = A5 三项验收标准（Choose 隔离 / Fork COW / Own 独占） | A2 + A3 + A6 | **未验证**（同 A5） |
| **P4 撤销双态** | A6 载体：UndoStack（已实现）+ TokioExecutor Full 策略（A5，未合并）+ `Runtime::recover`（已实现） | 单元（UndoStack 级，已验）+ 集成（端到端，待 A5） | `w;w̄=1` 状态复原（UndoStack 级**已满足**）；Write 文件 → recover → 内容还原；recover 后 Context（cwd/env）复原 | A2 + A5 + A6 | **已验证**（UndoStack 层）；端到端**待合并** |
| **P5 无死锁** | A7 载体：TLA 模型（A6，已交付）+ interpret 动态占坑/回滚/重试（A2，未实现）+ 静态串行降级（D6/§9.1，已实现） | 模型（TLC 已通过）+ 压力（执行级，待 A2）+ 单元（静态矩阵，已覆盖） | TLC 无「持有-等待」环（**已满足**）；占坑失败后 registry 无残留登记（回滚完整性）；静态冲突被顺序调度 | A2 + A6（+A3 静态） | **已验证**（模型层）；执行级**待合并** |

依赖关系：P1←A1+A2；P2←A3；P3←A5；P4←A6；P5←A7+D6/§9.1（同 `proofs.md` 附图）。

## 3. 门禁清单（G1–G4：各自要求「哪些验证项必须为已验证」）

| 门禁 | 判据（contracts.md §4） | 必须为「已验证」的验证项 | 当前缺口 |
| --- | --- | --- | --- |
| **G1** 阶段 1 | 8 分支合并回 main + `cargo test --workspace` 绿 | 不依赖 interpret 的全部静态/registry 项：A3 矩阵层（单测+属性）、A4 registry 层、A6 UndoStack 层、A2 静态层（空集/空栈）、A7 静态层（矩阵）、A4 宏 AST 形状、错误映射/typestate/D1 单测 | ⚠️ main 当前红：A6 `a4_random_read_write_sequence` 与冻结语义冲突（§5 issue-1），G1 绿须先修复该测试；修复后 G1 判据可满足 |
| **G2** 阶段 2 | A1 审计报告闭环（contracts-audit.md）+ `cargo clippy --workspace` 无 error | G1 全部 + **执行级闭环**：A1 结合律、A2 执行级（Pure 跳过/单位元）、A3 动态调度（Fork 并行/串行降级）、A4 执行级（interpret 接入 check_linear）、A5（Choose 隔离 + Fork COW）、A6 端到端（algeff-std Full 策略往返）、A7 动态（占坑/回滚/重试压力测试）；P1–P5 对应执行级项；TLA 模型检测通过（已满足） | **A2 interpret、A5 TokioExecutor 未合并——G2 硬前置**；clippy 0 error 已满足（core 1 warning 随 A2 合并消除）；issue-1 必须在 A4 执行级闭环前修复 |
| **G3** 阶段 3 | bench 可运行 + CI yaml 通过校验 + 文档齐备 | G2 全部保持；criterion 4 项 bench（echo/parallel_reads/shared_read/append）实际运行成功且有基线记录（`scripts/perf.sh`）；ci.yml（ubuntu+windows）通过校验；mdBook `docs/` 构建成功 | bench 已交付（A7）但**未实际运行**；CI/docs 已交付待校验 |
| **G4** 阶段 4 | 发布 0.1.0 + 契约冻结复审 | G3 全部保持；A1–A7、P1–P5 全项闭环（无「未验证/待合并」残留）；A1 复审 `spec/` 与 contracts.md 一致性并确认冻结 | 依赖 G2/G3 全部前置 |

## 4. 风险标注：interpret 未合并前的验证空窗

**空窗定义**：`runtime.rs::interpret`（A2）与 `algeff-std` TokioExecutor（A5）未合并期间，执行级验证无法运行。

**完全阻塞（等 A2 interpret 合并后才能验证）**：
- A1（唯一载体 = Sequential 解释）；A5（Choose/Fork 执行语义 + `make_mut` 触发时机）；A3 动态调度（Fork 并行/串行降级）；A4 执行级（interpret 内接入 check_linear）；A7 动态部分（原子占坑/回滚/有限重试）；P1/P2/P3/P5 的执行级对应项。
- 结论：**G2 是 A2 合并的硬前置门禁**；在 A2 合并前，A1/A5 及 P3 保持「未验证」，属预期空窗而非回归。

**等 A5 TokioExecutor 合并**：A6 端到端（真实文件撤销往返）、P4 执行级——Full 策略物理实现（写前读 + 恢复逆操作）是唯一载体。

**空窗期间可先行且已完成**（不依赖解释器）：A3/A4 registry 层（单元+属性）、A6 UndoStack 层、A7 TLA 模型检测、A2 静态层、A4 宏 AST 形状、错误映射/typestate/D1。**G1 不依赖执行级公理，不受空窗影响。**

**新增风险（A6 交付质量问题，非空窗）**：
- `a4_random_read_write_sequence`（`crates/algeff-core/tests/axioms.rs`）把 `Write→Own` 误判为「重复消费应拒绝」，与冻结语义矛盾——`check_linear` 双消费集设计（`consumed`/`owned_consumed`）明确允许 Write→Own（`resource.rs::linearity_write_then_own_legal`，pdr.md §14 示例），且 A4 验收标准含此序列。该测试当前**确定性失败**（3/3 复现，最小反例 `[Signal:Write, Signal:Own]`）。它把 A4 属性测试与既有单元测试置于互斥状态，须由 A6 修复（对齐冻结语义）并走 CTO 裁决，否则 G1 无法绿、A4 执行级闭环被污染。
- 次要：core 的 `reactor` 字段 warning（never read）在 A2 合并 interpret 后自然消除；在此之前 clippy 保持 0 error，G2 判据不受影响。

## 附：验证项 → 证据清单

| 验证项 | 证据位置 | 记录状态 |
| --- | --- | --- |
| A3 矩阵（单元） | `resource.rs` tests：`conflict_matrix_exhaustive_4x4` 等 6 项 | 已验证（cargo test 绿） |
| A4 registry（单元） | `resource.rs` tests：`linearity_*` 5 项 + `clear_resets_linear_state_and_handles` | 已验证（cargo test 绿） |
| A2/A3/A4/A6 静态+属性 | `crates/algeff-core/tests/axioms.rs` 22 项 | 21 通过 / 1 失败（issue-1） |
| A7 模型 | `tla/scheduler.tla` + `tla/README.md` §3（TLC 输出） | 已验证（4 不变式 + 1 时序属性） |
| A4 宏 AST 形状 | `crates/algeff-macro/tests/macros.rs` 8 项 | 已验证（cargo test 绿） |
| 门禁工具链 | `cargo clippy --workspace`（0 error）、`ci.yml` | 已满足 / 待 CI 实际运行 |
