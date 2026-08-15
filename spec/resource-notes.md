# Resource & Coeffects 并发安全与调度说明（A3）

> 状态：阶段 1（A3 交付）。对应 `crates/algeff-core/src/resource.rs` 与
> `crates/algeff-core/src/coeffects.rs`（feature `coeffects`）。
> 契约基线：pdr.md §2.3 / §3 / §4（A3/A4/A7）/ §5.2 / §9。

## 1. registry 状态隔离策略（Fork 用 clone，决策 D13）

`ResourceRegistry` 持有三类状态：

- `handles: HashMap<Fd, ResourceHandle>`：物理句柄表（全部 Arc 共享）。
- `consumed: HashSet<Resource>`：A4 线性检查的 Write 消费记录。
- `owned_consumed: HashSet<Resource>`：A4 线性检查的 Own 终结记录。
- `next_fd: Fd`：全局唯一句柄分配器（单调递增，永不复用，决策 D1）。

**Fork 隔离**：`ResourceRegistry` 实现 `Clone`（决策 D13）。`Fork` 子任务执行前
克隆 registry 作为私有状态（`Arc` 句柄浅拷贝，零拷贝共享底层资源），子任务内的
分配 / 消费 / 终结只写入克隆体，与主 registry 完全隔离。父分支与兄弟分支互不可见
对方的线性消费记录——这正是公理 A5「Fork 子任务通过 COW 隔离」的工程映射
（物理层 COW 由 `Arc::make_mut` 延迟复制实现，见 pdr.md §9.2；线性状态的 COW 由
Clone 实现）。任务完成后，由调度方决定将子状态合并回主 registry 或直接丢弃。

**并发约束**：`sync` 之外，registry 的线性检查是同步单线程 API（`&mut self`），
不跨线程共享；跨任务共享的只有 `CoeffectStore`（内部 `Arc<tokio::sync::Mutex>`）。
因此不需要额外的锁竞争优化，`can_parallel` 的静态预检在解释器单线程内完成。

## 2. A7 无死锁的工程映射

pdr.md 公理 A7：动态资源获取采用「原子占坑 + 失败回滚 + 有限重试」，不存在循环
等待链。工程上分两层：

**静态层（零锁）**：`ResourceRegistry::can_parallel` / `can_parallel_with` 基于
`ResourceSet` 在调度前做冲突矩阵判定（pdr.md §9.1）。冲突 → 降级为顺序执行
（公理 A3 的「否则串行」），从不进入等待；不冲突 → 零锁并行。静态可判定路径
永远不会死锁，因为它根本不等待。

**动态层（MutexLock）**：对 `DataOp::MutexLock { id }` 这类运行期互斥，建议实现
（当前 core 仅提供注册表，物理执行由 A5 的 `TokioExecutor` 负责）：

1. 在 `ResourceRegistry` 中登记 `Mutex` 句柄（`ResourceHandle::Mutex`，Arc 共享）；
2. 用 `try_lock()`（非阻塞）尝试占坑：成功 → 继续执行并压入 UndoOp（释放锁）；
3. 失败 → **回滚本任务已获取的动态资源**（执行已累积的逆操作），让出执行权，
   延迟后重试，**有限重试次数**（如指数退避 + 上限），超限报 `WouldBlock`；
4. 绝不阻塞等待持锁者——不存在「任务 A 持有 X 等 Y、任务 B 持有 Y 等 X」的
   循环等待链，满足公理 A7 与命题 P5。

同步原语（如 `tokio::sync::Mutex`）的 `.lock().await` 会挂起等待，**不得**在
解释器任务内直接使用，否则引入循环等待风险；一律走 try_lock + 回滚重试。

## 3. Append 的 opt-in 语义

pdr.md §9.1 与决策 D6：`Append∥Append` **默认视为不可并行**（返回 `false`），
因为 OS 虽保证原子追加，但追加顺序不确定，违反确定性原则。调用方仅在结果与
追加顺序无关时，通过 `can_parallel_with(a, b, /* append_order_insensitive = */ true)`
显式 opt-in 并行。其余同资源组合（Read×Write、Write×Write、Own×任何）恒为串行。

线性检查与 Append 的关系：`Append` 不消费资源（公理 A4：Read/Append 不消费），
同一资源可无限次 Append；`Write` 至多一次；`Own` 终结。`Write → Close(Own)` 是
合法序列（见 §5 测试映射）。

## 4. 路径规范化说明

`ResourceRegistry::canonicalize_path(p, cwd)` 采用**纯词法**规范化（决策 D12）：

- 相对路径以 `cwd` 拼接为绝对路径；
- 消除 `.` 段；`..` 段弹出上一级；
- 不触碰真实文件系统：符号链接解析属物理执行层，规范化结果确定且可重放。

`Resource::Path` 的标识稳定性依赖调用方在登记前先规范化；物理层（A5）执行
`Open` 等操作时以规范化路径为准，保证同一文件在 `ResourceSet` 中只有一个键。
Windows 路径（盘符、`\`）由 `std::path` 组件语义处理，与词法规则一致。

## 5. A3/A4/A7 的测试映射

| 公理 | 测试 | 位置 |
| --- | --- | --- |
| A3 冲突矩阵 | `conflict_matrix_exhaustive_4x4`（4×4 模式对 × 同/异资源）、`conflict_matrix_read_read_ok`、`conflict_matrix_write_blocks`、`append_parallel_needs_opt_in` | resource.rs |
| A4 线性 | `linearity_write_then_own_legal`、`linearity_own_is_terminal`、`linearity_double_write_rejected`、`linearity_read_append_repeatable`、`clear_resets_linear_state_and_handles` | resource.rs |
| A7 无死锁 | 静态层由上述冲突矩阵覆盖；动态层由 `arbiter.rs` 的 `try_claim` 原子回滚 / Read-Read 共享 / Read-Write 互斥 / 有限重试测试覆盖（ResourceArbiter） | resource.rs + tests/arbiter.rs |
| §5.2.2 notify | `registry_sync_activation_sequence`、`registry_sync_multi_component_and_neutral`、`notify_states` | coeffects.rs |

## 6. 变更记录

- s1/a3：check_linear 细化（Write 至多一次 / Own 终结 / Read·Append 不限）；
  新增 `ResourceRegistry::clear`（供 A2 Replace）；Component 生命周期回调 +
  ComponentRegistry::sync；冲突矩阵穷举测试。
- s2/a3：新增 §7「解释器集成模式」（Open/Write/Close 生命周期、Replace→clear、
  Fork clone 隔离-合并、A4 随机序列状态机）；配套集成测试
  `crates/algeff-core/tests/registry_integration.rs`（不依赖 interpret，用公共
  API 预演 A2 解释器调用序列，为 A2 合并后集成铺路）。
- s3/a3：新增 `ResourceArbiter`（动态占坑原语：try_claim 原子占坑 + 整体回滚、
  release、held；Read 共享 / Write·Own·Append 互斥）；配套测试
  `crates/algeff-core/tests/arbiter.rs`（原子回滚、模式矩阵、释放重占、有限重试
  不变量）；新增 §8「ResourceArbiter 与 A7 的映射」。
- s4/a3：arbiter 属性测试强化——新增 `ResourceArbiter::is_clean`（泄漏检测原语）；
  测试增补 proptest（随机 claim/release 交错序列：单调不减 / 失败原子性快照 /
  无泄漏三不变量）、同资源 4×4 互斥矩阵穷举、`is_clean` 全生命周期测试；
  §8 增补属性测试不变量说明与 `is_clean` 用法。

## 7. 解释器集成模式（A2 合并前的预演）

> 本节给出 A2 解释器将执行的 registry 调用序列（伪代码）与决策 D10/D13 的
> 对应关系，由 `crates/algeff-core/tests/registry_integration.rs` 实测验证。
> 该测试不依赖 `interpret`（A2 未合并），仅用现有公共 API 预演调用序列。

### 7.1 Open → Write → Close 生命周期（对应 D1/A4）

```
// Syscall(Open) 成功后登记句柄：
let fd = registry.allocate(handle);            // D1：全局唯一、单调递增
// Syscall(Write { fd }) 执行前的线性预检：
registry.check_linear(&usage(Fd(fd), Write))?; // A4：Write 至多一次
// Syscall(Close { fd })：
registry.check_linear(&usage(Fd(fd), Own))?;   // A4：Own 终结（之后一切 usage 拒绝）
registry.take(fd);                             // Own 语义：取出并释放句柄
```

要点：`Write → Close(Own)` 是合法序列（pdr.md §14 示例）；Close 后 fd 不复用
（D1），资源键保持终结标记（后续任何 usage 报 `InvalidInput`）。
对应测试：`open_write_close_lifecycle`。

### 7.2 Replace → clear()（对应 D10）

`Replace { target }` 语义（决策 D10）：先 recover 再执行 target。registry 侧配合
为 `clear()`：释放当前路径积累的全部句柄与线性标记（`handles`/`consumed`/
`owned_consumed` 清空；`next_fd` **不复位** —— D1 单调性），随后新蓝图在同一
注册表上继续分配与消费。验证：`clear()` 后同资源可再次 Write + Own 成功
（A4 状态复位），新分配的 fd 不复用旧值（D1）。
对应测试：`replace_semantics`。

### 7.3 Fork 隔离-合并（对应 D13）

```
// Fork 前：子任务克隆父状态（COW 隔离，A5）
let mut child = parent.clone();
// 子任务在私有副本上分配 / 消费 / 终结（对父完全不可见）
let fd = child.allocate(handle);
child.check_linear(&usage(Fd(fd), Write))?;
// join 后合并：子注册表句柄迁回父，next_fd 取 max
//   —— 意图语义（需 merge 原语，见 7.4 / RFC-A3-2）：
//      parent.handles.extend(child.handles);
//      parent.next_fd = max(parent.next_fd, child.next_fd);
//   —— 当前公共 API 可行路径（值迁移 + fd 重分配）：
//      let h = child.take(fd);
//      let new_fd = parent.allocate(h);
```

**无重复 fd 保证**：子注册表克隆自父，`next_fd` 继承父值；子新分配的 fd 均
≥ 父 `next_fd`，与父已有句柄（均 < 父 `next_fd`）互不重叠 —— extend 式合并
天然无冲突（测试中以 `parent.lookup(cfd).is_none()` 断言该前置条件）。
对应测试：`fork_clone_merge_pattern`。

### 7.4 当前 API 的合并缺口（RFC-A3-2）

`ResourceRegistry` 公共 API 未暴露句柄枚举与「固定 fd 插入」，测试中的合并只能用
`take` + `allocate` 完成**值迁移**：句柄值身份（Arc）保留、fd 身份重分配。
fd 重分配导致子注册表内部的 `Resource::Fd` 键与父侧不一致，子路径的
`consumed`/`owned_consumed` 无法按原键合并（spec/axioms.md 提示的 consumed
合并将漏报）。建议（A2 集成时由 CTO 裁决）：新增
`pub fn merge(&mut self, other: Self)` —— 固定 fd 插入 + `consumed`/
`owned_consumed` 取并集 + `next_fd = max`；或等价地暴露句柄迭代器与
固定 fd 插入原语。

### 7.5 对应关系小结

| 调用序列 | 决策 | 公理 | 测试 |
| --- | --- | --- | --- |
| allocate → check_linear(Write) → take(Close/Own) | D1 | A4 | `open_write_close_lifecycle` |
| 多句柄 + 多消费 → clear() → 再消费 | D10 | A4 | `replace_semantics` |
| clone → 子分配+消费 → 合并回父 | D13 | A3/A5 | `fork_clone_merge_pattern` |
| 随机 usage 序列的状态机不变量 | — | A4 | `linearity_sequence_random` |

> 备注：任务文本提及「与 D10/D14 的对应关系」，但当前 contracts.md 决策表只有
> D1–D13，**D14 未定义**（全仓库检索无 D14 条目）。本节按现存决策 D1/D10/D13
> 撰写对应关系；D14 的存在性需 CTO 澄清（见 RFC-A3-3）。

## 8. ResourceArbiter 与公理 A7 的映射（批 3 落地）

批 2 的 §2 给出了 A7 的工程分层建议；本批在 core 落地动态层原语
`ResourceArbiter`（`crates/algeff-core/src/resource.rs`，测试 `tests/arbiter.rs`）。

**静态层（Fork 级，零锁）**：`ResourceRegistry::can_parallel` /
`can_parallel_with` 在调度前对两个 `ResourceSet` 做 §9.1 冲突矩阵判定。
冲突 → 降级串行（公理 A3「否则串行」），从不进入等待；不冲突 → 零锁并行。
静态可判定路径不等待，故无死锁。

**动态层（MutexLock 级，try_claim + 回滚 + 有限重试）**：
`ResourceArbiter::try_claim(set)` 对 set 中每个资源原子占坑——先在副本上
模拟全部占坑，全部可占才提交，任一失败**整体回滚**（自身状态完全不变，
无部分占坑残留）。调用方在失败后执行已累积逆操作并**有限重试**（如指数退避
+ 上限），超限报 `WouldBlock`；本原语**不提供阻塞等待**。同步互斥的
`.lock().await` 不得在解释器任务内直接使用（会挂起等待，引入循环等待风险，
见 §2）。

**仲裁表语义**：`claims: HashMap<Resource, usize>` 是动态占坑表，记录**所有
当前占坑者**的占用；后续 `try_claim` 与该表比对，冲突即失败。解释器单线程
（trampoline）内以 `&mut self` 串行访问天然互斥；Fork 并行时按 D13 克隆隔离
（与 registry 同策略）。跨任务的实际互斥由物理层（如 `tokio::sync::Mutex::
try_lock`，A5 执行器）保证，本原语负责 set 级原子性：失败不残留部分占坑，
重试在干净状态下重新尝试，可重入、可确定性复现。

**模式判定（对齐 §9.1 矩阵）**：Read 可共享（占坑计数累加）；Write/Own 互斥
（资源已被任何模式占坑则拒绝）；Append 按互斥的保守默认（对齐决策 D6：
Append∥Append 默认串行，顺序无关的 opt-in 由静态层 `can_parallel_with` 表达）。

**分层无循环等待论证**：

1. 静态层零等待：冲突即串行，串行执行天然无等待；
2. 动态层不等待：`try_claim` 失败立即返回，调用方回滚后让出执行权，延迟重试
   而非挂起等待持锁方——不存在「任务 A 持 X 等 Y、任务 B 持 Y 等 X」的持有-
   等待关系（命题 P5）；
3. 重试有界：有限重试次数 + 超限报 `WouldBlock`，不会无限循环；
4. 原子性：`try_claim` 失败不改状态（整体回滚），重试等价于在干净状态下重新
   尝试，行为可确定性复现（配合 A6 的 `tla/scheduler.tla` 模型，测试
   `finite_retry_eventually_succeeds` 固定序列验证）。

**与 registry 的关系**：`ResourceArbiter` 独立于 `ResourceRegistry`。registry
持有物理句柄与 A4 线性状态；arbiter 只跟踪动态占坑（占坑计数）。二者配合：
MutexLock 执行时先 `registry.lookup(fd)` 取物理句柄（A5 执行器 `try_lock` 保证
跨任务互斥），arbiter 记录本仲裁域的占坑并提供 set 级原子性与失败回滚的可重试
性。`can_parallel`（静态）负责 Fork 级并行判定，arbiter（动态）负责 MutexLock
级占坑——两层职责正交，互不依赖。

**属性测试强化（批 4，A7 不变量）**：`tests/arbiter.rs` 新增 proptest
`random_claim_release_keeps_invariants`，对随机 (resource, mode) 集合 × 随机
claim/release 交错序列断言三条不变量：

1. **单调不减**：每次 `try_claim` 返回 `true` 后，held 集 = 本次集合 ∪ 之前持有
   （占坑只增不减，直到 `release` 才缩小）；
2. **原子性快照**：`try_claim` 返回 `false` 后，arbiter 状态与调用前完全一致
   （对资源池逐资源做布尔 held 快照断言，无部分占坑残留——对应 A7「原子占坑 +
   失败回滚」）；
3. **无 panic / 无泄漏**：随机序列全程不 panic；测试维护一个与实现同编码的独立
   参照模型（Read 计数累加 / `usize::MAX` 独占标记）作为 oracle，每步交叉校验
   arbiter 与模型的布尔 held 状态一致；结束时按模型释放全部占坑，断言
   `is_clean()` 为 `true`（claims 表空）——任何计数漂移（如 Read 多计）都会在此
   泄漏检测中暴露。

**`is_clean` 用法**：`ResourceArbiter::is_clean(&self) -> bool` 是仲裁表空检测
原语（`claims` 为空即无任何占坑），供泄漏检测与「全部释放后复位」断言使用：
新建时干净、持有时不干净、失败的 `try_claim` 不改变干净度、全部 `release` 后
恢复干净（测试 `is_clean_tracks_full_lifecycle` 验证全生命周期）。

**动态层互斥矩阵穷举**：`arbiter_mutex_matrix_exhaustive_4x4` 对同资源 4×4
模式对穷举（第一方任意模式首占成功，第二方仅 Read×Read 可共享），与 §9.1
冲突矩阵一致：Read-Read 可、Read-Write 不可、Write-Write 不可、Own×任意不可、
Append×Append 保守不可（对齐决策 D6，opt-in 由静态层 `can_parallel_with` 表达）。
