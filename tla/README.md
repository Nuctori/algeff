# Algeff 调度器模型（tla/）

> 拥有者：A6 Verification（pdr.md §19.1）。交付物：`scheduler.tla`。
> 对应公理：**A7 无死锁调度**（pdr.md §四 A7）；工具链见 pdr.md §19.4（模型检测 TLA+/Apalache）。

## 1. 模型目的

pdr.md §四 A7：*动态资源获取采用原子占坑 + 失败回滚 + 有限重试，不存在循环等待链。*

本模型将 Algeff 的运行时资源调度抽象为有限状态系统，验证该策略确实满足
「无循环等待」不变式，并在公平调度下每个任务都能终止（成功 `done` 或重试耗尽 `failed`）。

## 2. 模型设计（`scheduler.tla`）

### 2.1 常量与状态变量

| 名称 | 含义 |
| --- | --- |
| `R` | 资源集合（有限） |
| `T` | 任务集合（有限） |
| `requested[t]` | 任务 t 所需资源集（⊆ R） |
| `MaxRetries` | 有限重试上限（∈ Nat） |
| `holders[r]` | 持有资源 r 的任务集（不变式保证 `|holders[r]| ≤ 1`） |
| `retries[t]` | 任务 t 累计失败尝试次数 |
| `status[t]` | `idle` / `running` / `done` / `failed` |

### 2.2 动作

- `Claim(t)`：**原子占坑成功**。`requested[t]` 全部空闲 → 一举持有全部资源，`status := running`。
  占坑是单个原子动作，不存在「持有部分资源」的中间状态。
- `ClaimFail(t)`：**占坑失败**。任一所需资源被占 → 原子性保证未持有任何资源（即「失败回滚」，
  无需部分释放），`retries[t] := retries[t] + 1`；达到 `MaxRetries` → 永久 `failed`（abort），
  否则回到 `idle`（**有限重试**）。
- `Finish(t)`：执行完毕，释放全部占坑，`status := done`。

`Next` 为上述动作的任一任务实例；`Spec == Init /\ [][Next]_vars`（公平性不进 Spec，
而是作为活性属性的前提，便于 Apalache 只查不变式）。

### 2.3 不变式与活性

- `TypeOK`：状态变量类型正确。
- `ExclusiveHold`：互斥——任一资源至多被一个任务持有。
- `ExactHold`：运行中的任务恰好持有其所需资源集（不多不少）。
- `NoCircularWait`：**waits-for 图上无环**。等待关系定义在通用图上：
  `t` 等待 `u` 当且仅当 `u` 持有 `t` 所需的某个资源（排除自等待）。
  环的等价刻画：不存在非空任务子集 S，其中每个任务都等待 S 内另一任务。
- `Progress`（活性，需 TLC + 弱公平 WF）：每个任务最终 `running`（随后 `done`）
  或 `failed`，不会永久停滞。

### 2.4 为什么该模型成立（论证）

由于 `Claim` 原子，任何可达状态中任务要么**持有其全部所需资源**（running），
要么**什么都不持有**（idle/done/failed）。因此 waits-for 图的边只可能从
「未持有任何资源的任务」指向「运行中的任务」，而运行中任务不再等待任何资源
（出度为零）——有向图不可能形成环。`NoCircularWait` 恒成立。

**不变式的非平凡性**：`NoCircularWait` 的定义不依赖原子性假设。若实现退化为
「逐资源增量占坑 + 持久持有」（任务 A 占 r1 等 r2，任务 B 占 r2 等 r1），
waits-for 图将出现 2-环，该不变式会被 TLC 立即发现。这正是本模型要持续守住的断言，
也是「原子占坑」策略相对朴素增量占坑的本质优势。

## 3. 用 TLC 检查

前置：JDK + `tla2tools.jar`（https://github.com/tlaplus/tlaplus/releases，`java -version` 可验证）。

在同目录创建 `scheduler.cfg`：

```ini
SPECIFICATION Spec
INVARIANT TypeOK
INVARIANT ExclusiveHold
INVARIANT ExactHold
INVARIANT NoCircularWait
PROPERTY Progress
CONSTANT
  R = {r1, r2}
  T = {t1, t2, t3}
  requested = t1 :> {r1} @@ t2 :> {r2} @@ t3 :> {r1, r2}
  MaxRetries = 3
```

运行：

```bash
java -jar tla2tools.jar -config scheduler.cfg scheduler.tla
```

预期输出（示例）：

```
Model checking completed. No error has been found.
  Invariants checked: 4
  Temporal properties checked: 1
```

可调整 `R/T/requested/MaxRetries` 构造压力场景（如多任务争用同一资源、菱形依赖），
TLC 会穷举所有可达状态验证不变式；`Progress` 的检查需要 Spec 之外的公平性前提
（已内置于属性本身），TLC 自动处理。

> 提示：`MaxRetries = 0` 表示任务一次失败即 abort，也是合法配置（用于验证
> 「重试上限为 0 时无死锁但任务会失败」的边界行为）。

## 4. 用 Apalache 检查（仅不变式）

Apalache（https://apalache.informal.systems）只支持不变式检查，不支持时序算子
（`<>`/`[]`）与活性属性。因此用 Apalache 时检查四个不变式：

```bash
apalache-mc check --inv=NoCircularWait scheduler.tla
# 或一次检查多个：apalache-mc check --inv=TypeOK --inv=ExclusiveHold --inv=ExactHold --inv=NoCircularWait scheduler.tla
```

注意：若 Apalache 因模块中的 `WF_vars`/`<>`（`Progress` 定义）报「unsupported temporal
operator」，请临时删除 `Progress` 定义再运行——四个不变式不依赖它，删除不影响
Init/Next 与不变式检查（不动 `Spec` 本身）。活性 `Progress` 一律交给 TLC。

## 5. 模型简化与局限

- 有限状态：`R`、`T`、`MaxRetries` 均为有限常量（TLC 穷举的前提）；无限资源/任务
  由 A4 线性检查与 Rust 运行时保证。
- 原子占坑单步建模：真实实现的「逐资源获取 + 失败回滚」在模型中被压缩为单个
  原子动作——这正是 A7「原子占坑」的语义（半占状态不存在，故无需建模回滚序列）。
- 公平性简化为弱公平 WF；真实运行时以 tokio 调度近似。
- 不建模任务的实际执行时长/优先级：所有任务执行均为「瞬时完成」（Finish 一步），
  对死锁/无环性质的判定无影响。

## 6. 与 Rust 实现的对应关系

| 模型元素 | Rust 工程实现 |
| --- | --- |
| `Claim(t)`（原子占坑） | `ResourceRegistry::can_parallel`（A3）+ 未来 `Runtime::run` 的 Fork 调度（A2，待合并） |
| `ExclusiveHold` | `ResourceRegistry::check_linear` + 注册表持有语义（pdr.md §2.3） |
| `retries[t]` 有限重试 | 运行时调度循环的重试上限（实现细节，阶段 2） |
| `NoCircularWait` | 静态部分由本模型验证；阶段 2 以 `Runtime::run` 并发压力测试佐证 |

### 6.1 执行级测试（Rust）与模型的关系

本模型验证的是**调度策略**（原子占坑 / 无环等待）；`crates/algeff-core/tests/execution_axioms.rs`
（A6 批 3，interpret 合并后的执行级公理测试）验证的是**解释器对该策略及幺半群 / 线性 / 撤销语义
的实现**。二者互补，共同支撑 pdr.md §七「验证方式」的工程落地：

| 模型元素 | 执行级测试（`crates/algeff-core/tests/execution_axioms.rs`） |
| --- | --- |
| `ExclusiveHold`（互斥持有） | `exec_A4_linearity_runtime`：同资源二次 Write 在解释器 `check_linear` 处被运行时拒绝 |
| `Claim` 顺序化（阶段 1，D14） | `exec_fork_conflict_static`：同资源 Write×Write → `can_parallel=false` → 顺序执行 + combine |
| 撤销（recoverΓ） | `exec_A6_undo_roundtrip` / `exec_D10_replace_order`：LIFO 逆序 + 栈清空 |
| 幺半群结构（P1） | `exec_A1_associativity` / `exec_A2_identity`：结合律与单位元的执行 trace 等价 |

### 6.2 并发压力测试（Rust）与模型的关系

`crates/algeff-core/tests/concurrency_stress.rs`（A6 批 4，pdr.md §19.4 loom 的
替代策略——tokio 原生并发压力）以真实多线程调度佐证本模型：`concurrent_arbiter_claims`
对应 `ExclusiveHold` / `Claim`（原子占坑 + 失败回滚 + 有限重试）——8 任务争用同一
`ResourceArbiter` 互斥集合，断言任意时刻至多一个持有者、`tokio::join!` 全完成（无死锁 /
无丢失唤醒）；`parallel_runs_isolated_state` / `replay_under_concurrency` 补充模型未建模的
维度——D13 隔离-合并模式下并发任务的 registry 状态独立（fd 分配序列一致）与解释器可重放性。
执行级可重放性最终属性见 `crates/algeff-core/tests/replay_property.rs`（A6 批 5）：随机蓝图×interpret 的
执行-撤销-重放属性测试——确定性重放、A6 撤销往返（recover 后重放轨迹一致）与 A4 线性守恒（recover
恢复「状态」而非「线性标记」），与本节调度器模型的「无环等待」不变式互补。
