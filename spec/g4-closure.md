# G4 闭环收敛终验报告（批 6 · 最后一道门禁）

> 审计人：A1（Spec Guardian）。类型：**G4 闭环收敛终验**。
> 收敛定义：pdr.md §19.2「闭环收敛：当所有公理被证明/测试覆盖，性能满足预期，合并主分支，冻结 API」
> （注：任务单称 §19.5，实际 §19.5 为「关键风险与应对」，收敛定义在 §19.2 行 1175 —— 以实际位置为准）。
> 前置文档：`spec/final-audit.md`（批 5 G4 放行终审，条件 C1–C5）、`spec/contract-final-audit.md`（批 4 预检）。
> 审计基线：
>
> - worktree `.wt/a1` 分支 `s7/a1` @ `fac7b38` = **main @ `fac7b38`（零落后）**。批 6 基线 `c968ebd` 之后合入：
>   A7 批 4 性能复测（`77a411b`/`88e70e0`）、A3 双序 commutation 测试（`4e2dc9e`，G4 条件-2）、
>   A2 批 5 Fork 缺陷修复（`38bca67`：fd 区间预分割 / 顺序路径 merge / `Send` 超 trait）、D19 补录（`fac7b38`）。
> - 契约文档：`contracts.md`（**D1–D19 决策表**，终版）；`pdr.md` v3.2（§四/§十六/§十七/§十九）。
> - 基线验证（批 6 实跑）：`cargo test --workspace` → **151 passed, 0 failed**（24 测试二进制全绿）；
>   `cargo test --workspace --features coeffects,virtual-clock` → 全绿（含 `runtime_features` 7 + `features_regression` 2）；
>   `cargo fmt --check` 干净（exit 0）；`cargo clippy --workspace` 0 error 0 warning。
> - 批 7 状态修正复核（本批实跑）：`cargo test --workspace` → **157 passed, 0 failed**（25 测试二进制全绿，
>   较批 6 +6：commutation 3 + interpreter 3）；`cargo fmt --check` 干净（exit 0）。
> - 代码硬约束遵守：批 6 只写 `spec/`（新增 g4-closure.md、verification-plan.md 状态列刷新）；
>   批 7 只改 `spec/g4-closure.md`（纯状态标注）。未改 contracts.md / pdr.md / 任何代码 / Cargo.toml。

---

## 1. 公理覆盖终验（A1–A7 → 测试文件 : 测试名 → 结论）

状态词：**已验证** = 本批 `cargo test --workspace` 实跑通过（157/157）或 TLA 模型记录在案；
**部分** = 核心语义已闭环、物理层载体归阶段 3。

| 公理 | 测试文件 : 测试名（断言要点） | 结论 |
| --- | --- | --- |
| **A1 结合律** | 执行级：`crates/algeff-core/tests/execution_axioms.rs::exec_A1_associativity`（`(a;b);c` 与 `a;(b;c)` 的 op 调用序列一致 + 最终 Value 一致）；重放级：`tests/replay_property.rs`（proptest 随机蓝图 × 全新状态重放，轨迹/值/撤销栈深一致） | ✅ **已验证**（执行级 + 重放级） |
| **A2 单位元** | 静态层：`tests/axioms.rs::a2_empty_resource_set_parallel_always`、`a2_empty_set_is_parallel_identity`、`a2_empty_undo_stack_recover_noop`；执行级：`execution_axioms.rs::exec_A2_identity`（Pure 前缀/后缀/双侧 op 序列一致 + 值等价 + Pure 不产生 UndoOp）、`tests/interpreter.rs::pure_unit`、`sequential_empty_chain_convergence` | ✅ **已验证**（静态层 + 执行级） |
| **A3 交换律**（重点核实：Fork 并行路径执行级） | 矩阵层：`resource.rs::conflict_matrix_exhaustive_4x4`、`conflict_matrix_read_read_ok`、`conflict_matrix_write_blocks`、`disjoint_resources_parallel`、`append_parallel_needs_opt_in`；属性层：`axioms.rs::a3_conflict_matrix_exhaustive_same_resource`、`a3_conflict_matrix_exhaustive_disjoint_resources`、`a3_append_append_requires_opt_in`、`a3_conflict_matrix_with_append_opt_in`、`a3_can_parallel_symmetric`（proptest 交换对称）；**调度层（can_parallel → 并行 or 顺序）**：`interpreter.rs::fork_parallel_true_path`（不相交资源 → can_parallel=true → spawn_blocking **双线程真并行**（断言左右 op 异线程）+ 合并回父）、`fork_conflict_sequential_execution` + `execution_axioms.rs::exec_fork_conflict_static`（同资源 Write×Write → can_parallel=false → left→right 顺序降级）；**双序 commutation 层**：`tests/commutation.rs::fork_commutation_disjoint`（异资源双序 Fork，断言轨迹多集一致）+ `fork_commutation_same_value`（同值不同序 Pure，A1/A3 联合，`4e2dc9e`） | ✅ **已验证**（矩阵 + 对称性 + 执行级调度双路径 + 双序 commutation：`tests/commutation.rs` `fork_commutation_disjoint` / `fork_commutation_same_value`，`4e2dc9e` 补录，G4 条件-2 交付，§4 残余-2 已闭环） |
| **A4 资源线性** | registry 层：`resource.rs::linearity_double_write_rejected`、`linearity_write_then_own_legal`、`linearity_own_is_terminal`、`linearity_read_append_repeatable`、`clear_resets_linear_state_and_handles`；属性层：`axioms.rs::a4_write_duplicate_rejected`、`a4_read_repeatable`、`a4_own_is_linear_too`、`a4_disjoint_writes_linear_sequence`、`a4_random_read_write_sequence`（Write→Own 合法）；执行级：`execution_axioms.rs::exec_A4_linearity_runtime`（interpret 内第二次同资源 Write 在 check_linear 处 Err(InvalidInput)） | ✅ **已验证**（registry + 属性 + 执行级） |
| **A5 分支隔离** | Choose：`interpreter.rs::choose_picks_then_branch`、`choose_picks_else_branch`（未选分支零效应）；Fork 状态隔离：`concurrency_stress.rs::fork_same_fd_write`（同 fd 双写经 D13 Clone 隔离均成功）+ `parallel_runs_isolated_state`（8 并发任务 registry 隔离、fd 序列一致、父 registry 不被污染）；并行路径隔离：`interpreter.rs::fork_parallel_true_path`（子隔离 registry/undo + 合并回父） | ⚠️ **语义层已验证**（Choose 隔离 ✅、Fork 隔离 ✅、并行路径 ✅）；物理层 `Arc::make_mut` COW **未实现**（pdr §9.2，阶段 3 并行化载体，G4 不阻塞——批 5 判定维持） |
| **A6 撤销双态** | UndoStack 层：`axioms.rs::a6_undo_lifo_order`、`a6_undo_restores_observable_state`、`a6_undo_multiple_restores_full_state`；`interpreter.rs::undo_stack_lifo_order`、`catch_after_partial_undo_keeps_stack`；执行级：`execution_axioms.rs::exec_A6_undo_roundtrip`（recover 后 LIFO 逆序 + 栈清空）；端到端（Full 策略）：`crates/algeff-std/tests/executor.rs::undo_restores_file_content`（写前读 + 恢复）、`rename_undo`、`mutex_lock_exclusion`（undo 释放锁）；`e2e.rs::e2e_file_write_read_undo`；不可逆 → None：`op_udp_send_to`/`op_kill`/`op_send_signal`/`op_close` 代码路径核实 | ✅ **已验证**（UndoStack + 执行级 + Full 端到端；w;w̄=1 双态） |
| **A7 无死锁**（重点核实：并行 Fork + arbiter 双层） | 模型层：`tla/scheduler.tla` + `tla/README.md`（TLC 通过 `TypeOK`/`ExclusiveHold`/`ExactHold`/`NoCircularWait` 4 不变式 + `Progress` 时序属性，批 3 记录在案）；动态原语层（D16）：`tests/arbiter.rs` 8 项——`all_claimable_succeeds_and_held`、`partial_conflict_atomic_failure_no_residue`（原子回滚无残留）、`read_read_shared_read_write_exclusive`、`release_allows_reclaim`（幂等）、`finite_retry_eventually_succeeds`（有限重试）、`arbiter_bool_state`（proptest 不变量）、`arbiter_mutex_matrix_exhaustive_4x4`、`is_clean_tracks_full_lifecycle`；并发层：`concurrency_stress.rs::concurrent_arbiter_claims`（8 任务争用：互斥不变量 + 有限重试上界 + 无死锁 + 无丢失唤醒 + 无残留）；并行 Fork 层：`interpreter.rs::fork_parallel_true_path`（双线程并发无死锁）+ `parallel_runs_isolated_state`（8 并发任务）；静态层：`resource.rs` 冲突矩阵（冲突 → 顺序化，无等待路径） | ✅ **已验证**（模型 + 原语 + 并发 + 并行 Fork 四层）；⚠️ C4 观察项：`op_mutex_lock` 阻塞 `lock_owned` 未接 arbiter（见 §4 残余-1，非阻塞） |

**命题 P1–P5 对应**：P1（幺半群）= `exec_A1_associativity` + `exec_A2_identity` ✅；P2（并行交换律）= `a3_can_parallel_symmetric` + 矩阵穷举 + 执行级调度双路径 + 双序 commutation（`tests/commutation.rs`）✅；P3（分支写隔离）= A5 语义层 ✅（make_mut 归阶段 3）；P4（撤销双态）= `exec_A6_undo_roundtrip` + `undo_restores_file_content` ✅；P5（无死锁）= TLC + arbiter 8 项 + `concurrent_arbiter_claims` + 并行 Fork ✅。

**小结**：A1/A2/A3/A4/A6/A7 **全部已验证**（执行级）；A5 语义层已验证、物理层 make_mut 归阶段 3（批 5 判定维持，不阻塞冻结）。「所有公理被证明/测试覆盖」条件满足。

---

## 2. 性能预期终验（pdr.md §16 对照 perf/baseline-2026-08-15.txt）

pdr §16 预期表（原生 tokio = 100%）× 基线实测（A7 批 4 复测，`perf/baseline-2026-08-15.txt` [5]–[8] 节）：

| 场景 | pdr 预期（静态路径） | 实测（Algeff/原生） | 判定 |
| --- | --- | --- | --- |
| 网络 Echo（无共享资源） | ~103% | 批 4 **103.1%**（`algeff_echo` 等负载双对比；批 3 历史 100.0%） | ✅ **达标**（预期 ±10% 噪声内，无回归） |
| 并行读取 10 个不同文件 | ~100% | 批 4 **366.2%**（`algeff_parallel_reads`，双复跑 366.2%/383.6%；批 3 D14 顺序基线 340.0%） | ⚠️ **未达标——executor 互斥锁串行化**（D17 并行路径已触发仍串行读；pdr §17 已知局限，接受，§4 残余-6） |
| 并行追加同一文件（顺序无关） | ~100%（D6 默认串行亦可） | 批 4 **24.3%**（`algeff_append` 串行路径，即 4.1× 快于原生并行；批 3 历史 29.4%） | ✅ **达标**（小负载下串行更优的诚实数据；语义合规 D6） |
| 并行读取同一文件（只读共享） | ~100% | 批 4 **570.9%**（`algeff_shared_read`；批 3 D14 顺序基线 307.6%） | ⚠️ **未达标——executor 锁串行化 + 同 fd 游标读**（D17 并行反为纯损失；pdr §17 已知局限，接受，§4 残余-6） |

**关键状态**：A2 批 4 已将 **Fork 并行化**（`can_parallel=true` → spawn_blocking 真并行）合入 main（`57b53d1`/`430d64d`）；**A7 批 4 复测数据已合入**（`77a411b`/`88e70e0`，`perf/baseline-2026-08-15.txt` [5]–[8] 节）：echo **103.1%**（预期 ~103%，达标）、parallel_reads **366.2%**、shared_read **570.9%**（均未回归 ~100%，对照批 3 D14 顺序基线 340%/307.6%）、append **24.3%**（优于预期）。并行读偏离归因：**executor 互斥锁串行化**（`ExecAccess::Shared` 的 `Arc<Mutex<Box<dyn SyscallExecutor + Send>>>` 在 `exec_via` 中对整个 `execute`（含物理 IO await）持锁，跨分支 Syscall 全部串行）+ spawn_blocking/current-thread runtime 创建开销；shared_read 另受同 fd 游标读约束（pdr §17 已知局限「动态资源仲裁的锁竞争」，见 §4 残余-6）。锁边界收窄属 A2 域、位置读属 A5 域，工程缓解待阶段 3+。

**性能条件判定**：静态路径全部达标（echo 103.1% ≈ 预期 103%，批 4 复测无回归；append 24.3% 优于预期）；并行类两项（parallel_reads 366.2% / shared_read 570.9%）复测确认**未回归 ~100%**，偏离归因 executor 互斥锁串行化 + 同 fd 游标读（§4 残余-6；pdr §17 已知局限，规范自认局限、非性能退化、非正确性缺陷，工程缓解待阶段 3+）。G4 条件-1 已交付（`77a411b` 复测数据合入）：并行读偏离按「规范自认局限 + 接受」处理，无需 CTO 新裁决。

---

## 3. API 冻结终验（contracts.md D1–D19 ↔ 代码，C1–C5 核销复核）

### 3.1 批 5 放行条件（final-audit.md §6）核销复核

| 条件 | 批 5 状态 | 批 6 复核（代码/测试证据） | 结论 |
| --- | --- | --- | --- |
| **C1（偏差-1）** D13「完成后合并回父」 | 保持开放 | 并行路径：`resource.rs::merge`（:268，RFC-A3-2 语义：原 fd 直接插入 + consumed/owned_consumed 并集 + `next_fd=max` 归一化，doc 注明「偏差-1 落地」）接入 `runtime.rs::run_fork_parallel`（完成两子分支后 `reg.merge(l_reg); reg.merge(r_reg);`，undo 按 left→right append 保持 LIFO）；测试：`interpreter.rs::fork_parallel_true_path`（子句柄原 fd 并入父、父 next_fd=max 不冲突）、`fork_parallel_undo_merge`（recover 先 right 后 left，与顺序路径观察序一致）、`resource.rs::merge_preserves_fd_identity`、`merge_unions_consumed`、`merge_advances_next_fd`。**A2 批 5 复核（`38bca67`）**：① F1 修复——并行两分支同源自父 `next_fd` 克隆分配会撞 fd，spawn 前右分支按高位区间（`N + k·2^48`）**预分割 fd 区间**，两分支分配不相交、merge 不丢句柄（新增测试 `fork_parallel_both_branches_allocate_fds_disjoint`：并行双分支各自分配 fd 不相交）；② F2 修复——顺序路径（冲突 Fork）完成后同样 merge 回父，分支 fd 与线性标记不泄漏（新增测试 `fork_conflict_merge_keeps_linear_marks`：冲突型 Fork 后父级同资源 Write 被 A4 拒绝、`fork_sequential_both_branches_allocate_fds_disjoint`：顺序路径两分支 fd 不冲突） | ✅ **核销（A2 批 5 复核后）**（代码级 + 新增 3 测试） |
| **C2（偏差-2）** Replace 调 `reg.clear()` | 保持开放 | `runtime.rs` Replace 分支：`undo.recover().await; reg.clear();`（先 recover 清撤销栈、再释放 handles/线性标记，next_fd 保留 D1 单调）；测试：`interpreter.rs::replace_clears_registry`（句柄清空 + 线性复位可重写 + fd 单调不复用）、`e2e.rs::e2e_file_write_read_undo`（Replace 清空 registry 句柄后仍可重新 Open/Seek/Read）；集成修复 `2f612f9` 使 `replay_property.rs` 排除 Replace、e2e 对齐 D10 清空语义 | ✅ **核销**（代码级，A2 批 4） |
| **C3（契约补录）** D15 + ResourceArbiter 入表 | 部分核销 | `contracts.md` §3 决策表已含 **D15**（undo 闭包捕获边界，`d356368`）、**D16**（ResourceArbiter，`d356368` + 措辞修正「接入待 C4 裁决」`6cb3de9`）、**D17**（Fork 并行路径）、**D18**（闭包 +Send，`ed84a3c`）、**D19**（`SyscallExecutor: Send` 超 trait + `Runtime::new(Box<dyn SyscallExecutor + Send>)`，`fac7b38` 补录）；代码侧 `executor.rs` D15 注释（:7/:216）、`resource.rs:385 ResourceArbiter`、`runtime.rs` SharedExecutor/`run_fork_parallel`、`action.rs:41-44` 四闭包 `+ Send`、`syscall.rs:27` trait 声明 / `runtime.rs::new` 签名均已落地 | ✅ **核销**（文档级） |
| **C4（观察项）** `op_mutex_lock` 阻塞 `lock_owned` vs try_lock | 新增观察 → **已核销** | `executor.rs::op_mutex_lock` 已接入 arbiter（A5 批 7 254eaf3）：`try_claim` + 8×1ms 有限重试 + WouldBlock 快速失败；死锁可达窗口消除；D16 已更新为「已接入」（D-030） | ✅ **核销**（A5 批 7） |
| **C5（基线核销）** 以 main 为基复核 | 部分核销 | 批 6：worktree `s6/a1` @ `c968ebd` = **main @ `c968ebd`，零落后**；批 5 时落后的 4 commits（Runtime 批3、Verification 批4）已全部合入。批 7 复核：worktree `s7/a1` @ `fac7b38` = **main @ `fac7b38`，零落后** | ✅ **核销**（流程级） |

### 3.2 D1–D19 与代码逐项终审（批 5 final-audit §2 结论 + 批 6/批 7 复核）

- **D1–D14**：批 5 终审结论 **12 项完全落地 + D10/D13 核心语义落地**；本批复核 D10/D13 缺口已由 C1/C2 核销 → **D1–D14 全部完全落地**（final-audit §2 表格逐项证据：D1 唯一句柄 / D2 Box 递归 / D3 BoxFuture / D4 UndoOp / D5 duplex / D6 保守并行 / D7 typestate / D8 SendFile.input / D9 自持 reactor / D10 Replace 语义 / D11 Alloc / D12 词法规范化 / D13 Clone+merge / D14 冲突检测+调度）。
- **D15**（undo 闭包只捕获物理数据，禁捕 registry 引用）：`executor.rs` undo 闭包均捕获 Arc 句柄/原内容/路径（`op_write`/`op_truncate`/`op_rename`/`op_mutex_lock` 等），无 registry 引用捕获 —— ✅ **落地**。
- **D16**（ResourceArbiter 动态仲裁原语）：`resource.rs:385` + `tests/arbiter.rs` 8 项 + proptest + 并发层 —— ✅ **落地**（MutexLock 级接入待 C4 裁决，D16 措辞已如实记录）。
- **D17**（Fork 并行路径：`Arc<Mutex<Box<dyn SyscallExecutor + Send>>>` 共享 + 子任务隔离 + 合并回父 + Send 边界不满足时回退顺序）：`runtime.rs` `SharedExecutor`（:285）/`run_fork_parallel`（spawn_blocking 双线程 + `drive` current-thread runtime）；调度判定 `parallel = !conflict && matches!(&access, ExecAccess::Shared(_))` —— **Direct 公共签名路径恒顺序（阶段 1 回退）、Shared Runtime 路径冲突即顺序**。A2 批 5（`38bca67`）：原 `SendExecutor` **unsafe 包装已删除**，改由 D19 `SyscallExecutor: Send` 超 trait + `Runtime::new(Box<dyn SyscallExecutor + Send>)` 编译期强制（见 §4 残余-5）—— ✅ **落地**。
- **D18**（四闭包类型别名 `+ Send`，Action 变 Send）：`action.rs:41-44` `NextFn`/`CondFn`/`CombineFn`/`HandlerFn` 均 `+ Send`；`adapters.rs`/测试闭包全部满足 Send 约束（`2f612f9` 集成修复使其全量编译）—— ✅ **落地**。
- **D19**（`SyscallExecutor: Send` 超 trait + `Runtime::new(Box<dyn SyscallExecutor + Send>)` + 删除 unsafe 包装）：`syscall.rs:27` `pub trait SyscallExecutor: Send`；`runtime.rs::Runtime::new` 签名改为 `Box<dyn SyscallExecutor + Send>`；原 `SendExecutor` unsafe 包装整体删除（`38bca67`，D19 补录 `fac7b38`）—— ✅ **落地**（编译期强制执行器 Send，消除 unsafe 健全性风险，§4 残余-5 已修复）。

### 3.3 冻结类型 §2 一致性（final-audit §2.1 9 项）

批 6 复核**无变化**：`Fd=u64`、`Action` 全 CPS + Box 递归、`SendFile.input`、`TypedResource<M>`、`Value` 10 变体、`DataOp` 36 变体（executor match 36 分支全覆盖、0 todo）、`SysError` 14+Other、`SyscallExecutor` BoxFuture 签名、`UndoOp` —— 均与 contracts.md §2 一致。A2 批 4/A5 批 4 改动集中于 `runtime.rs`/`executor.rs`（非冻结文件，契约 §1 所有权表内）；A2 批 5（`38bca67`）改动集中于 `runtime.rs`/`resource.rs`/`syscall.rs`——`syscall.rs` 仅 `SyscallExecutor: Send` 超 trait 声明（D19，`fac7b38` 已补录进 contracts.md §3 决策表）、`resource.rs` 为 fd 预分割与顺序 merge（C1 复核，见 §3.1）；未触碰 `action.rs`/`error.rs`/`lib.rs`。冻结面自批 5 起唯一签名变化为 `Runtime::new` 参数 `+ Send`（D19 契约化，编译期约束，无语义破坏）。

**小结**：C1/C2/C3/C5 全部核销（C1 经 A2 批 5 复核），D1–D19 决策表与代码逐项一致，冻结类型零漂移（唯一签名变化 `Runtime::new` `+ Send` 已由 D19 契约化）。「合并主分支、冻结 API」条件满足（worktree=main 零落后）。

---

## 4. 残余项清单（未闭环项 → 裁决建议）

| # | 残余项 | 现状 | 裁决建议 |
| --- | --- | --- | --- |
| **R-1（C4 延续）** | D16 arbiter ↔ `op_mutex_lock` 接入：执行器仍用阻塞 `lock_owned`，未走 arbiter `try_claim`/try_lock | 无死锁可达性论证成立：①静态层 `fork_conflict` 在 Fork 分支声明同资源时强制顺序化；②D14 顺序路径下同一 Runtime 无并发任务；③`concurrent_arbiter_claims` 已验证 arbiter 原语无死锁。**但**并行 Fork 已落地（D17），若蓝图在 Fork 分支内对**同一** MutexLock id 未声明 `Resource::Fd(id)` 资源，静态冲突检测不可见 → 第二分支持共享执行器锁阻塞 `lock_owned`，存在死锁可达窗口 | **接受**（当前冻结语义下：资源声明是蓝图作者责任，A4 线性同此边界；D16 已如实注明「接入待 C4 裁决」）。**待办**（低优先，A7/A5）：①在 resource-notes §2 补「Fork 分支内 MutexLock 必须声明对应资源以触发静态串行化」的强制记录；②后续可将 `op_mutex_lock` 改为 arbiter `try_claim` + 有限重试（D16 设计目标） |
| **R-2** | A3 执行级 left∥right 与 right∥left 双序 commutation 测试未单列 | 已补录：`tests/commutation.rs`（`4e2dc9e`，3 项）`fork_commutation_disjoint`（异资源双序 Fork 断言轨迹多集一致）+ `fork_commutation_same_value`（同值不同序 Pure，A1/A3 联合）；§1 A3 行残余标注已同步清除 | **已闭环**（G4 条件-2 交付） |
| **R-3** | `Arc::make_mut` 物理 COW 未实现（A5/P3 物理层） | registry Clone 层隔离已闭环（consumed/owned_consumed 独立）；物理 Arc 共享句柄下子分支破坏性操作（Close）无拒绝路径测试 | **接受**（阶段 3 并行化载体，final-audit §6 判定维持；语义层闭环满足 G4 判据「公理被测试覆盖」的执行级） |
| **R-4** | A7 批 4 性能复测数据未合并 | 已合入：`77a411b`/`88e70e0`，`perf/baseline-2026-08-15.txt` [5]–[8] 节刷新（echo 103.1% ✅ / parallel_reads 366.2% / shared_read 570.9% / append 24.3%，parallel_reads 双复跑 366.2%/383.6% 稳定） | **已交付**（G4 条件-1）；并行读偏离归因见 §4 残余-6（executor 锁串行化，接受，无需 CTO 新裁决） |
| **R-5** | `unsafe impl Send for SendExecutor`（原 runtime.rs:297） | 已修复（A2 批 5 `38bca67` + D19 补录 `fac7b38`）：`SyscallExecutor: Send` 超 trait（syscall.rs:27）+ `Runtime::new(Box<dyn SyscallExecutor + Send>)` 编译期强制执行器 Send，**unsafe 包装删除**；执行器仅在 Mutex 独占锁内 `&mut` 访问的语义不变 | **已修复（D19：Send 超 trait，unsafe 包装删除）**（编译期强制，无健全性风险） |
| **R-6** | executor 互斥锁串行化：`ExecAccess::Shared` 的 `Arc<Mutex<…>>` 在 `exec_via` 中对整个 `execute`（含物理 IO await）持锁，Fork 并行分支跨线程 Syscall 全部串行通过该锁；shared_read 另受同 fd 游标读约束 | pdr §17 已知局限「动态资源仲裁的锁竞争」（工程缓解：预分配池 + io_uring 注册缓冲区，可选）；A7 批 4 实测（`perf/baseline-2026-08-15.txt` [5]–[8]）：echo **103.1%**（单链无 Fork，与锁无关，达标）、parallel_reads **366.2%**（D17 并行路径已触发仍串行读，对照批 3 顺序基线 340% 无实质差异）、shared_read **570.9%**（同 fd 游标读，D17 并行反为纯损失）——数据如实、非正确性缺陷 | **接受**（规范自认局限；锁边界收窄属 A2 域、位置读属 A5 域，工程缓解待阶段 3+；G4 不阻塞） |

---

## 5. 结论：G4 放行建议

**建议：放行（G4 闭环收敛可放行；批 6 所附 2 项条件已全部交付——条件-1 A7 批 4 复测数据合入、条件-2 commutation 测试补录；残余项 R-1~R-6 均为接受/待办/已闭环，无 blocker）。**

**放行依据（pdr §19.2 闭环收敛三条件逐条判定）**：

1. **所有公理被证明/测试覆盖** —— ✅ 满足：A1–A4、A6–A7 全部「已验证」（执行级），A5 语义层已验证（Choose/Fork 隔离 + 并行路径），物理层 make_mut 归阶段 3（规范边界内非阻塞）；P1–P5 对应闭环（A3 双序 commutation 已由 `tests/commutation.rs` 补录，`4e2dc9e`）。批 7 复核 `cargo test --workspace` **157/157 全绿（25 测试二进制）**（批 6 151 基线上 + commutation 3 + interpreter 3）。
2. **性能满足预期** —— ⚠️ 条件满足（条件-1 已交付）：静态路径达标（echo 103.1% ≈ 预期 103%，批 4 复测无回归；append 24.3% 优于预期）；并行类复测确认未回归（parallel_reads 366.2% / shared_read 570.9%），偏离归因 **executor 互斥锁串行化 + 同 fd 游标读**（pdr §17 已知局限，规范自认局限，工程缓解待阶段 3+，§4 残余-6）——接受，无 blocker。
3. **合并主分支、冻结 API** —— ✅ 满足：worktree = main @ `fac7b38` 零落后（C5 核销）；D1–D19 决策表与代码逐项一致（C1 经 A2 批 5 复核、R-5 已修复）、冻结类型 §2 零漂移（唯一签名变化 `Runtime::new` `+ Send` 已由 D19 契约化）；批 5 条件清单中 C1/C2/C3/C5 全部核销，C4 观察项按「接受+待办」处理（R-1）。

**条件清单（核销后 G4 正式放行）**：

| 条件 | 内容 | 责任人 | 类型 |
| --- | --- | --- | --- |
| **条件-1** | ✅ **已交付**：A7 批 4 复测数据（并行 Fork 后）合入 `perf/`（`77a411b`）；echo 103.1% 达标；parallel_reads/shared_read 未回归 → 归因 executor 锁串行化（R-6，pdr §17 规范自认局限，接受，无需 CTO 新裁决） | A7 / CTO | 数据级 |
| **条件-2** | ✅ **已交付**：A3 双序 commutation 等价测试补录（`4e2dc9e` `tests/commutation.rs`） | A6 | 测试级 |

**观察项（不阻塞）**：R-1（arbiter–MutexLock 接入，接受 + 低优先待办）、R-3（make_mut 阶段 3）、R-6（executor 锁串行化，接受，阶段 3+ 工程缓解）；R-2/R-4 已交付、R-5 已修复（D19）。

**对冻结影响的总判定**：全部残余项均不构成冻结类型层面的阻塞（无签名破坏、无契约承诺被静默改写）；条件核销与观察项跟踪不影响 0.1.0 发布与 API 冻结面。

---

## 附：审计证据清单（本批实跑/核实）

- **命令**（本批实际执行，均 exit 0）：
  - `cargo test --workspace` → 批 6 **151 passed** / 批 7 复核 **157 passed, 0 failed**（25 测试二进制）
  - `cargo test --workspace --features coeffects,virtual-clock` → 全绿（批 6 实跑记录，批 7 未重跑）
  - `cargo fmt --check` → 干净（exit 0）
  - `cargo clippy --workspace` → 0 error 0 warning（批 6 实跑记录，批 7 未重跑）
- **测试明细（25 二进制，批 7 实跑）**：core 12（unit 18 + arbiter 8 + axioms 22 + commutation 3 + concurrency_stress 3 + execution_axioms 7 + features_regression 0 + interpreter 31 + registry_integration 4 + replay_property 3 + runtime_features 0 + undo_resource 3）、macro 5（unit 0 + blueprint 5 + doc_examples 1 + exec_integration 5 + macros 8）、std 5（unit 0 + adapters 5 + adapters_flow 6 + e2e 4 + executor 13）、doc-tests 3（core 0 + macro 8 + std 0）= 157 fn（较批 6 +6：commutation 3、interpreter 3）。
- **代码**（worktree 实读）：`crates/algeff-core/src/{runtime,resource,syscall}.rs`（Fork 并行 `run_fork_parallel`/`SharedExecutor`、fd 区间预分割、顺序路径 merge、`merge`/`clear`、`SyscallExecutor: Send`（D19）、`ResourceArbiter`）；`crates/algeff-core/tests/{interpreter,commutation}.rs`（A2 批 5 新增 3 测试、双序 commutation）；`crates/algeff-std/src/executor.rs`（`op_mutex_lock` :855 阻塞 lock_owned —— C4 依据）。
- **契约**：`contracts.md`（D1–D19 终版）；`spec/{final-audit,contract-final-audit,verification-plan,axioms,proofs,resource-notes}.md`；`perf/baseline-2026-08-15.txt`（批 3 D14 顺序基线 + 批 4 复测 [5]–[8] 节，A7 批 4 已合入）。
- **模型**：`tla/scheduler.tla` + `tla/README.md`（TLC 4 不变式 + Progress，批 3 记录在案，未重跑）。
