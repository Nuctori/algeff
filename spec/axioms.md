# 公理系统 A1–A7（形式化 + 工程映射）

> 拥有者：A1（Spec Guardian）。状态：**随契约冻结，只读**；修订走 RFC → CTO 裁决。
> 源规范：`pdr.md` §四（v3.2）。工程对照：`contracts.md` §2/§3、`crates/algeff-core/src/`（worktree `.wt/a1` @ `f119797`）。
> 本文件的「验证方式」为建议测试名，供 A6 在 `crates/algeff-core/tests/axioms.rs` 落地（契约 §5 A6 任务）。

## 记号（pdr.md §4.1）

- `1` = `Pure(())`（`crates/algeff-core/src/action.rs::unit()`）。
- `a ; b` = 顺序组合（`Action::Sequential`）。
- `a ∥ b` = 并行组合（`Action::Fork`）。
- `Δ(a)` = `a` 涉及资源键集合（`Action::Syscall.resources: ResourceSet`，`resource.rs`）。
- `w̄` = `w` 的逆操作（撤销；`syscall.rs::UndoOp`）。
- 等号 `=` 为**执行语义等价**（作用于上下文 Γ 的状态变换相同，pdr.md §5.1.1），非 AST 语法相等——Action 含闭包，语法不可比。

---

## A1 结合律（顺序组合）

**形式化陈述**

```
∀ a, b, c.  (a ; b) ; c = a ; (b ; c)
```

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| 类型 | `crates/algeff-core/src/action.rs::Action::Sequential { current: Box<Action>, next: NextFn }` | 顺序组合的载体；CPS 下 `next` 本身就是"后继动作" |
| 语义 | `crates/algeff-core/src/runtime.rs::interpret`（A2 交付） | 解释器保持 AST 结构；`Sequential` 展开为"先 current 后 next"，结合律由解释顺序保证 |
| 契约 | `contracts.md` §2 类型冻结 / §5 A2 任务 | `interpret` 逐节点执行 |

**验证方式（A6 建议）**

- `axiom_a1_associativity`：构造 `(a;b);c` 与 `a;(b;c)` 两条蓝图（a/b/c 用记录型 DataOp 序列，如 `Write`/`Seek`/`Read`），在 trace 记录型 executor 上执行，断言两边产生的 DataOp 序列与资源状态变换一致（proptest 参数化 DataOp 组合）。
- `axiom_a1_sequential_ast_shape`：静态断言 `Sequential{current: Sequential{..}, next}` 可扁平化，解释器不改变嵌套深度语义。

**风险备注**

- 闭包 `NextFn` 使语法级相等不可判定，测试只能验证**可观察语义**（trace / 状态）。A6 需在测试中固定"可观察量"（操作序列 + 最终 Γ），否则测试无意义。
- 结合律是 P1（幺半群）的支柱；若解释器在 `Sequential` 上做扁平化优化，必须保持 trace 顺序不变（pdr.md §六 P1 工程含义：编译期恒等变换）。

---

## A2 单位元

**形式化陈述**

```
∀ a.  1 ; a = a    且    a ; 1 = a
```

其中 `1 = Pure(Unit)`。

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| 类型 | `action.rs::Action::Pure(Value::Unit)`、`action.rs::unit()` | 单位元构造 |
| 语义 | `runtime.rs::interpret` | `Pure` 节点直接被解释器求值为值并跳转到 `next`，不产生任何效果（"直接跳过"） |
| 契约 | `contracts.md` §2 `Value` 冻结 / pdr.md §四 A2 | — |

**验证方式（A6 建议）**

- `axiom_a2_left_identity`：`unit().and_then(a)`（`Sequential{Pure(Unit), a}`）与单独 `a` 的执行 trace 相同。
- `axiom_a2_right_identity`：`a` 以 `unit()` 收尾（`next = |_| Pure(Unit)`）与单独 `a` 相同。
- `axiom_a2_pure_no_effect`：`Pure` 不触碰 registry / UndoStack / context（断言三者状态不变）。

**风险备注**

- `Pure` 若被解释器误当作可撤销操作压栈，会破坏 A6 的撤销双态（撤销栈多出空操作，LIFO 顺序仍对，但 `recover` 会多执行一个 no-op）。测试应断言 `Pure` 不产生 `UndoOp`。
- `Value::Unit` 之外的 `Pure(v)` 不是单位元，测试只针对 `Unit`。

---

## A3 交换律（并行组合）

**形式化陈述**

```
∀ a, b.  Δ(a) ∩ Δ(b) = ∅  ∧  不存在 Write/Own 模式重叠
      ⇒  a ∥ b = b ∥ a
```

注意（pdr.md §四 A3 原文）：Append 并行虽然 OS 保证原子追加，但追加顺序不确定，违反确定性原则；Append∥Append 仅在结果对顺序不敏感时允许，否则降级顺序执行。

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| 冲突判定 | `crates/algeff-core/src/resource.rs::ResourceRegistry::can_parallel` / `can_parallel_with(a, b, append_order_insensitive)` | 冲突矩阵（pdr.md §9.1）载体；保守默认 `can_parallel` 下 Append∥Append 返回 false（决策 D6） |
| 并行调度 | `runtime.rs::interpret`（A2 交付） | `Fork` 节点：`can_parallel` 为真 → `tokio::spawn` 并行；为假 → 顺序降级 |
| 句柄分配 | `resource.rs::ResourceRegistry::allocate` | 全局唯一句柄（决策 D1），保证不相交资源确实不冲突 |
| 契约 | `contracts.md` D6 / pdr.md §9.1 / §四 A3 | — |

**验证方式（A6 建议）**

- `axiom_a3_parallel_commutes_disjoint`：proptest 生成不相交 `ResourceSet` 的两组操作（含 Write/Own），`left∥right` 与 `right∥left`（combine 为对称函数）执行后资源状态与结果值一致。
- `axiom_a3_conflict_matrix_exhaustive`：穷举 §9.1 矩阵（Read×Read 并行；Read×Write、Write×Write、Own×任意 串行；Append×Append 需 opt-in），断言 `can_parallel_with` 输出。
- `axiom_a3_append_order_insensitive_optin`：`can_parallel_with(a,b,true)` 才允许 Append∥Append；解释器仅在显式声明顺序无关时并行（D6）。
- 模型检测：`tla/scheduler.tla`（A6）中验证并行调度等价性。

**风险备注**

- **隐含前提（本审计新增）**：`a ∥ b = b ∥ a` 还要求 `CombineFn` 对两个 `Value` 参数交换不变（或结果顺序无关）。pdr.md 未显式声明；若 combine 非对称（如 `(x,y) → x`），并行顺序会改变结果值。**建议 A6 测试统一用对称 combine**，并在本文件记录该前提（见 `proofs.md` P2）。
- 冲突判定基于 `Resource::Path` 的**词法规范化**标识（D12），符号链接别名可能造成漏报（false parallel）→ A3 保证的强度取决于 D12 语义，见 `contracts-audit.md` D12 条目。

---

## A4 资源线性

**形式化陈述**

```
∀ a, r ∈ Δ(a)：
    mode(r) ∈ {Write, Own}  ⇒  r 在 a 的执行路径中被恰好消费一次
    mode(r) ∈ {Read, Append} ⇒  r 不被消费（可重复）
```

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| 运行时检查 | `resource.rs::ResourceRegistry::check_linear(&mut self, usage) -> Result<(), SysError>` | Write/Own 登记 `consumed: HashSet<Resource>`；重复消费返回 `SysError::InvalidInput` |
| 物理释放 | `resource.rs::ResourceHandle`（全 `Arc`）+ Rust 所有权 | pdr.md §1.2 物理层：Drop 保证释放；`take`（Own 语义：Close/替换） |
| API 辅助 | `resource.rs::TypedResource<Owned>::new_owned` | Owned 不能降级为 Read/Write，防意外共享（pdr.md §3.3） |
| 契约 | `contracts.md` §2 类型冻结 / pdr.md §四 A4、§九 | — |

**验证方式（A6 建议）**

- `axiom_a4_double_write_rejected`（已存在于 `resource.rs` 单测 `linearity_double_write_rejected`，升级为属性测试）：同一 Write 资源二次 `check_linear` 报 `InvalidInput`。
- `axiom_a4_read_repeatable`（已存在 `linearity_read_repeatable`）：Read 资源可重复检查。
- `axiom_a4_own_exactly_once_along_path`：蓝图路径上 Own 资源恰好 `take` 一次；`Scope`/`Catch`/`Replace` 各路径分支下仍恰好一次（proptest 枚举分支）。

**风险备注**

- `check_linear` 目前是**执行时断言**（非编译期线性类型），用户绕过 `TypedResource` 直接构造 `ResourceUsage` 可破坏 A4——pdr.md §3.5 / §18 明示这是用户责任。A6 测试应覆盖"绕过即破坏"的边界文档化，而非假设 A4 无条件成立。
- 消费登记在**当前执行路径**上（Fork 子任务用 registry Clone 隔离，D13），合并回主 registry 时需合并 `consumed`，否则父路径重复消费误报/漏报（A2/A3 实现注意）。

---

## A5 分支隔离

**形式化陈述**

```
Choose：左分支的 Write 不影响右分支的 Read（两分支均从分支点上下文 Γ₀ 出发）
Fork  ：子任务通过 COW 隔离（ReadOnly 共享、Mutable 延迟复制、Own 独占转移）
```

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| Choose | `action.rs::Action::Choose { cond, then_branch, else_branch }` + `runtime.rs::interpret` | 只执行被选分支；未选分支不执行，天然无相互影响 |
| Fork COW | `resource.rs::ResourceRegistry` 实现 `Clone`（决策 D13）+ `ResourceHandle` 全 `Arc` | `registry.clone()` 隔离子任务状态；`Arc::make_mut` 实现延迟复制（pdr.md §9.2，A2/A3 交付） |
| 契约 | `contracts.md` D13（见 audit：未入决策表，建议补录）/ pdr.md §四 A5、§9.2 | — |

**验证方式（A6 建议）**

- `axiom_a5_choose_write_isolation`：Choose 左分支 Write 文件、右分支 Read 同文件；断言右分支读到写前内容（且左分支不执行时右分支结果不受左分支蓝图影响）。
- `axiom_a5_fork_cow_isolation`：Fork 两分支共享 `Arc<[u8]>`，左分支写（触发 `make_mut`），断言右分支读到的仍是原数据。
- `axiom_a5_fork_own_exclusive`：Own 资源仅允许一个分支持有（另一个分支获得错误或独占转移）。
- 并发测试（loom）：`loom` 下验证 COW 竞态（pdr.md §七 A5 验证方式）。

**风险备注**

- 当前 `ResourceHandle` 的所有变体已 `Arc` 共享（含 PipeReader/PipeWriter 等），这是 COW 前提；但**副本触发机制（`make_mut`）尚未实现**（A2/A3 交付）。A5 的测试在解释器落地前无法全量运行。
- registry Clone 是**浅拷贝**：`handles` 的 Arc 指针共享、`consumed` 集合独立。这与 §9.2「ReadOnly 共享、Mutable 延迟复制」一致；但若子任务对共享句柄做破坏性操作（如 Close），会经 Arc 影响父任务——**Own 语义必须用 `take` + 独占转移**，A6 应有测试覆盖"子任务 Close 共享句柄"的拒绝路径。

---

## A6 撤销双态条件

**形式化陈述**

```
∀ 可逆操作 w，∃ 逆操作 w̄：  w ; w̄ = 1   （资源状态恢复至执行前）
不可逆操作（UDP 发送、进程信号等）仅提供补偿挂钩，不满足该公理
```

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| 逆操作类型 | `crates/algeff-core/src/syscall.rs::UndoOp = Pin<Box<dyn Future<Output=()> + Send>>` | 异步逆操作（决策 D4，tokio IO 撤销必然异步） |
| 逆操作来源 | `syscall.rs::SyscallExecutor::execute -> Result<(Value, Option<UndoOp>), SysError>` | 只有可逆操作（撤销策略 Full，pdr.md §11.2）返回 `Some(undo)` |
| 撤销栈 | `runtime.rs::UndoStack::push` / `recover`（LIFO） | trackΓ：逆操作压栈；recoverΓ：逆序执行 |
| 运行时入口 | `runtime.rs::Runtime::recover` | 执行全部累积逆操作（pdr.md §5.1.3） |
| 工程语义 | pdr.md §5.1.4 Effect Function：`e(γ) = (δ, g)` 且 `g(δ) = γ` | 执行时动态返回逆 |
| 契约 | `contracts.md` D4 / pdr.md §四 A6、§11.2 | Full 策略才满足 A6；BestEffort/Skip 不满足 |

**验证方式（A6 建议）**

- `axiom_a6_undo_roundtrip_write_file`：Write 文件 → `recover` → 文件内容恢复（Full 策略；`algeff-std/tests/` 文件往返，A5 交付）。
- `axiom_a6_undo_roundtrip_open_close`：Open → `recover` → fd 关闭、registry 无句柄。
- `axiom_a6_undo_lifo_order`：多操作（Open→Write→Seek）撤销后状态精确复原，断言 LIFO 顺序（逆序执行）正确。
- `axiom_a6_irreversible_compensation_hook`：`UdpSendTo`/`Kill`/`SendSignal` 执行返回 `None`（无逆），仅允许补偿挂钩（pdr.md §11.1）——断言 execute 返回 `None` 且不产生 UndoOp。
- `axiom_a6_scope_undo_component`：`Scope` 卸载时撤销上下文（cwd 恢复），pdr.md §5.1.3 可逆性定理的复合验证。

**风险备注**

- A6 只在 **Full 撤销策略**下成立（pdr.md §11.2）；BestEffort/Skip 是显式例外，A6 测试必须按策略分级标记（`#[ignore]` 或 feature 门控），否则大文件写入场景会误报。
- 撤销操作的**原子性**由用户承诺（补偿闭包正确性），pdr.md §17 已知局限；A6 只能测框架侧的"逆存在且 LIFO 执行"，不能测用户补偿逻辑的正确性。

---

## A7 无死锁调度

**形式化陈述**

```
动态资源获取采用：原子占坑（atomic placeholder）+ 失败回滚 + 有限重试（bounded retry）
⇒ 不存在任务互相等待资源形成的循环等待链（circular wait chain）
```

**工程实现位置**

| 层 | 文件 : 符号 | 说明 |
| --- | --- | --- |
| 冲突判定（静态降级） | `resource.rs::ResourceRegistry::can_parallel_with` | 静态冲突 → 顺序执行（P5 机制 1，无等待） |
| 占坑/回滚（动态） | `runtime.rs::interpret` 的 Fork/动态资源路径（A2 交付）+ `resource.rs::ResourceRegistry`（A3 交付） | 原子占坑 + 失败回滚 + 有限重试；**当前骨架未实现**（`interpret` 为 `todo!`） |
| 模型 | `tla/scheduler.tla`（A6 交付，契约 §5） | 原子占坑+回滚重试，无循环等待的模型验证 |
| 契约 | `contracts.md` §5 A2/A6 任务 / pdr.md §四 A7、§六 P5 | — |

**验证方式（A6 建议）**

- `axiom_a7_no_circular_wait_tla`：`tla/scheduler.tla` + Apalache 模型检测：所有可达状态无"持有-等待"环（Coffman 条件 4 不成立）。
- `axiom_a7_bounded_retry_progress`：N 任务竞争 M 资源（M < N）压力测试：断言全部任务在有限步内完成、重试次数有上界、无任务永久阻塞。
- `axiom_a7_static_conflict_serialized`：`can_parallel=false` 的两任务被顺序调度（无锁等待路径）。

**风险备注**

- A7 是**当前契约中唯一无实现载体**的公理（`interpret` 未实现、`tla/scheduler.tla` 未交付）。冻结状态只冻结了接口，不冻结"公理已被证明"——G2 门禁前 A2/A3/A6 必须闭环。
- 无死锁 ≠ 无活锁：有限重试保证"重试次数有界"，但若每次重试都因同伴抢占而失败，任务仍会最终报错（失败回滚上抛）。pdr.md §十七"动态资源仲裁的锁竞争"是已知工程缓解项。
- 建议实现时对资源采用**全序获取**（按资源键排序占坑），可从结构上消除循环等待（见 `proofs.md` P5 的补充论证）。

---

## 附：公理 → 契约/代码对照总表

| 公理 | 主载体（文件 : 函数） | 契约引用 | 当前状态 |
| --- | --- | --- | --- |
| A1 | `runtime.rs::interpret`（Sequential） | §2 Action / §5 A2 | 骨架（todo!） |
| A2 | `action.rs::Action::Pure` + `interpret` | §2 Value | 骨架（todo!） |
| A3 | `resource.rs::ResourceRegistry::can_parallel(_with)` | D6 / §9.1 | ✅ 已实现+单测 |
| A4 | `resource.rs::ResourceRegistry::check_linear` | §2 / §四 A4 | ✅ 已实现+单测 |
| A5 | `resource.rs::ResourceRegistry::clone`（D13）+ `ResourceHandle`(Arc) | D13（待补录）/ §9.2 | 部分（clone 有，make_mut 待 A2/A3） |
| A6 | `syscall.rs::UndoOp`/`SyscallExecutor::execute` + `runtime.rs::UndoStack` | D4 / §11.2 | 类型就绪，解释器待 A2 |
| A7 | `tla/scheduler.tla` + `interpret` 动态资源路径 | §5 A2/A6 | ❌ 未实现（最大风险项） |
