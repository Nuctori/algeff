# 形式化命题与证明 P1–P5

> 拥有者：A1（Spec Guardian）。源规范：`pdr.md` §六（v3.2）。
> 证明建立在 `spec/axioms.md` 的公理 A1–A7 之上。每条附「Rust 测试映射」建议，供 A6 落地。
> 等号 `=` 均为**执行语义等价**（状态变换等价，见 `axioms.md` 记号）。

---

## P1：Action 组合形成幺半群

**陈述**：`(Action, ;, 1)` 构成幺半群。

**证明**

幺半群 = 集合 + 结合二元运算 + 单位元。

1. **封闭性**：`;` 是 `Action × Action → Action` 的全函数（`Action::Sequential { current, next }` 对任意两个 Action 可构造），故 `Action` 在 `;` 下封闭。
2. **结合律**：由公理 A1（`(a;b);c = a;(b;c)`）直接给出。
3. **单位元**：由公理 A2（`1;a = a` 且 `a;1 = a`，`1 = Pure(Unit)`）直接给出。

故 `(Action, ;, 1)` 满足幺半群三条公理。∎

**说明（证明的前提与边界）**

- 等价是在**执行语义**层面（诱导的状态变换 / 可观察 trace），而非 AST 语法层面。Action 含 `NextFn` 闭包，语法相等不可判定；幺半群律陈述的是"语义行为等价"。
- 工程含义（pdr.md §六）：宏（`plan!`，A4）可在编译期做恒等变换优化（如 `Sequential(Pure(Unit), a)` → `a`）而不改变程序语义——前提是解释器保持 trace 顺序。

**Rust 测试映射（A6 建议）**

| 测试名 | 位置 | 断言 |
| --- | --- | --- |
| `prop_p1_monoid_associativity` | `crates/algeff-core/tests/axioms.rs`（proptest） | `(a;b);c` 与 `a;(b;c)` trace 相同（= `axiom_a1_associativity`） |
| `prop_p1_monoid_identity` | 同上 | `1;a`、`a;1` 与 `a` trace 相同（= `axiom_a2_*_identity`） |
| `prop_p1_plan_flatten_equiv` | `crates/algeff-macro/tests/macros.rs` | `plan!{a;b;c}` 展开与手写嵌套 `Sequential` 语义等价 |

---

## P2：资源不相交的并行组合满足交换律

**陈述**：若 `Δ(a) ∩ Δ(b) = ∅` 且无 Write/Own 重叠，则 `a ∥ b = b ∥ a`。

**证明**

设 `Δ(a) ∩ Δ(b) = ∅`，且两操作无 Write/Own 重叠。

**引理（交错可交换）**：对不相交资源的两组操作，`a` 与 `b` 的任意执行交错（interleaving）产生的资源状态变换相同。

- 证明：`a` 的每个效果只读写 `Δ(a)` 中的资源，`b` 的每个效果只读写 `Δ(b)` 中的资源；因 `Δ(a) ∩ Δ(b) = ∅`，两组的读写集不相交，任何两个效果（分属两组）相互独立（先执行谁，结果相同）。由归纳可得引理。∎（引理）

1. 并行组合 `a ∥ b` 的调度集合 = `a`、`b` 效果的全部交错；`b ∥ a` 的调度集合 = 同样集合（只是分支位置互换）。由引理，两组交错产生相同的资源状态变换。
2. 因此，在 **combine 对称**（`CombineFn(x, y)` 对 `x,y` 交换不变，或结果顺序无关）的前提下，`a ∥ b` 与 `b ∥ a` 产生相同的语义。这正是公理 A3 的内容。
3. 由公理 A3 直接成立。∎

**依赖的前提**：A3；以及 combine 对称性（`axioms.md` A3 风险备注中记录的本审计新增前提）。

**工程含义**：运行时可根据冲突矩阵（`ResourceRegistry::can_parallel`）**零锁并行调度**；Append∥Append 需显式声明顺序无关（决策 D6）后才允许并行。

**Rust 测试映射（A6 建议）**

| 测试名 | 位置 | 断言 |
| --- | --- | --- |
| `prop_p2_parallel_commutes_disjoint` | `crates/algeff-core/tests/axioms.rs`（proptest，对称 combine） | 不相交资源时 `left∥right` 与 `right∥left` 结果值与最终状态一致 |
| `prop_p2_conflict_matrix` | 同上 | `can_parallel_with` 对 §9.1 矩阵全组合的输出与期望一致（= `axiom_a3_conflict_matrix_exhaustive`） |
| `prop_p2_combine_asymmetry_detected` | 同上 | 非对称 combine（如取第一参数）时结果不同——**记录该反例**，证明"交换律需要对称 combine"的边界 |

---

## P3：分支写隔离

**陈述**：Choose 分支中，左分支的 Write 不影响右分支的 Read；Fork 子任务通过 COW 隔离。

**证明**

**Choose 情形**：Choose 的解释语义（`runtime.rs::interpret`）是求值 `cond` 后**恰好执行一个分支**。两分支的求值上下文都从分支点上下文 `Γ₀` 派生：

- 左分支的任何 Write 修改的是左分支执行路径上的上下文副本/registry 状态；
- 右分支从 `Γ₀` 出发执行，其 Read 观察到的是 `Γ₀` 下的资源状态。

若右分支读取到左分支写入的内容，则意味着右分支的执行路径与左分支共享了状态——与"分支上下文从 `Γ₀` 派生、互不写入对方"的解释语义矛盾。由公理 A5 保证。∎（Choose）

**Fork 情形**：Fork 时每个子任务获得 registry 的**隔离副本**（决策 D13：`ResourceRegistry: Clone`），句柄本体为 `Arc` 共享（`resource.rs::ResourceHandle`）：

- **ReadOnly**：两个分支共享同一 `Arc`（零拷贝，pdr.md §9.2）；
- **Mutable**：共享 `Arc` 句柄，首次写入触发 `Arc::make_mut` 产生私有副本（延迟复制）——写入发生在副本上，兄弟分支的 Read 仍指向原数据；
- **Own**：所有权转移，仅一个分支持有。

因此任何子任务的 Write 都作用于私有副本，不可能影响兄弟分支的 Read。∎（Fork）

**工程实现链**：`ResourceRegistry::clone`（D13，已冻结）→ `Arc::make_mut`（A2/A3 交付）→ `Action::Fork` 解释（A2 交付）。

**Rust 测试映射（A6 建议）**

| 测试名 | 位置 | 断言 |
| --- | --- | --- |
| `prop_p3_choose_write_isolation` | `crates/algeff-core/tests/axioms.rs` | Choose 左 Write / 右 Read 同资源，右分支读到写前内容（= `axiom_a5_choose_write_isolation`） |
| `prop_p3_fork_cow_isolation` | 同上（loom 并发） | 左分支 `make_mut` 写后，右分支 `Arc` 内容不变（= `axiom_a5_fork_cow_isolation`） |
| `prop_p3_fork_own_exclusive` | 同上 | Own 资源仅一个分支持有，另一分支报错（= `axiom_a5_fork_own_exclusive`） |

**边界与风险**：当前冻结代码只有 registry `Clone`，`make_mut` 的**副本触发时机**尚未实现（`interpret` 为 `todo!`）；P3 的 Fork 部分在 A2/A3 落地前无法端到端验证。另注意：子任务若通过 Arc 对共享句柄做破坏性操作（如 Close），会绕过 COW——Own 语义必须走 `take` + 独占转移（见 `axioms.md` A5 风险备注）。

---

## P4：可逆操作满足撤销双态

**陈述**：对于可逆操作 `w`，执行 `w` 后执行 `w̄`，资源状态恢复至执行前。

**证明**

由公理 A6：`w ; w̄ = 1`（`1` = 恒等状态变换）。

运行时机制（pdr.md §5.1）：

- **trackΓ**（§5.1.2）：执行 `w` 时，效果函数 `e(γ) = (δ, g)`（§5.1.4）把正变换 `f` 应用到当前状态 `γ`，并把逆变换 `g` 复合进累积逆变换 `φ ← φ ∘ g`，同时将逆操作压入 `UndoStack`（`runtime.rs::UndoStack::push`）。
- **recoverΓ**（§5.1.3）：`recover(γ, φ) = (φ(γ), id_Γ)`，按 LIFO 顺序执行全部逆操作（`UndoStack::recover`）。

对单个可逆操作 `w`：

```
track(w, w̄)(γ₀, id) = (w(γ₀), id ∘ w̄)          （trackΓ 定义）
recover ∘ track(w, w̄)(γ₀, id) = (w̄(w(γ₀)), id)  （recoverΓ 定义）
                               = ((w ; w̄)(γ₀), id)
                               = (γ₀, id)          （A6：w;w̄ = 1）
```

资源状态恢复至执行前。∎

**复合情形（可逆性定理，pdr.md §5.1.3）**：对操作序列 `w₁…wₙ`，LIFO 撤销保证

```
recover ∘ track(wₙ) ∘ ⋯ ∘ track(w₁)(γ₀, id) = (γ₀, id)
```

（逆序应用 `w̄₁ ∘ ⋯ ∘ w̄ₙ` 与正序 `wₙ ∘ ⋯ ∘ w₁` 抵消；结合律 A1 保证分组无关。）

**适用范围边界**：只有撤销策略 **Full**（pdr.md §11.2）的操作返回逆操作（`SyscallExecutor::execute -> Option<UndoOp>` 为 `Some` 时才可撤销）；BestEffort/Skip 不满足 A6，不在此命题范围内。不可逆操作（UDP 发送、进程信号）仅提供补偿挂钩。

**Rust 测试映射（A6 建议）**

| 测试名 | 位置 | 断言 |
| --- | --- | --- |
| `prop_p4_undo_single_roundtrip` | `crates/algeff-core/tests/axioms.rs` + `algeff-std/tests/` | Write 文件 → `Runtime::recover` → 内容恢复（= `axiom_a6_undo_roundtrip_write_file`） |
| `prop_p4_undo_lifo_composite` | 同上 | Open→Write→Seek 撤销后精确复原（= `axiom_a6_undo_lifo_order`） |
| `prop_p4_recover_returns_id_context` | 同上 | recover 后 `Context`（cwd/env）与执行前相等 |
| `prop_p4_irreversible_no_undo` | 同上 | `UdpSendTo`/`Kill`/`SendSignal` 返回 `None`（= `axiom_a6_irreversible_compensation_hook`） |

---

## P5：Algeff 调度器无死锁

**陈述**：不存在任务互相等待资源形成的循环等待链（circular wait chain）。

**证明**：Algeff 的调度采用**双机制**，分别对应静态与动态两类资源冲突。死锁的四个必要条件（Coffman）中，本设计从结构上破坏条件 4（循环等待）与条件 2（持有并等待）。

**机制 1：静态冲突降级为顺序执行（无等待路径）**

对于蓝图可静态判定的冲突（`ResourceRegistry::can_parallel = false`，含 Write×Write、Own×任意、默认的 Append×Append），解释器**不并行**，将任务串行调度（`runtime.rs::interpret` 的 Fork 分支）：

- 串行执行是线性序，任务间不存在"等待资源"状态——**循环等待链无从构造**。
- 该路径由决策 D6 与 §9.1 冲突矩阵保证，与公理 A3 的保守方向一致。

**机制 2：动态资源获取 = 原子占坑 + 失败回滚 + 有限重试（公理 A7）**

对运行期才确定的资源竞争（如 `Fork` 子任务在解释期动态申请资源），采用：

1. **原子占坑（atomic placeholder）**：任务一次性声明所需的全部资源占坑（对 `ResourceRegistry` 的登记操作是原子的、全有或全无）。占坑成功前，任务**不持有任何其他资源锁**；占坑失败（有任务已占该坑）则立即回滚本次占坑产生的所有登记。
2. **失败回滚（rollback）**：占坑失败的登记全部撤销，任务不残留任何占坑状态。
3. **有限重试（bounded retry）**：重试次数有上界 `B`；超过 `B` 次仍失败则向上返回错误（不无限等待）。

论证无循环等待：假设存在循环等待链 `T₁ → T₂ → … → Tₖ → T₁`（`Tᵢ` 持有 `Tᵢ₊₁` 需要的资源）。由机制 2 的**原子占坑**，`Tᵢ` 占坑时一次性获得其全部资源；若某个资源被 `Tᵢ₊₁` 持有，`Tᵢ` 的占坑**整体失败并回滚**，不可能处于"持有部分资源、等待其余资源"的状态。因此"持有并等待"（Coffman 条件 2）在动态路径上不成立——**循环等待链不可能存在**。∎

**补充论证（实现建议，加强结构性保证）**：若实现时对资源采用**全序占坑**（按资源键 `Resource` 的规范序排序后再原子登记），则即使未来引入部分占坑，也不存在环（全序下等待关系是偏序，无环）。建议 A2/A3 实现时采纳；当前契约不强制。

**与 A7 的关系**：P5 的动态部分正是公理 A7 的陈述本身（原子占坑 + 失败回滚 + 有限重试）；静态部分由 D6/§9.1 补充。**P5 不是独立新假设，而是把 A7 与静态降级策略组合后的工程结论。**

**边界与风险**

- 无死锁 ≠ 无活锁：有限重试只保证重试次数有界，不保证一定成功；竞争激烈时任务以错误终止（回滚上抛）——这是有意的设计（pdr.md §十七"动态资源仲裁的锁竞争"）。
- A7/P5 目前**无实现载体**（`interpret` 为 `todo!`，`tla/scheduler.tla` 未交付）。G2 门禁（契约 §4）前必须由 A2 实现占坑/回滚/重试、A6 完成模型检测。
- 用户隐瞒依赖（不声明 `ResourceSet`）可导致 `can_parallel` 误判为可并行——由 pdr.md §18 划为用户责任，不在此命题保证范围内。

**Rust 测试映射（A6 建议）**

| 测试名 | 位置 | 断言 |
| --- | --- | --- |
| `prop_p5_static_conflict_serialized` | `crates/algeff-core/tests/axioms.rs` | `can_parallel=false` 的两 Fork 分支被顺序执行（= `axiom_a7_static_conflict_serialized`） |
| `prop_p5_bounded_retry_all_complete` | 同上（压力测试：N 任务 / M 资源，M < N） | 全部任务在有限步内完成或返回错误；重试次数 ≤ B；无任务永久挂起（= `axiom_a7_bounded_retry_progress`） |
| `prop_p5_placeholder_rollback_no_leak` | 同上 | 占坑失败后 registry 无残留占坑登记（回滚完整性） |
| `prop_p5_no_circular_wait` | `tla/scheduler.tla` + Apalache | 模型检测：所有可达状态无"持有-等待"环（= `axiom_a7_no_circular_wait_tla`） |

---

## 附：命题依赖关系图

```
A1 + A2 ──► P1（幺半群）
A3 ────────► P2（并行交换律）
A5 ────────► P3（分支写隔离）
A6 ────────► P4（撤销双态）
A7 + D6/§9.1 ──► P5（无死锁）
```

- P1–P4 各自直接依赖单条（组）公理，无交叉依赖。
- P5 依赖 A7（动态）与 §9.1/D6（静态），是唯一需要"公理 + 工程决策"共同支撑的命题。
