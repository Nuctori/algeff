# G4 契约终审预检报告（D1–D14 落地核对）

> 审计人：A1（Spec Guardian）。类型：**G4 契约终审预检**（contracts.md §4 G4「契约冻结复审」的准备步骤）。
> 审计基线：
> - worktree `.wt/a1` 分支 `s4/a1` @ `c32209a`（merge: s3/a1 Spec 批3；含 A2 解释器 `003acd7`、D14 `f3494c0`、A3 批2 `1031673`、A6 批2 `b374e68`、A4 批3/批4 `121c806`/`c66c837`）。
> - ⚠️ **基线差异（审计期间更新）**：本 worktree **落后 main（@ `cda2e73`）16 commits**。本报告初稿完成时 main 仅落后 8 commits；审计期间 main 又合并了 **A5（`0549bd5` s1/a5：TokioExecutor 全 DataOp + adapters + 集成测试）**、A7 批2（`49beb04` 基线数据+bench 文档）、A4 批5（`da5aa27` doctest 组合示例）与 cargo fmt（`cda2e73`）。**A5 已合入 main**：main 的 `algeff-std/src/executor.rs` 已无 `todo!()`（0 处），`op_pipe_open` 用 `tokio::io::duplex(PIPE_BUF_SIZE=64K)` 实现 D5；`crates/algeff-std/tests/executor.rs` 含 9 项端到端测试（文件往返/撤销还原/TCP echo/Dup/pipe duplex/MutexLock/spawn wait）。本报告 D1–D14 核对基于 **worktree 实际代码（c32209a）**；main 相对 worktree 的新增（A5/D15/execution_axioms/ResourceArbiter）单列 §4 与偏差清单。
> - 契约文档：`contracts.md`（D1–D14 决策表，`a16380f` 审计修正：D13 补录 + DataOp 39→36；`f3494c0` 补录 D14；**main 与 worktree 均无 D15 条目**）。
> - 基线验证：`cargo test --workspace` 全绿，**91 passed**（core 15 + axioms 22 + interpreter 25 + registry_integration 4 + undo_resource 3 + macro unit 0 + blueprint 5 + exec_integration 5 + macros 8 + doc 4）。

---

## 1. D1–D14 逐条落地核对

状态词：**已实现** = 代码落地且（有测试或可运行路径）；**部分** = 类型/骨架落地但关键语义或物理实现缺失；**待 A5** = 依赖 algeff-std 物理执行层。

| # | 决策 | 落地位置（文件 : 符号 : 行） | 测试/证据 | 状态 |
| --- | --- | --- | --- | --- |
| D1 | `Fd = u64` 全局唯一句柄（单调、永不复用） | `action.rs:14 pub type Fd = u64`；`resource.rs:216 next_fd`、`resource.rs:231 allocate()`（`fd = next_fd; next_fd += 1`，永不复用）；`resource.rs:254 clear()` doc 注明「next_fd 不复位（D1）」 | `resource.rs::handle_allocate_unique_and_take`、`clear_resets_linear_state_and_handles`、`interpreter.rs::runtime_smoke` | ✅ 已实现 |
| D2 | Action 递归字段一律 `Box<Action>` | `action.rs:214 pub enum Action`：`then_branch/else_branch/left/right/inner/target/action/current/on_timeout` 全部 `Box<Action>`（E0072 消解） | 编译通过即证据；`macros.rs` AST 形状测试（`plan!`/`fork!`/`scope!`/`choose!` 展开含 `Box::new`） | ✅ 已实现 |
| D3 | SyscallExecutor 返回 `BoxFuture`（dyn 兼容） | `syscall.rs:13 pub type BoxFuture<'a,T> = Pin<Box<dyn Future + Send + 'a>>`；`syscall.rs:21 trait SyscallExecutor`，`execute`(:24)/`watch_signal`(:31)/`invoke`(:40) 均返回 `BoxFuture` | `runtime.rs:96 Runtime.executor: Box<dyn SyscallExecutor>`（dyn 持有编译通过） | ✅ 已实现 |
| D4 | `UndoOp` 为异步 future | `syscall.rs:15 pub type UndoOp = Pin<Box<dyn Future<Output=()> + Send>>`；`execute -> Result<(Value, Option<UndoOp>), SysError>`；`runtime.rs:60 UndoStack`（push/LIFO recover） | `interpreter.rs::undo_stack_lifo_order`、`replace_recovers_undo_stack`、`undo_resource.rs`（3 项） | ✅ 已实现 |
| D5 | PipeOpen 用 tokio duplex 实现 | **类型载体已冻结**：`resource.rs:207-208 ResourceHandle::PipeReader(Arc<ReadHalf<DuplexStream>>)/PipeWriter(Arc<WriteHalf<DuplexStream>>)`；`action.rs:158-160 PipeOpen { flags: PipeFlags }`（注释标 D5）。**物理实现缺失**：`algeff-std/src/executor.rs:26-29 TokioExecutor::execute = todo!("A5: ...")` | 类型层编译通过；物理层无载体（A5 未合并） | ⚠️ **部分 / 待 A5**（类型已冻结，PipeOpen→duplex 的物理创建待 A5） |
| D6 | `can_parallel` 保守：Append∥Append 默认串行 | `resource.rs:288 can_parallel`（委托 `can_parallel_with(a,b,false)`）、`resource.rs:292 can_parallel_with(..., append_order_insensitive)`；`(Append,Append)` 仅在 opt-in 时并行 | `conflict_matrix_exhaustive_4x4`、`append_parallel_needs_opt_in`、`conflict_matrix_write_blocks`、`disjoint_resources_parallel` | ✅ 已实现 |
| D7 | typestate 包装命名 `TypedResource<M>` | `resource.rs:74 pub struct TypedResource<M>`；`new_read/new_write/new_append/new_owned` + `into_write/into_read/into_append/into_owned` + `into_usage`（`ModeMarker`）；Owned 不可降级（无 into_read 等） | `typestate_usage_mode_matches`；`registry_integration.rs`（4 项）使用 typestate 构造 usage | ✅ 已实现 |
| D8 | `SendFile.in` → `input` | `action.rs:198-203 SendFile { out, input, offset, len }`（注释「契约 D8：in→input（保留字）」） | 编译通过；`describe()` 覆盖 | ✅ 已实现 |
| D9 | Runtime 自持 tokio reactor；`Runtime::new` 须在 tokio 上下文外调用 | `runtime.rs:108 reactor: tokio::runtime::Runtime`；`runtime.rs:112-123 Runtime::new` 创建 reactor，expect 信息「已在 tokio 上下文中？」注明约束；`runtime.rs:178 run_blocking` 用 `reactor.block_on` | `interpreter.rs::runtime_run_blocking_full_path`、`runtime_smoke` | ✅ 已实现 |
| D10 | `Replace` 语义：先 recover 再执行 target | `runtime.rs:349-352 interpret` 的 `Action::Replace` 分支：`undo.recover().await` → `run_sub(*target)`（不回原流） | `interpreter.rs::replace_recovers_undo_stack`（断言 undo 栈清空、target 结果） | ⚠️ 已实现（核心语义）；**缺口**：未调 `reg.clear()`（见 §3 偏差-2） |
| D11 | `Alloc` 返回 `Value::Bytes(vec![0; len])` | `runtime.rs:344-346 interpret` 的 `Action::Alloc` 分支：`next(Value::Bytes(vec![0u8; len]))` | `interpreter.rs::alloc_zeroed_bytes` | ✅ 已实现 |
| D12 | 路径规范化：纯词法（绝对化+消除 `.`/`..`），不碰真实 FS | `resource.rs:319-336 ResourceRegistry::canonicalize_path(p, cwd)`：`Component::CurDir` 丢弃、`ParentDir` pop、其余 push；doc 注明「符号链接解析留给物理执行层」 | `canonicalize_absolute_and_parents`（3 断言）；`interpreter.rs::scope_restores_cwd`（Scope 接入） | ✅ 已实现 |
| D13 | `ResourceRegistry` 实现 `Clone`（Fork 隔离-合并） | `resource.rs:215-216 #[derive(Default, Clone)] pub struct ResourceRegistry`（handles 全 Arc 浅共享 + consumed/owned_consumed 独立）；`runtime.rs:324-325` Fork 分支 `reg.clone()` 隔离 | `registry_integration.rs::fork_clone_merge_pattern`（用 take+allocate 值迁移 workaround，RFC-A3-2） | ⚠️ **部分**：Clone 已实现；**「完成后合并回父」未在 interpret 实现**（l_reg/r_reg 执行后直接丢弃，见 §3 偏差-1） |
| D14 | Fork 阶段 1：静态冲突检测 + 顺序执行（left→right→combine） | `runtime.rs:256-262 fork_conflict`（静态收集左右子树 Syscall 资源 → `can_parallel`）；`runtime.rs:221-250 collect_syscall_resources`（遍历全部 Action 节点）；`runtime.rs:319-330` Fork 分支：`let _conflict = fork_conflict(...)`（检测暂不改调度）+ 顺序 `run_sub(left)` → `run_sub(right)` → `combine(lv,rv)` | `interpreter.rs::fork_conflict_sequential_execution`（断言 op 顺序 write:1→read:1:8）、`fork_disjoint_resources_can_parallel` | ✅ 已实现（阶段 1 语义完整） |

**小结**：D1–D4、D6–D9、D11、D12、D14 **已实现**（10 项）；D5 **待 A5**（类型已冻结）；D10、D13 **部分**（核心语义在，各有一处缺口，见 §3 偏差-1/2）。

---

## 2. 冻结类型一致性（contracts.md §2 ↔ 代码逐项）

| contracts.md §2 声明 | 代码实际 | 结论 |
| --- | --- | --- |
| `Fd = u64` | `action.rs:14 pub type Fd = u64` | ✅ 一致 |
| Action 全部 CPS；递归字段 `Box<Action>` | `action.rs:214 Action`（14 节点全 CPS，9 处 Box 递归字段）；`NextFn/CondFn/CombineFn/HandlerFn` 均在 `action.rs:40-43` | ✅ 一致 |
| `SendFile { out, input, offset, len }` | `action.rs:198-203` | ✅ 一致 |
| `TypedResource<M>`（与 `Resource` 枚举同模块共存） | `resource.rs:74` + `resource.rs:17 enum Resource` | ✅ 一致 |
| `Value`：Unit/Bool/U64/I64/Bytes/Str/Fd/Pid/Addr/List | `action.rs:24-36` **10 个变体**（Unit/Bool/U64/I64/Bytes/Str/Fd/Pid/Addr/List） | ✅ 一致 |
| `DataOp`：**36 个变体**（含 `SendFile` 改名） | `action.rs:67-211` **36 个变体**（本次复核计数：文件 11 + 目录 3 + TCP 6 + UDP 3 + 管道 1 + 进程 3 + 信号 1 + 内存 2 + 时间 1 + 同步 2 + 其他 3 = 36） | ✅ 一致（contracts-audit 修正的 39→36 已生效） |
| `SysError`：14 种 POSIX + `Other(i32)` | `error.rs:8-26` **15 个变体**（14 + Other） | ✅ 一致 |
| `SysError` 含 `from_errno`/`code`/`From<io::Error>` | `error.rs:48 from_errno`、`error.rs:28 code`、`error.rs:94 From<io::Error>` | ✅ 一致 |
| `SyscallExecutor`：dyn 兼容 trait（方法返回 `BoxFuture`，非 async fn） | `syscall.rs:21-46`（execute/watch_signal/invoke 均 `BoxFuture`；`watch_signal`/`invoke` 有默认 ENOSYS 实现） | ✅ 一致 |
| `UndoOp = Pin<Box<dyn Future<Output=()> + Send>>` | `syscall.rs:15` | ✅ 一致 |

**附加说明（非偏差，记录）**：

1. `Seek` 的 `whence` 用 `std::io::SeekFrom`（`action.rs:90`）、`TcpShutdown.how` 用 `std::net::Shutdown`（`action.rs:142`）——pdr §2.2 的自定义 `SeekWhence`/`ShutdownHow` 以 std 类型等价替代，属实现细节（contracts-audit 已记录，非冻结项）。
2. `OpenFlags`/`PipeFlags`/`MmapProt` 手写 struct（`action.rs:47-65`），不引入 bitflags 依赖——实现选择，无契约冲突。
3. `Invoke` 的 `yields` 字段在 `runtime.rs:358` 被 `_` 忽略——A2 当前不消费 yields（Invoke 默认 ENOSYS），与 contracts.md 无冲突（冻结签名保留字段）。
4. 逻辑时钟下沉：pdr §12.3 中 `virtual_clock` 在 Runtime 内，代码实现在 `Context`（`runtime.rs:32-36`）——`interpret` 冻结签名 `(&mut Context, ...)` 使时钟只能从 Context 访问（runtime.rs 注释已说明）。contracts.md 未冻结 Runtime 内部布局（runtime.rs 归 A2），记录不阻塞。

---

## 3. 偏差清单（契约 ↔ 代码）

### 偏差-1（D13 后半句「完成后合并回父」未实现）—— 契约说了，代码没做完整

- **契约**：contracts.md §3 D13「Fork 并行时子任务隔离状态，**完成后合并回父**」。
- **代码**：`runtime.rs:319-330` Fork 分支克隆 `l_reg`/`r_reg`（:324-325）执行两分支后**直接丢弃**，无任何合并回主 registry 的操作；代码注释（:322-323）自认「合并回主 registry 的 API 缺失 → RFC」。合并仅存在于测试 workaround（`registry_integration.rs::fork_clone_merge_pattern` 用 `take`+`allocate` 值迁移，fd 身份重分配、consumed 键不一致）。
- **影响**：Fork 分支内分配的 fd / 消费记录对主 registry 不可见；A5（分支写隔离）执行级语义与 P3 验证依赖的「合并」无载体。
- **建议**：①（推荐）A3 实现 `ResourceRegistry::merge(&mut self, other)`（固定 fd 插入 + consumed/owned_consumed 并集 + `next_fd = max`，即 RFC-A3-2，已在 `spec/resource-notes.md` §7.4 记录），A2 在 Fork 分支接入；②或修改契约 D13 措辞为「隔离状态；合并由调度方（A2）按 RFC-A3-2 执行」。
- **责任人**：A3（merge 原语）→ A2（interpret 接入）；契约措辞裁决归 CTO。
- **对冻结影响**：不阻塞类型冻结（无签名破坏）；阻塞 A5/P3 的执行级闭环。

### 偏差-2（D10 的 registry 侧配合 `clear()` 未接入 interpret）—— 文档承诺了，代码没做

- **契约/文档**：`spec/resource-notes.md` §7.2「Replace → clear()（对应 D10）：registry 侧配合为 `clear()`——释放当前路径积累的全部句柄与线性标记」；`resource.rs:254 clear()` 已实现且 doc 注明「供 A2 的 `Replace`（决策 D10）使用」。
- **代码**：`runtime.rs:349-352` Replace 分支只 `undo.recover().await`（:351），**未调用 `reg.clear()`**；`interpreter.rs::replace_recovers_undo_stack` 用空资源集测试，未覆盖 registry 状态。
- **影响**：Replace 后旧句柄与 consumed/owned_consumed 标记残留在主 registry，target 蓝图在同一注册表上继续执行时可能被陈旧线性状态误伤（语义与 §7.2 文档不符）。
- **建议**：A2 在 Replace 分支 `undo.recover().await` 后补 `reg.clear()`（一行，语义即 D10 的「资源不泄漏」）；或修订 resource-notes 说明「clear 由调用方/物理层负责」。
- **责任人**：A2（runtime.rs）；文档裁决 A1/CTO。
- **对冻结影响**：不阻塞类型冻结；影响 D10 语义完整性。

### 偏差-3（D5 物理实现缺失 —— worktree 内未实现，main 已合并 A5）

- **契约**：contracts.md D5「PipeOpen 用 tokio duplex 实现」。
- **worktree 代码**：类型载体已冻结（`resource.rs:207-208`）；`algeff-std/src/executor.rs` 全部 DataOp 物理执行为 `todo!()`。
- **main 现状（审计期间更新）**：A5 已合入 main（`0549bd5`），`op_pipe_open` 用 `tokio::io::duplex(64K)`（executor.rs 注释「决策 D5」），全部 DataOp 已实现，`tests/executor.rs` 9 项端到端测试在 main 上可运行。
- **影响**：worktree 基线（本报告核对对象）无端到端载体；main 基线已闭环。
- **建议**：G4 终审以 main 为基复核（A5 已就绪）；本报告保留 worktree 视角的 D5=待 A5 标注，但**不再构成阻塞**。
- **责任人**：A5（已完成，main）；CTO 确保合并至终审基线。
- **对冻结影响**：不阻塞（类型形状已冻结，且 main 已实现）。

### 偏差-4（worktree 基线落后 main 16 commits；main 已含 A5/D15/execution_axioms/ResourceArbiter）—— 审计基线事实，需在 G4 终审前核销

- **事实**：本 worktree `s4/a1` @ `c32209a`，main @ `cda2e73`，落后 16 commits（审计期间从 8 涨到 16：A5 `0549bd5`、A7 批2 `49beb04`、A4 批5 `da5aa27`、fmt `cda2e73` 相继合入）。
- **main-only 内容**：
  1. **A5（`0549bd5`）**：TokioExecutor 全 DataOp + adapters + `tests/executor.rs`（9 项端到端）——解决偏差-3、A6 端到端、P4 执行级；
  2. **D15（CTO 裁决，未入契约）**：`executor.rs` 注释「撤销闭包约束（CTO 裁决 D15）：undo 闭包只捕获物理资源数据，禁止捕获 registry 引用」（executor.rs:7、:216）——**contracts.md 与 pdr.md 均无 D15 条目**（已核对 main 两个文件 grep 计数 0），属「代码/裁决做了契约没写」；
  3. `execution_axioms.rs`（s3/a6，7 项执行级测试）与 `ResourceArbiter`（s3/a3，RFC-A3-4 已批——契约决策表亦未收录）；
  4. RFC-1（std path 依赖补 version）、A8 批3 发布面检查、A7 批2 基准数据。
- **建议**：G4 终审以 main 为基复核一次；**D15 补录 contracts.md §3 决策表**（CTO 裁决内容：undo 闭包不得捕获 registry 引用）；`ResourceArbiter` 补录 A1 文档（axioms A7 工程映射 / resource-notes §8）与契约决策表（CTO 裁决）。
- **责任人**：A1（补录文档，下一批）+ CTO（D15/ResourceArbiter 决策表补录 + 合并基线对齐）。
- **对冻结影响**：不阻塞；属审计完整性与契约文档补录事项。

### 偏差-5（无新发现的「代码做了契约没写」破坏项）—— 复核确认

- `watch_signal`/`invoke` 默认 ENOSYS（`Other(38)`）：`syscall.rs:35-46` 提供默认实现——契约未提，属实现细节（pdr §17 已知局限），无冲突。
- `ResourceHandle` 变体（`resource.rs:202-210` File/TcpListener/TcpStream/UdpSocket/PipeReader/PipeWriter/Mutex/Child）与 `SyscallExecutor` 三方法签名：契约未穷举，属冻结骨架内的合理展开，无冲突。
- 上一轮审计（`spec/contracts-audit.md`）记录的 D5 语义降级（内存管道≠OS 管道）、D12 范围收窄（符号链接不解析）继续有效，本次复核代码与记录一致，无新增。

---

## 4. 结论：G4 冻结是否可放行

**总体判定：G4 契约冻结可进入终审流程，但有条件——按以下清单核销后可正式放行。**

**审计期间关键变化（影响结论）**：A5 已在审计期间合入 main（`0549bd5`），原「待 A5」的 D5 物理实现、A6 端到端、P4 执行级**在 main 基线上已闭环**；本报告基于 worktree（c32209a）的「待 A5」标注仅代表 worktree 基线，不代表 main。G4 终审应**以 main 为基**执行复核。

**A5 合并前标记为「待验证」的条目（本批 worktree 基线不核销；main 基线大多已核销）**：

| 条目 | worktree 状态 | main 状态 | 放行前动作 |
| --- | --- | --- | --- |
| D5 物理实现（PipeOpen→duplex） | `TokioExecutor::execute` 为 `todo!()` | ✅ 已实现（`op_pipe_open`，duplex 64K） | main 基线上执行 `tests/executor.rs::pipe_duplex` |
| A6 端到端（文件往返/TCP echo/撤销往返） | 无载体 | ✅ `tests/executor.rs` 9 项 | main 基线 `cargo test -p algeff-std` |
| P4 执行级 / P3 执行级 | 依赖 A5 + Fork 合并（偏差-1） | P4 ✅（撤销还原测试在）；P3 仍依赖 merge API（偏差-1） | main 基线复核 P3 |
| 偏差-2（Replace 未 clear） | interpret 缺一行 `reg.clear()` | 待核实（需在 main 基线复核 interpret） | A2 补丁 + replace 测试补 registry 断言 |
| 偏差-1（Fork 合并回父） | RFC-A3-2 merge API 未落地 | 待核实 | A3 实现 + A2 接入，或 CTO 裁决改契约措辞 |
| 基线对齐（偏差-4） | worktree 落后 main 16 commits | — | 以 main 复核 execution_axioms/ResourceArbiter/D15，补录契约文档 |

**已满足、可直接放行的部分**：

- 冻结类型 §2 全部 9 项一致（Value 10 变体、DataOp 36、SysError 14+Other、SyscallExecutor 签名、UndoOp、TypedResource、SendFile.input、Fd、Box 递归）。
- D1–D4、D6–D9、D11、D12、D14 决策主体全部落地且有测试。
- `cargo test --workspace` 全绿（91 passed，exit 0）。
- 契约文档 D13/D14 已补录，DataOp 计数已修正 39→36（`a16380f`/`f3494c0`）。

**条件式放行表述**：G4 契约冻结在「**以 main（cda2e73）为基复核** + 偏差-1/2 核销 + **D15/ResourceArbiter 补录契约**」三项完成**后**可正式放行。A5 已合入 main，原「待 A5」三项（D5 物理、A6 端到端、P4 执行级）在 main 基线上已具备载体，G4 终审不再以 A5 合并为前置条件；剩余硬前置为偏差-1（Fork 合并）与偏差-2（Replace clear）的落地/裁决，以及 D15 契约补录（契约文档完整性）。在此之前，上表 6 项保持「待验证」状态，不构成冻结类型层面的阻塞（无签名破坏、无契约承诺被静默改写）。建议 CTO 将本报告 §3 偏差-1/2/4 与 §4 放行条件列入 G4 终审检查单。

---

## 附：审计证据清单

- 代码：`crates/algeff-core/src/{action,error,resource,runtime,syscall,lib}.rs`（行号见 §1/§2 表格）；`crates/algeff-std/src/{executor,adapters,lib}.rs`（A5 骨架）；`crates/algeff-macro/src/lib.rs`（4 宏）。
- 测试：`crates/algeff-core/tests/{axioms,interpreter,registry_integration,undo_resource}.rs`、`crates/algeff-macro/tests/macros.rs`、`crates/algeff-macro/tests/{blueprint,exec_integration}.rs`（注：blueprint/exec_integration 位于 `crates/algeff-macro/tests/`）。
- 文档：`contracts.md`（D1–D14、§2 冻结类型、§4 G4；**main 与 worktree 均无 D15**）、`spec/{contracts-audit,resource-notes,verification-plan}.md`。
- main 基线（审计期间新增，`git show main:` 核实）：`crates/algeff-std/src/executor.rs`（全 DataOp 实现、0 处 `todo!()`、D15 注释）、`crates/algeff-std/tests/executor.rs`（9 项端到端）、`crates/algeff-core/tests/execution_axioms.rs`（7 项执行级）、`resource.rs::ResourceArbiter`（RFC-A3-4）。
- 基线验证：`cargo test --workspace`（worktree c32209a）→ **91 passed, 0 failed**（exit 0）；`git status` 干净（审计前后均无未提交改动，除本文件）。
