# G4 契约冻结终审报告（放行终审）

> 审计人：A1（Spec Guardian）。类型：**G4 放行终审**（contracts.md §4 G4「发布 0.1.0 + 契约冻结复审」）。
> 前置文档：`spec/contract-final-audit.md`（批 4 预检，其中标注「待验证」的条目在本批逐一终审）。
> 审计基线：
> - worktree `.wt/a1` 分支 `s5/a1` @ `d17a5d5`（merge: s4/a1 Spec 批4；已含 A2 解释器 `003acd7`、A5 TokioExecutor `0549bd5`、A3 `1031673`、A4 批5 `da5aa27`、A6 `b374e68`、A7 批2 `49beb04`、fmt `cda2e73`）。
> - main @ `e37c5ab`（worktree **落后 main 4 commits**：s3/a2 Runtime 批3「coeffects/virtual-clock 接线」`20ffc21`、s4/a6 Verification 批4「并发压力」`e37c5ab`；二者均不触碰 D1–D14 相关代码路径，main 新增内容单列 §4.2 复核）。
> - 契约文档：`contracts.md`（D1–D14 决策表；**无 D15 条目**——已 grep 复核 contracts.md/pdr.md 计数 0）；`pdr.md` v3.2（§2/§3/§10/§11/§12/§13/§17）。
> - 基线验证（本批实际执行）：`cargo test --workspace` → **121 passed, 0 failed**（exit 0）。明细：core 单测 15 + arbiter 5 + axioms 22 + execution_axioms 7 + interpreter 25 + registry_integration 4 + undo_resource 3 + macro 单测 0 + blueprint 5 + doc_examples 1 + exec_integration 5 + macros 8 + std 单测 0 + adapters 5 + executor 8 + 宏 doc-tests 8 = 121。
> - 代码硬约束：本批只写 `spec/`，不改代码/contracts.md/pdr.md/Cargo.toml；所有「代码级缺口」以终审条件移交 CTO。

---

## 1. 批 4 预检「待验证」条目终审复核（6 项）

批 4 预检 §4 表格列出 6 项「待验证」，本批逐项给出**最终结论**（代码核实 + 测试实跑，见 §5 证据清单）。

| # | 条目 | 批 4 时状态 | 本批核实（文件 : 符号 / 测试） | 最终结论 |
| --- | --- | --- | --- | --- |
| 1 | **D5 物理实现**（PipeOpen→duplex） | 待验证（worktree 无载体） | `executor.rs::op_pipe_open`：`tokio::io::duplex(PIPE_BUF_SIZE = 64K)`，split 出 ReadHalf/WriteHalf 相连管道，两个 fd 入 registry（注释「决策 D5」）；测试 `tests/executor.rs::pipe_duplex`（writer 写 → reader 读 `b"ping"`）**实跑通过** | ✅ **已核销**：D5 全链路落地 |
| 2 | **A6 端到端**（文件往返/TCP echo/撤销往返） | 待验证（无载体） | `tests/executor.rs` 8 项端到端全部实跑通过：`file_write_read_roundtrip`（Open→Write→Seek→Read）、`undo_restores_file_content`（Full 写前读+恢复逆操作，撤销后文件内容还原）、`rename_undo`、`tcp_echo_roundtrip`（Bind→Connect→Write→Accept→Read 全链路）、`dup_shares_handle`、`pipe_duplex`、`mutex_lock_exclusion`、`spawn_wait_exit_code`。**注：实跑计数为 8 项（批 4 记录「9 项」为旧基线计数，以当前代码为准）** | ✅ **已核销** |
| 3 | **P4 执行级**（撤销双态） | 待验证（依赖 A5） | `execution_axioms.rs::exec_A6_undo_roundtrip`（Runtime 路径：interpret 压栈 2 → recover 后 LIFO 逆序 + 栈清空，w;w̄=1 执行级）；`tests/executor.rs::undo_restores_file_content` + `rename_undo`（Full 策略物理端到端）；`interpreter.rs::undo_stack_lifo_order` | ✅ **已核销** |
| 4 | **P3 执行级**（Fork 合并回父） | 待核实（依赖 merge API） | `runtime.rs` Fork 分支（:319-330）`let mut l_reg = reg.clone(); ... run_sub(left, ..., &mut l_reg) ... run_sub(right, ..., &mut r_reg) ...` 后**直接丢弃**，无合并；`resource.rs` **无 `merge` 方法**（grep 确认）；`registry_integration.rs::fork_clone_merge_pattern` 仍为 `take`+`allocate` 值迁移 workaround（fd 身份重分配、consumed 键不一致，注释自认 RFC-A3-2）；main 新增 `concurrency_stress.rs::parallel_runs_isolated_state` 验证的是**隔离**而非合并 | ❌ **未核销**：偏差-1 保持开放（终审条件 C1） |
| 5 | **偏差-2**（Replace 未调 `reg.clear()`） | 待核实 | `runtime.rs` Replace 分支（:349-352）：`undo.recover().await` → `run_sub(*target)`，**无 `reg.clear()`**；`resource.rs::clear`（:254，doc 注明「供 A2 的 Replace（D10）使用」）存在且 `registry_integration.rs::replace_semantics` 验证了 registry 侧语义，但 interpret 未接入 | ❌ **未核销**：偏差-2 保持开放（终审条件 C2） |
| 6 | **基线对齐 + 契约补录**（偏差-4） | worktree 落后 main 16 commits | ① worktree 已前进至 `d17a5d5`（A5/A7批2/A4批5/fmt 全含），**仍落后 main 4 commits**（见 §4.2）；② **D15 仍未补录**：contracts.md/pdr.md 均无 D15，D15 仅存在于 `executor.rs:7、:216` 注释（undo 闭包不得捕获 registry 引用）；③ ResourceArbiter 已实现并有文档（resource-notes §8 / axioms A7 映射），契约决策表未收录 | ⚠️ **部分核销**：基线差距大幅缩小但未归零；D15/ResourceArbiter 契约补录未执行（终审条件 C3） |

**小结**：6 项中 **3 项核销**（D5 物理、A6 端到端、P4 执行级——批 4 预判「A5 合入 main 后闭环」成立），**2 项保持开放**（偏差-1/2，均为代码级一行/裁决级事项），**1 项部分核销**（基线对齐 + 契约补录）。

---

## 2. contracts.md D1–D14 与最终代码逐条核对

核对基准：worktree `s5/a1` 实际代码（main 相对 worktree 的 4 commits 不触碰以下决策路径，见 §4.2 佐证）。

| # | 决策 | 落地位置（文件 : 符号） | 测试/证据 | 状态 |
| --- | --- | --- | --- | --- |
| D1 | `Fd = u64` 全局唯一句柄（单调、永不复用） | `action.rs:14 pub type Fd = u64`；`resource.rs::allocate`（`fd = next_fd; next_fd += 1`）；`resource.rs::clear` doc「next_fd 不复位（D1）」 | `resource.rs` 单测 `handle_allocate_unique_and_take`、`clear_resets_linear_state_and_handles`；`axioms.rs::registry_fd_monotonic_unique_never_reused`；`executor.rs::op_dup2` 注释（D1 使 Dup2 无法精确落到 new_fd，语义退化为「先关 new_fd 再复制」——实现已按 D1 处理） | ✅ **全部落地** |
| D2 | Action 递归字段一律 `Box<Action>` | `action.rs` `Action`：`then_branch/else_branch/left/right/inner/target/action/current/on_timeout` 9 处 `Box`（E0072 消解） | 编译通过即证据；`tests/macros.rs` AST 形状（展开含 `Box::new`）；`exec_integration.rs`（宏展开后动作可执行） | ✅ **全部落地** |
| D3 | SyscallExecutor 返回 `BoxFuture`（dyn 兼容） | `syscall.rs::BoxFuture`；`execute/watch_signal/invoke` 均返回 `BoxFuture`；`runtime.rs` `executor: Box<dyn SyscallExecutor>` | `runtime.rs:96` dyn 持有编译通过；`interpreter.rs` 全部测试经 `&mut dyn SyscallExecutor` 驱动 | ✅ **全部落地** |
| D4 | `UndoOp` 异步 future | `syscall.rs:15`；`execute -> (Value, Option<UndoOp>)`；`runtime.rs::UndoStack::push/recover`（LIFO） | `interpreter.rs::undo_stack_lifo_order`、`replace_recovers_undo_stack`；`execution_axioms.rs::exec_A6_undo_roundtrip`；`tests/undo_resource.rs` 3 项 | ✅ **全部落地** |
| D5 | PipeOpen 用 tokio duplex | `executor.rs::op_pipe_open`（`tokio::io::duplex(64K)`；注释「决策 D5」）；类型载体 `resource.rs::ResourceHandle::PipeReader/PipeWriter` | `tests/executor.rs::pipe_duplex` 实跑通过 | ✅ **全部落地**（批 4「待 A5」→ 已核销） |
| D6 | `can_parallel` 保守：Append∥Append 默认串行 | `resource.rs::can_parallel`（委托 `can_parallel_with(a,b,false)`）；`(Append,Append)` 仅 opt-in 并行 | `resource.rs` `conflict_matrix_exhaustive_4x4`、`append_parallel_needs_opt_in`、`conflict_matrix_write_blocks`、`disjoint_resources_parallel`；`axioms.rs::a3_append_append_requires_opt_in` | ✅ **全部落地** |
| D7 | typestate 命名 `TypedResource<M>` | `resource.rs:74`；`new_read/new_write/new_append/new_owned` + `into_read/into_write/into_append/into_owned` + `into_usage`（ModeMarker）；Owned 无降级方法 | `resource.rs` `typestate_usage_mode_matches`；`axioms.rs::typestate_usage_mode_matches`、`typestate_transitions_are_valid`；`adapters.rs` 全部预包装函数以 `TypedResource` 声明资源 | ✅ **全部落地** |
| D8 | `SendFile.in` → `input` | `action.rs` `SendFile { out, input, offset, len }`（注释「契约 D8」） | 编译通过；`executor.rs::op_send_file`（input 侧 seek+read，out 侧文件/TCP/管道写端） | ✅ **全部落地** |
| D9 | Runtime 自持 tokio reactor；`new` 须在 tokio 上下文外 | `runtime.rs` `reactor: tokio::runtime::Runtime`；`Runtime::new` expect 信息注明约束；`run_blocking` 用 `reactor.block_on` | `interpreter.rs::runtime_smoke`、`runtime_run_blocking_full_path`、`runtime_run_async_full_path`；`execution_axioms.rs::exec_A6_undo_roundtrip`（Runtime 路径） | ✅ **全部落地** |
| D10 | Replace：先 recover 再执行 target | `runtime.rs` Replace 分支：`undo.recover().await` → `run_sub(*target)`（不回原流） | `interpreter.rs::replace_recovers_undo_stack`（LIFO + 栈清空 + target 结果）；`execution_axioms.rs::exec_D10_replace_order`；`registry_integration.rs::replace_semantics`（registry 侧 clear 语义） | ⚠️ **核心语义已落地**；**缺口**：interpret 未调 `reg.clear()`（偏差-2，终审条件 C2） |
| D11 | `Alloc` 返回 `Value::Bytes(vec![0; len])` | `runtime.rs` Alloc 分支：`next(Value::Bytes(vec![0u8; len]))` | `interpreter.rs::alloc_zeroed_bytes`；`runtime_run_blocking_full_path`、`runtime_run_async_full_path`（Bytes[0,0,0]） | ✅ **全部落地** |
| D12 | 路径规范化：纯词法，不碰真实 FS | `resource.rs::canonicalize_path`（CurDir 丢弃、ParentDir pop、其余 push；doc「符号链接解析留给物理执行层」） | `resource.rs::canonicalize_absolute_and_parents`（3 断言）；`interpreter.rs::scope_restores_cwd`、`scope_restores_cwd_on_error`、`scope_nested_cwd_join_and_restore`（Scope 接入） | ✅ **全部落地** |
| D13 | `ResourceRegistry` 实现 `Clone`（Fork 隔离-合并） | `resource.rs` `#[derive(Default, Clone)]`（handles Arc 浅共享 + consumed/owned_consumed 独立）；`runtime.rs` Fork 分支 `reg.clone()` 隔离 | `registry_integration.rs::fork_clone_merge_pattern`（隔离断言 ✅：父不可见子句柄、子消费不污染父；**合并为 workaround**）；main 侧 `concurrency_stress.rs::parallel_runs_isolated_state`（多线程隔离） | ⚠️ **Clone 隔离已落地**；**缺口**：「完成后合并回父」未在 interpret 实现（偏差-1，终审条件 C1） |
| D14 | Fork 阶段 1：静态冲突检测 + 顺序执行 | `runtime.rs::fork_conflict` + `collect_syscall_resources`（遍历全部 Action 节点）+ Fork 分支顺序 `left→right→combine` | `interpreter.rs::fork_conflict_sequential_execution`（断言 op 序 `write:1`→`read:1:8`、combine 30）、`fork_disjoint_resources_can_parallel`；`execution_axioms.rs::exec_fork_conflict_static`（fork_conflict 报冲突 + 顺序 + combine 30） | ✅ **全部落地**（阶段 1 语义完整） |

**小结**：D1–D9、D11、D12、D14 **完全落地且测试覆盖**（12 项）；D5 由批 4 的「待 A5」转为完全落地。**D10、D13 核心语义落地，各存一处代码级缺口**（偏差-1/2，见 §3 与终审条件 C1/C2）。

### 2.1 冻结类型一致性（contracts.md §2 ↔ 代码）——复核无变化

| contracts.md §2 声明 | 代码实际 | 结论 |
| --- | --- | --- |
| `Fd = u64` | `action.rs:14` | ✅ 一致 |
| Action 全 CPS；递归字段 `Box<Action>` | `action.rs` 13 节点全 CPS（9 处 Box 递归字段；NextFn/CondFn/CombineFn/HandlerFn 均在 `action.rs:40-43`） | ✅ 一致 |
| `SendFile { out, input, offset, len }` | `action.rs:198-203` | ✅ 一致 |
| `TypedResource<M>`（与 `Resource` 枚举同模块） | `resource.rs:74` + `resource.rs:17 enum Resource` | ✅ 一致 |
| `Value`：Unit/Bool/U64/I64/Bytes/Str/Fd/Pid/Addr/List | `action.rs:24-36` **10 个变体** | ✅ 一致 |
| `DataOp`：**36 个变体**（含 SendFile 改名） | `action.rs` **36 个变体**（文件 11 + 目录 3 + TCP 6 + UDP 3 + 管道 1 + 进程 3 + 信号 1 + 内存 2 + 时间 1 + 同步 2 + 其他 3 = 36）；`executor.rs` `execute` match **36 分支全覆盖**（0 处 `todo!()`/`unimplemented!()`，grep 复核） | ✅ 一致（**DataOp 36 全覆盖**） |
| `SysError`：14 种 POSIX + `Other(i32)`，含 from_errno/code/From<io::Error> | `error.rs:8-26` 15 变体；`from_errno`/`code`/`From<io::Error>` 齐全 | ✅ 一致 |
| `SyscallExecutor`：dyn 兼容（BoxFuture，非 async fn） | `syscall.rs:21-46`；watch_signal/invoke 默认 ENOSYS | ✅ 一致 |
| `UndoOp = Pin<Box<dyn Future<Output=()> + Send>>` | `syscall.rs:15` | ✅ 一致 |

**撤销策略 Full/BestEffort 落地复核（pdr.md §11.2 / 批 4 待验证项）**：

| 策略 | 落地位置 | 证据 |
| --- | --- | --- |
| **Full**（写前读 + 完整回滚） | `executor.rs::op_write`（`FULL_UNDO_MAX_BYTES = 1MB`：先 seek 定位 + 读覆盖区原内容 + 写回后返回恢复原区域 + `set_len` 的 undo）；`op_truncate`（<1MB 时先 `fs::read` 全量缓存，undo 恢复原内容+原长度）；`op_rename`（undo = 反向 Rename） | `tests/executor.rs::undo_restores_file_content`、`rename_undo` 实跑通过 |
| **BestEffort**（大文件/流式：undo=None） | `op_write`（≥1MB → `None`；只写句柄写前读失败 → 降级 `None`）；`op_truncate`（≥1MB → `None`）；`op_mkdir`（undo 尽力 remove_dir，非空静默失败）；`op_unlink`/`op_rmdir`（`None` + 补偿挂钩注释） | 代码路径核实；executor.rs 注释「BestEffort（pdr.md §11.2）」 |
| **不可逆 → None** | `op_udp_send_to`/`op_kill`/`op_send_signal`/`op_close`/`op_mmap`（COW 语义）均返回 `None` | 代码路径核实（注释「undo=None：不可逆」） |

---

## 3. 偏差清单终审（承接批 4 §3）

### 偏差-1（D13 后半句「完成后合并回父」未实现）—— **保持开放**

- 现状（批 5 复核）：`runtime.rs` Fork 分支克隆 `l_reg`/`r_reg` 执行后**直接丢弃**，无合并操作；`resource.rs` 无 `merge`；代码注释自认「合并回主 registry 的 API 缺失 → RFC」。测试 `fork_clone_merge_pattern` 的合并部分仍是 `take`+`allocate` 值迁移（fd 身份重分配、consumed 键不一致）。main 侧新增 `parallel_runs_isolated_state` 只验证隔离。
- **影响**：Fork 分支内分配的 fd / 消费记录对主 registry 不可见；P3 执行级闭环依赖的「合并」无载体。D14 阶段 1 顺序执行 + 隔离 clone 下，**当前语义自洽**（分支结果经 combine 回归，registry 状态不回传是有意隔离），但契约 D13 字面承诺（「完成后合并回父」）与代码不符。
- **裁决选项**：①（推荐）实现 `ResourceRegistry::merge(&mut self, other)`（固定 fd 插入 + consumed/owned_consumed 并集 + next_fd = max，即 RFC-A3-2，resource-notes §7.4 已记录）+ interpret 接入；②修改契约 D13 措辞为「隔离状态；阶段 1 合并为 no-op，合并语义由阶段 3 并行化交付」。

### 偏差-2（D10 的 registry 侧配合 `clear()` 未接入 interpret）—— **保持开放**

- 现状：`runtime.rs` Replace 分支只 `undo.recover().await`，未调 `reg.clear()`；`resource.rs::clear` 已实现（doc 注明供 D10），`replace_semantics` 验证了 registry 侧语义，`replace_recovers_undo_stack` 用空资源集测试未覆盖 registry 状态。
- **影响**：Replace 后旧句柄与 consumed/owned_consumed 标记残留主 registry，target 蓝图在同一注册表继续执行时可能被陈旧线性状态误伤（与 resource-notes §7.2 文档不符）。
- **裁决选项**：①A2 在 Replace 分支 `undo.recover().await` 后补 `reg.clear()`（一行）；②修订 resource-notes 措辞（clear 由调用方/物理层负责）。

### 偏差-3（D5 物理实现）—— **已闭环**（批 4 预判成立，见 §1 第 1 项）

### 偏差-4（基线对齐 + 契约补录）—— **部分闭环**（见 §1 第 6 项与 §4.2）

### 偏差-5（无新增「代码做了契约没写」破坏项）—— **维持**

- 复核无新增：`watch_signal`/`invoke` 默认 ENOSYS 属实现细节（pdr §17 框架内）；`ResourceHandle` 8 变体与执行器三方法属冻结骨架内合理展开；D5 内存管道语义、D12 词法规范化边界维持批 4/contracts-audit 记录。**新增观察项**（非偏差）：`executor.rs::op_mutex_lock` 用 `m.lock_owned().await` 阻塞获取，与 resource-notes §2「.lock().await 不得在解释器任务内直接使用」的工程映射不完全一致——见 §4.3（A7 观察项），归入终审条件 C4。

---

## 4. 公理 A1–A7 验证状态总表（终审）

参照 `spec/verification-plan.md` §1 矩阵，逐条给出**测试文件 + 测试名 + 结论**。状态词：**已验证** = 测试实跑通过（本批 `cargo test --workspace` 全绿）或 TLA 模型记录在案；**部分** = 有载体但关键子项缺失；**未验证** = 无载体。

| 公理 | 测试文件 : 测试名（断言要点） | 结论 |
| --- | --- | --- |
| **A1 结合律** | `execution_axioms.rs::exec_A1_associativity`：`(a;b);c` 与 `a;(b;c)` 的 op 调用序列一致 + 最终 Value 一致（gettime→read:2:4，50） | ✅ **已验证**（执行级） |
| **A2 单位元** | `axioms.rs::a2_empty_resource_set_parallel_always`、`a2_empty_set_is_parallel_identity`（proptest 空集并行恒真）、`a2_empty_undo_stack_recover_noop`；`execution_axioms.rs::exec_A2_identity`（Pure 前缀/后缀/双侧 op 序列一致 + 值等价 + Pure 不产生 UndoOp）；`interpreter.rs::pure_unit`、`sequential_empty_chain_convergence` | ✅ **已验证**（静态层 + 执行级） |
| **A3 交换律** | 矩阵层：`resource.rs::conflict_matrix_exhaustive_4x4`（同/异资源 4×4 穷举）、`conflict_matrix_read_read_ok`、`conflict_matrix_write_blocks`、`disjoint_resources_parallel`、`append_parallel_needs_opt_in`；属性层：`axioms.rs::a3_conflict_matrix_exhaustive_same_resource`、`a3_conflict_matrix_exhaustive_disjoint_resources`、`a3_append_append_requires_opt_in`、`a3_conflict_matrix_with_append_opt_in`、`a3_can_parallel_symmetric`（proptest 对称性）；调度层：`interpreter.rs::fork_disjoint_resources_can_parallel`（异资源 Fork 顺序执行结果正确）、`fork_conflict_sequential_execution`；`execution_axioms.rs::exec_fork_conflict_static` | ✅ **已验证**（矩阵 + 对称性 + Fork 调度语义；D14 阶段 1 恒顺序执行，`left∥right` 与 `right∥left` 双调度并行比较不适用——阶段 1 设计限定） |
| **A4 资源线性** | registry 层：`resource.rs::linearity_double_write_rejected`、`linearity_write_then_own_legal`、`linearity_own_is_terminal`、`linearity_read_append_repeatable`、`clear_resets_linear_state_and_handles`；属性层：`axioms.rs::a4_write_duplicate_rejected`、`a4_read_repeatable`、`a4_own_is_linear_too`、`a4_disjoint_writes_linear_sequence`（proptest）、`a4_random_read_write_sequence`（proptest，Write→Own 合法对齐冻结语义）；`registry_integration.rs::linearity_sequence_random`（proptest 状态机）、`open_write_close_lifecycle`；执行级：`execution_axioms.rs::exec_A4_linearity_runtime`（interpret 内第二次同资源 Write 在 check_linear 处 Err(InvalidInput)） | ✅ **已验证**（registry 层 + 属性层 + 执行级） |
| **A5 分支隔离** | Choose：`interpreter.rs::choose_picks_then_branch`、`choose_picks_else_branch`（仅被选分支执行，未选分支零效应）；Fork 线性状态隔离：`registry_integration.rs::fork_clone_merge_pattern`（父不可见子句柄、子消费不污染父）；main 侧并发隔离：`concurrency_stress.rs::parallel_runs_isolated_state`（N 任务 registry 隔离 + fd 分配序列一致）；**缺口**：`Arc::make_mut` 延迟复制未实现（pdr §9.2）；Fork 分支执行级 COW 隔离测试无；「子任务 Close 共享句柄拒绝路径」无测试 | ⚠️ **部分**：Choose 隔离 ✅、Fork 线性状态隔离 ✅（registry Clone）；**物理层 make_mut COW 未实现** → P3 未闭环（关联偏差-1/C1） |
| **A6 撤销双态** | UndoStack 层：`axioms.rs::a6_undo_lifo_order`、`a6_undo_restores_observable_state`、`a6_undo_multiple_restores_full_state`；`interpreter.rs::undo_stack_lifo_order`、`catch_after_partial_undo_keeps_stack`（Catch 不触碰撤销栈）；执行级：`execution_axioms.rs::exec_A6_undo_roundtrip`（recover 后 LIFO 逆序 + 栈清空）；端到端（Full 策略）：`tests/executor.rs::undo_restores_file_content`（写前读 + 恢复原内容）、`rename_undo`、`mutex_lock_exclusion`（undo 释放锁）；`tests/undo_resource.rs` 3 项；不可逆返回 None：`op_udp_send_to`/`op_kill`/`op_send_signal`/`op_close` 代码路径核实 | ✅ **已验证**（UndoStack 层 + 执行级 + Full 策略端到端；w;w̄=1 双态） |
| **A7 无死锁** | 模型层：`tla/scheduler.tla` + `tla/README.md`（TLC 通过 `TypeOK`/`ExclusiveHold`/`ExactHold`/`NoCircularWait` 4 不变式 + `Progress` 时序属性——批 3 记录在案，本批未重跑 TLC）；动态原语层：`tests/arbiter.rs::all_claimable_succeeds_and_held`、`partial_conflict_atomic_failure_no_residue`（原子回滚无残留）、`read_read_shared_read_write_exclusive`、`release_allows_reclaim`（幂等）、`finite_retry_eventually_succeeds`（固定序列 4 次内成功，失败无残留）；并发层（main 基线）：`concurrency_stress.rs::concurrent_arbiter_claims`（真实多线程下互斥不变量、无死锁）；静态层：`resource.rs` 冲突矩阵（冲突 → 顺序化，无等待路径） | ✅ **已验证**（模型层 + 原语层 + 并发层）；**观察项**：`op_mutex_lock` 阻塞 `lock_owned` 与 resource-notes §2 非阻塞建议不一致（§4.3，条件 C4）——当前 interpret 单线程 + D14 顺序 Fork 下同一 Runtime 无并发任务，阻塞等待不可达，无死锁可达性 |

**命题 P1–P5 对应（verification-plan §2，终审补充）**：P1（幺半群）= exec_A1_associativity + exec_A2_identity ✅；P2（并行交换律）= a3_can_parallel_symmetric + 矩阵穷举 ✅（静态）；P3（分支写隔离）= 同 A5 ⚠️ 未闭环（make_mut 缺失）；P4（撤销双态）= exec_A6_undo_roundtrip + undo_restores_file_content ✅；P5（无死锁）= TLA + arbiter 5 项 + concurrent_arbiter_claims ✅。

---

## 5. 已知局限（pdr.md §17）终审清单

| pdr §17 局限 | 当前代码状态 | 是否影响 G4 放行 |
| --- | --- | --- |
| 物理层进展属性未证明 | 规范边界：框架不承诺磁盘/网络延迟与进度；无代码层面的错误承诺（IO 错误全部映射 SysError） | **不影响**（规范明示不在证明范围内） |
| 补偿操作的原子性：用户承诺 | `Invoke` 默认 ENOSYS（`Other(38)`，补偿挂钩由用户提供）；executor undo 均尽力而为（rename undo 失败静默、mkdir undo 非空目录静默失败、op_write undo 失败 return）——与 pdr §18 用户责任一致 | **不影响**（用户责任已文档化；A6 只保证框架侧「逆存在且 LIFO 执行」） |
| `Other(i32)` 错误穷尽性削弱 | `error.rs::Other(i32)` 兜底 + `watch_signal`/`invoke` 默认 ENOSYS(38)；Catch 强制处理 14 种 POSIX 错误（`error.rs::SysError` 15 变体，`axioms.rs::error_from_errno_roundtrip_all_variants` 等 4 项验证） | **不影响**（规范明示的权衡） |
| 大文件写入撤销的双倍 IO | **已工程落地**：`FULL_UNDO_MAX_BYTES = 1MB`，`op_write`/`op_truncate` ≥1MB → undo=None（BestEffort，pdr §11.2）；只写句柄写前读失败降级 BestEffort | **不影响**（BestEffort 策略落地且符合 §11.2 分级语义） |
| 动态资源仲裁的锁竞争 | `ResourceArbiter::try_claim` 原子占坑 + 整体回滚（无锁竞争点，arbiter.rs 5 项验证）；执行器侧 `op_mutex_lock` 用 tokio mutex `lock_owned`（阻塞等待）——见 §4.3 观察项 | **不影响放行**（当前执行模型无并发任务、无死锁可达性）；作为观察项列入条件 C4 |
| Windows 兼容性部分支持 | `ci.yml` 矩阵含 windows-latest（check/test/clippy/fmt）；`op_chmod`/`op_chown` 非 Unix 返回 ENOSYS（`#[cfg(not(unix))]`）；`spawn_wait_exit_code` 含 Windows 分支（cmd /C exit）；D5 duplex 跨平台 | **不影响**（「部分支持」即设计声明，非缺陷） |

---

## 6. 终审结论：G4 放行建议

**建议：条件放行（G4 契约冻结可放行，附以下条件清单；条件均为代码级一行/裁决级/契约文档补录，不涉及冻结类型签名，无契约承诺被静默改写）。**

**放行依据（已满足部分）**：

1. **冻结类型 §2 全部 9 项一致**（Value 10 变体、DataOp 36 全覆盖、SysError 14+Other、SyscallExecutor BoxFuture 签名、UndoOp、TypedResource、SendFile.input、Fd、Box 递归）——批 5 复核无变化。
2. **D1–D14 主体全部落地**：12 项完全落地（含批 4 待验证的 D5）；D10/D13 核心语义落地（各 1 处代码级缺口列入条件）。
3. **批 4 预检 6 项待验证中 3 项核销**（D5 物理、A6 端到端、P4 执行级），**2 项保持开放并转入条件**（偏差-1/2），**1 项部分核销**（基线对齐 + D15 补录）。
4. **A1–A7 公理验证**：A1/A2/A3/A4/A6 已验证，A7 已验证（模型+原语+并发层，含 1 观察项），A5 部分（Choose/线性状态隔离已验证；make_mut COW 未实现——与偏差-1 同源，属阶段 3 并行化载体，不阻塞冻结）。
5. **`cargo test --workspace` 全绿**（121 passed, 0 failed，本批实跑）；crates 源码 **0 处 `todo!()`/`unimplemented!()`**。
6. 发布面就绪：workspace version 0.1.0、`scripts/release.sh`（tag→publish 流程）、`ci.yml`（ubuntu+windows 矩阵）、bench 4 项（echo/parallel_reads/shared_read/append）。

**条件清单（核销后 G4 正式放行；前三项为批 4 检查单延续，第四项为本批新增观察项）**：

| 条件 | 内容 | 责任人 | 类型 |
| --- | --- | --- | --- |
| **C1（偏差-1）** | D13「合并回父」二选一：①实现 `ResourceRegistry::merge`（RFC-A3-2：固定 fd 插入 + consumed/owned_consumed 并集 + next_fd=max）并接入 interpret Fork 分支；②CTO 裁决修订契约 D13 措辞（阶段 1 合并为 no-op，合并语义由阶段 3 交付）。P3 执行级随此项闭环 | A3（merge）→ A2（接入）/ CTO 裁决 | 代码级 / 裁决级 |
| **C2（偏差-2）** | interpret Replace 分支 `undo.recover().await` 后补 `reg.clear()`（一行），并给 `replace_recovers_undo_stack` 补 registry 状态断言；或 CTO 裁决修订 resource-notes §7.2 措辞 | A2 / CTO 裁决 | 代码级（一行） |
| **C3（契约补录）** | contracts.md §3 决策表补录 **D15**（CTO 裁决：undo 闭包只捕获物理资源数据、禁止捕获 registry 引用——executor.rs:7/:216 已按此实现）；`ResourceArbiter`（RFC-A3-4）入决策表或 A1 文档正式记录 | CTO / A1 | 文档级 |
| **C4（观察项，新增）** | `op_mutex_lock` 阻塞 `lock_owned` 与 resource-notes §2「try_lock + 回滚重试」工程映射不一致：记录为已知实现差异（当前 D14 顺序执行模型下同一 Runtime 无并发任务、无死锁可达性），或按 A7 原语改为 try_lock 路径 | A5 / CTO 记录 | 记录级 / 代码级 |
| **C5（基线核销）** | G4 正式放行复核建议以 **main（`e37c5ab`）** 为基：worktree 落后 main 4 commits（s3/a2 Runtime 批3：runtime_features 7 测试；s4/a6 Verification 批4：concurrency_stress 3 测试——均只增加 feature 接线与验证测试，不触碰 D1–D14 路径，本批已代码级审阅，见 §4.2） | CTO 合并对齐 | 流程级 |

**对冻结影响的总判定**：所有条件均**不构成冻结类型层面的阻塞**（无签名破坏、无契约承诺被静默改写）；条件核销后，A1–A7 中 A5/P3 的 make_mut 项仍按 pdr §9.2 归入阶段 3 并行化交付（D14 阶段 1 下非必需），G4 放行不要求其先行。

---

## 附：审计证据清单（本批实跑/核实）

- **命令**：`cargo test --workspace`（worktree `s5/a1` @ `d17a5d5`）→ **121 passed, 0 failed**，exit 0；`grep -rn "todo!\|unimplemented!\|TODO" crates/ --include="*.rs"` → 0 处；`grep -n "D15\|fn merge" contracts.md pdr.md crates/algeff-core/src/resource.rs` → contracts.md/pdr.md 0 命中、resource.rs 无 merge。
- **代码**（worktree 实读，行号与批 4 对齐复核）：`crates/algeff-core/src/{action,error,resource,runtime,syscall,coeffects,virtual_clock,lib}.rs`；`crates/algeff-std/src/{executor,adapters,lib}.rs`（全 DataOp 实现，0 todo）；`crates/algeff-macro/src/lib.rs`（4 宏 + doctest）。
- **测试**（worktree 实跑）：`tests/{arbiter,axioms,execution_axioms,interpreter,registry_integration,undo_resource}.rs`（core）、`tests/{macros,blueprint,doc_examples,exec_integration}.rs`（macro）、`tests/{adapters,executor}.rs`（std）。
- **main 基线（git show main: 审阅，未实跑）**：`crates/algeff-core/tests/concurrency_stress.rs`（`parallel_runs_isolated_state`/`concurrent_arbiter_claims`/`replay_under_concurrency`）、`crates/algeff-core/tests/runtime_features.rs`（coeffects 5 项 + virtual-clock 2 项）、`runtime.rs`/`coeffects.rs` diff（仅 feature 接线，不触碰 D1–D14 路径）。
- **模型**：`tla/scheduler.tla` + `tla/README.md`（TLC 4 不变式 + Progress，批 3 记录在案）。
- **文档**：`contracts.md`（D1–D14；无 D15）、`pdr.md` §2/§3/§10/§11/§12/§13/§17、`spec/{contract-final-audit,verification-plan,axioms,proofs,resource-notes,contracts-audit}.md`。
