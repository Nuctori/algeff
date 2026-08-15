# contracts.md 与 pdr.md 一致性审计（D1–D13）

> 审计人：A1（Spec Guardian）。审计基准：
> - `contracts.md`（阶段 0 冻结版，worktree `.wt/a1`，commit `f119797`）
> - `pdr.md` v3.2（基线只读）
> - 现有代码 `crates/algeff-core/src/`（冻结类型，worktree `.wt/a1`）
>
> 方法：逐条对照决策 D1–D13 与 §2 冻结类型列表，核对 pdr.md 原文、代码实际签名，输出「一致 / 偏差 / 建议」结论，并判定**是否影响契约冻结**。本文档只读 contracts.md，不修改。

---

## 0. 结论摘要

| 项 | 结论 | 冻结影响 |
| --- | --- | --- |
| D1–D4、D6–D9、D11 | 一致（含 3 处"澄清/补全"：D7、D10、D11） | 无 |
| **D5**（Pipe→tokio duplex） | **偏差（语义降级）**：内存管道 ≠ Unix 管道 | 不阻塞（类型形状不变），需语义记录 |
| **D12**（词法路径规范化） | **偏差（范围收窄）**：不做符号链接解析 | 不阻塞（以 D12 为合同权威），建议修订 pdr §2.3 |
| **D13**（registry Clone） | 实现一致；**contracts.md 决策表缺失该条目** | 不阻塞（实现已冻结），建议补录 |
| §2 冻结类型列表 | 基本一致；**1 处计数错误**（DataOp "39 个变体" → 实际 36） | 无（文档错误） |

**总评**：契约冻结**可维持**。2 处语义偏差均在类型/API 形状之外，不改变任何冻结签名；但必须在本文件记录语义权威，避免 A2/A3/A5 实现时按 pdr 字面语义实现造成行为分歧。建议 3 项 CTO 行动（见 §8）。

---

## 1. 决策逐条审计

### D1：`Fd = u64` 全局唯一句柄 —— ✅ 一致

- pdr.md §2.3：「Fd 由运行时分配全局唯一句柄（非 OS fd），避免重用冲突」；`Resource::Fd(i32)` 中的 i32 **只是示意**（pdr 原文未赋予 i32 语义）。
- 代码：`action.rs::pub type Fd = u64`；`resource.rs::Resource::Fd(Fd)`；`ResourceRegistry::allocate`（单调递增 `next_fd`，永不复用）。
- 结论：决策与 pdr 意图一致；u64 是 i32 示意的合理强化（单调句柄空间更大）。§2.3 的 `i32` 仅为伪代码示意，无冲突。
- **冻结影响：无。**

### D2：Action 递归字段装箱 —— ✅ 一致（工程必然）

- pdr.md §2.1 伪代码为裸递归（`then_branch: Action` 等）；Rust 中 `enum` 直接递归触发 E0072（无限大小）。
- 代码：`action.rs::Action` 全部递归字段为 `Box<Action>`（`then_branch`/`else_branch`/`left`/`right`/`inner`/`target`/`action`/`current`/`on_timeout`）。
- pdr.md §14 示例本身即用 `Box::new(...)`，证明 pdr 认可装箱写法。
- 结论：偏差仅在伪代码字面；D2 是 Rust 语言约束下的必要翻译，不改变 AST 拓扑（结构相似性原则 §1.1 保持）。
- **冻结影响：无。**

### D3：SyscallExecutor 返回 BoxFuture —— ✅ 一致（工程扩展）

- pdr.md §12.3 的 `Runtime` 结构未列 `executor` 字段；但 §12.1 分层架构明确「物理执行层（tokio）」与运行时分离，§2.2 提及"执行器执行"。
- 代码：`syscall.rs::SyscallExecutor` trait，方法返回 `BoxFuture`（`Pin<Box<dyn Future + Send>>`）；`runtime.rs::Runtime` 持有 `Box<dyn SyscallExecutor>`。
- 结论：trait 与 `executor` 字段是 pdr 未细化处的**接口补全**（阶段 0 由 CTO 冻结），与分层架构一致；`async fn` 不 dyn 兼容故需 `BoxFuture`，为工程必然。
- **冻结影响：无。**

### D4：UndoOp 为异步 future —— ✅ 一致

- pdr.md §11（撤销事务）与 §5.1.4（效果函数动态返回逆）；tokio IO 撤销（恢复文件内容、关闭 fd）必然异步。
- 代码：`syscall.rs::UndoOp = Pin<Box<dyn Future<Output=()> + Send>>`；`execute` 返回 `Option<UndoOp>`（仅 Full 策略可逆操作返回 `Some`，对齐 pdr.md §11.2）。
- **冻结影响：无。**

### D5：PipeOpen 用 tokio duplex 实现 —— ⚠️ **偏差（语义降级）**

- pdr.md §2.2 将管道列为 Unix 效应（「覆盖尽可能多的 Unix 系统效应，包括…管道…」），`PipeOpen { flags: PipeFlags }` 隐含 **OS 管道语义**（pipe(2)）：
  - 可继承给子进程（fork/spawn 的 stdio 管道）；
  - 真实阻塞 / O_NONBLOCK 语义；
  - 有 OS fd，可与 select/poll/其他 fd 操作互操作。
- contracts.md D5：改用 **tokio duplex（内存管道）**，理由：跨平台（Windows 无 tokio 封装的 OS pipe）。
- 代码：`resource.rs::ResourceHandle::PipeReader(ReadHalf<DuplexStream>)` / `PipeWriter(WriteHalf<DuplexStream>)`；`PipeFlags { nonblocking }` 在内存管道下**基本失效**（无阻塞语义）。
- 语义差异清单：
  1. 内存管道**不能跨进程**——`Spawn` 子进程无法通过 PipeOpen 获得管道（当前 `DataOp::Spawn` 无 stdio 配置，无直接冲突，但限制了未来扩展）；
  2. 无 OS fd，`Dup`/`Mmap` 等 fd 交互不可用（registry 中 Pipe 句柄是独立变体，不参与通用 fd 操作）；
  3. `nonblocking` 标志无实际语义（内存通道 backpressure 替代）。
- 建议：
  - contracts.md D5 行补充一句语义定义（"内存管道，非 OS 管道；跨进程管道走 Spawn stdio（未来 RFC）"）；
  - 未来可选 RFC：OS pipe feature（仅 Unix）或 `Spawn` 集成 stdio 管道。
- **冻结影响：不阻塞**。`PipeOpen` 的类型形状（`flags → 两个 Fd`）不变；语义以 D5 为合同权威，A5 实现不得假设 OS 管道语义。

### D6：`can_parallel` 保守：Append∥Append 默认串行 —— ✅ 一致

- pdr.md §9.1：「Append|Append ⚠️ 仅当结果顺序无关时并行，否则串行」；§四 A3 注释同。
- 代码：`resource.rs::ResourceRegistry::can_parallel`（默认 `append_order_insensitive=false`）与 `can_parallel_with(..., true)`（显式 opt-in）；已有单测 `append_parallel_needs_opt_in`。
- 结论：contracts 将 pdr 的 "⚠️" 明确为「默认串行 + 显式 opt-in」，方向与 pdr 一致（确定性优先）。
- **冻结影响：无。**

### D7：typestate 包装命名 `TypedResource<M>` —— ✅ 一致（解决 pdr 自身冲突）

- pdr.md §3.2 定义 `struct Resource<M>`，而 §2.3 已定义 `enum Resource`——**pdr 自身存在同名冲突**（同一命名空间无法共存）。
- 代码：`resource.rs::TypedResource<M>`（枚举 `Resource` 保持原名）。
- 结论：D7 是 pdr 内部矛盾的必然消解；类型状态 API 形状（构造器/转换/`into_usage`）与 §3.3/§3.4 一致。
- 建议：pdr.md 修订 §3 命名或加注（A1 可提修订建议，CTO 裁决）。
- **冻结影响：无。**

### D8：`SendFile.in` → `input` —— ✅ 一致（必要改名）

- pdr.md §2.2 用 `in`（Rust **保留字**，无法作字段名）；代码：`action.rs::SendFile { out, input, offset, len }`。
- 结论：必要改名，语义不变。
- **冻结影响：无。**

### D9：Runtime 自持 tokio reactor；`Runtime::new` 须在 tokio 上下文外调用 —— ✅ 一致

- pdr.md §12.3 `Runtime` 含 `reactor: tokio::runtime::Runtime` 字段（自持）；「须在 tokio 上下文外调用」是自持的工程推论（避免嵌套 `Runtime::new` panic，代码中 `expect` 信息已注明）。
- 代码：`runtime.rs::Runtime::new`。
- **冻结影响：无。**

### D10：`Replace` 语义：先 recover 再执行 target —— ✅ 一致（语义补全）

- pdr.md §2.1 `Replace { target }` **未定义执行语义**；pdr.md §8 宏表 `replace!(new_plan)`：「截断当前流 + RAII Drop **放弃所有资源**」。
- 结论：D10（先 recover 撤销当前路径全部效果，再执行 target）与 §8「放弃所有资源」**方向一致**，是 pdr 未细化处的安全补全（默认不泄漏资源）。
- 建议：pdr.md §2.1 补一句语义说明，消除两处（§2.1 与 §8）表述落差。
- **冻结影响：无**（A2 解释器按 D10 实现即可）。

### D11：`Alloc` 返回 `Value::Bytes(vec![0; len])` —— ✅ 一致（确定性澄清）

- pdr.md §2.1 `Alloc { len }` 未定义返回值；§8 `alloc!` → `Box<[u8]> 延迟复制`（未指定初始内容）。
- 结论：D11 明确零初始化，保证确定性（pdr.md 确定性原则）；COW 优化留给 A2。
- **冻结影响：无。**

### D12：路径规范化：词法（绝对化+消除 `.`/`..`），不碰真实 FS —— ⚠️ **偏差（范围收窄）**

- pdr.md §2.3：「Path 必须规范化（绝对路径、**消除 .. 和符号链接**）」。
- contracts.md D12：只做词法规范化（绝对化 + 消除 `.`/`..`），**符号链接解析留给物理执行层**；理由：确定性（符号链接解析需访问真实 FS，破坏纯函数性）。
- 代码：`resource.rs::ResourceRegistry::canonicalize_path(p, cwd)`——纯词法（`Component::CurDir` 丢弃、`ParentDir` pop），已有单测。
- 语义后果（必须记录）：
  1. **路径别名不统一**：`/a/link` 与 `/a/real`（link→real 符号链接）在 `Resource::Path` 层面是**两个不同资源**；
  2. 对 A3 的影响：`can_parallel` 对别名路径可能**漏报冲突**（判定为不相交 → 并行），实际操作同一物理文件——A3 交换律的保证强度因此**低于字面承诺**；
  3. 物理执行层（A5）可在执行时解析符号链接，但**解析结果不回流**到资源标识层（除非 RFC 变更）。
- 建议：
  - 方案 A（推荐）：pdr.md §2.3 修订为「词法规范化（绝对化+消除 `..`）由运行时保证；符号链接解析属物理执行层（决策 D12）」——使 pdr 与合同一致；
  - 方案 B：若坚持物理解析，需引入"规范化后回写标识"机制，破坏确定性且增加 IO 依赖——不推荐。
- **冻结影响：不阻塞**。资源标识语义以 D12 为合同权威；但 A6 的 A3 属性测试必须**按词法标识构造**资源集（避免别名场景误报），并在测试注释中记录该边界。

### D13：ResourceRegistry 实现 Clone（Fork 并行状态隔离） —— ✅ 一致 / ⚠️ **文档缺失**

- 来源：commit `f119797`「契约增强 D13」（CTO 级 unblock，无签名变更）。
- 代码：`resource.rs::#[derive(Default, Clone)] pub struct ResourceRegistry`（`handles` Arc 浅共享 + `consumed` 集合独立）。
- 与 pdr.md 一致性：§9.2「Fork：ReadOnly 共享 Arc、Mutable 延迟复制、Own 独占转移」——registry Clone（句柄 Arc 共享、状态集合隔离）+ `Arc::make_mut`（A2/A3 交付）正是 COW 的工程基础；与公理 A5 一致。
- **发现**：`contracts.md` §3 决策表**只有 D1–D12**，D13 条目缺失（仅存在于 resource.rs 注释与 commit log）。契约文档是 8 Agent 的唯一接口事实来源，缺失条目有被后续 Agent 忽略的风险。
- 建议：CTO 批准后在 contracts.md §3 补录 D13 行（`D13 | ResourceRegistry 实现 Clone | Fork 并行子任务状态隔离，完成后合并（pdr.md §9.2 COW）`）。
- **冻结影响：不阻塞**（实现已冻结，仅文档补录）。

---

## 2. §2 冻结类型列表审计

| contracts.md §2 声明 | 代码实际 | 结论 |
| --- | --- | --- |
| `Fd = u64` | `action.rs::pub type Fd = u64` | ✅ 一致 |
| Action 全部 CPS；递归字段 `Box<Action>` | `action.rs::Action`（9 处 Box 递归字段） | ✅ 一致（D2） |
| `SendFile.in → input` | `action.rs::SendFile { out, input, offset, len }` | ✅ 一致（D8） |
| `TypedResource<M>` | `resource.rs::TypedResource<M>` | ✅ 一致（D7） |
| `Value`：Unit/Bool/U64/I64/Bytes/Str/Fd/Pid/Addr/List | `action.rs::Value`（9 变体，含 `Addr(SocketAddr)`、`List`） | ✅ 一致（pdr §2.1 未穷举，contracts 是澄清） |
| `DataOp`：pdr.md §2.2 **全部 39 个变体** | `action.rs::DataOp` **36 个变体**（与 pdr §2.2 的 36 个一一对应） | ⚠️ **计数错误**：pdr §2.2 与代码均为 36（文件 11 + 目录 3 + TCP 6 + UDP 3 + 管道 1 + 进程 3 + 信号 1 + 内存 2 + 时间 1 + 同步 2 + 其他 3）。contracts.md 的"39"无法由任何一方支持。建议修正为 36。 |
| `SysError`：14 POSIX + `Other(i32)`，含 `from_errno`/`code`/`From<io::Error>` | `error.rs::SysError` + 三个 API 全部存在 | ✅ 一致 |
| `SyscallExecutor`：dyn 兼容 trait（BoxFuture） | `syscall.rs::SyscallExecutor` | ✅ 一致（D3） |
| `UndoOp = Pin<Box<dyn Future<Output=()> + Send>>` | `syscall.rs::UndoOp` | ✅ 一致（D4） |

**附加说明（冻结类型相关，非决策行）**：

- `SeekWhence`：pdr §2.2 用自定义 `SeekWhence`，代码复用 `std::io::SeekFrom`。属实现细节等价，不改变语义；建议 pdr 下次修订对齐或注明（非冻结项）。
- pdr §12.3 `Runtime` 无 `executor` 字段，代码新增（D3 扩展），已在 §1 D3 记录。

---

## 3. 任务指定的特别核查项（汇总）

| 特别核查项 | 结论 | 冻结影响 |
| --- | --- | --- |
| D5：Pipe→tokio duplex vs §2.2 Unix 管道语义 | **偏差（语义降级）**：内存管道、不可跨进程、`nonblocking` 失效 | 不阻塞，需语义记录 |
| D10：Replace=先 recover 再 target | **一致（补全）**：与 pdr §8 `replace!`「放弃所有资源」对齐 | 无 |
| D12：词法路径规范化 vs §2.3「消除符号链接」 | **偏差（范围收窄）**：符号链接别名不统一 → A3 冲突检测对别名路径漏报 | 不阻塞，语义以 D12 为权威 |
| D1：Fd=u64 vs §2.3 i32 示意 | **一致**：i32 仅为示意，u64 强化"全局唯一" | 无 |
| Action 递归字段装箱 vs §2.1 裸递归 | **一致（工程必然）**：E0072；§14 示例本身用 Box | 无 |
| D13：registry Clone | **实现一致 / 文档缺失**：contracts.md 决策表无 D13 条目 | 不阻塞，建议补录 |

---

## 4. 其他一致性发现（非决策行）

1. **pdr §3.2 `Resource<M>` 与 §2.3 `Resource` 枚举同名**——pdr 内部冲突，D7 已消解（§1 D7）。
2. **A3/§9.1 冲突矩阵未覆盖 Read×Append / Append×Read**——代码保守判串行（`can_parallel_with` 的 `_ => false`）；与 pdr 矩阵无矛盾（矩阵未定义，保守方向符合确定性原则）。
3. **A3 交换律隐含 combine 对称性前提**（`a∥b=b∥a` 需 `CombineFn` 对参数交换不变）——pdr 未显式声明；已在 `axioms.md` A3 与 `proofs.md` P2 记录，A6 测试须用对称 combine。
4. **SyscallExecutor 的 `watch_signal`/`invoke` 默认 ENOSYS（errno 38）**——代码已实现默认行为，contracts.md 未提及；属实现细节，不冲突（pdr §17 已知局限框架内）。
5. **contracts.md §6 工作流**与 pdr §19.2/19.3 阶段划分一致（G0–G4 门禁、worktree 流程）；无冲突。

---

## 5. 审计结论与 CTO 行动建议

**总体结论**：contracts.md 与 pdr.md 在**类型与接口层面完全一致**（冻结签名全部得到代码验证）；2 处语义偏差（D5、D12）均为实现策略选择，不触碰冻结类型；契约冻结**可维持**。

**建议 CTO 裁决/执行（按优先级）**：

1. **补录 D13 至 contracts.md §3 决策表**（文档完整性，1 行）。
2. **修正 contracts.md §2 DataOp 变体计数 "39" → "36"**（与 pdr §2.2 及代码一致）。
3. **裁决 D12 偏差**：推荐修订 pdr.md §2.3（词法规范化 + 符号链接属物理层），使 pdr 与合同一致；否则接受偏差记录（本文件 §1 D12 为权威记录）。
4. **记录 D5 语义**：contracts.md D5 行补充"内存管道"定义（可选，非阻塞）。
5. **G2 门禁提醒**：A7/P5 目前无实现载体（`interpret` todo!、`tla/scheduler.tla` 未交付），属 G2 前必须闭环项，非审计阻塞项。

---

## 附：审计证据清单

- `contracts.md`：D1–D12 决策表（§3）、冻结类型列表（§2）、所有权表（§1）。
- `pdr.md` v3.2：§2.1/§2.2/§2.3、§3、§四（A1–A7）、§5.1、§6（P1–P5）、§7、§9、§10、§11、§12.3、§14。
- 代码：`crates/algeff-core/src/{action,error,resource,runtime,syscall,coeffects,virtual_clock,lib}.rs`、`crates/algeff-std/src/executor.rs`。
- 基线验证：`cargo test --workspace` 全绿（11 passed，exit 0），commit `f119797` 前/后均验证。
