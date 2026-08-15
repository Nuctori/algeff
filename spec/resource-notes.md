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

**强制记录（R-1 待办①，批 5 落地）**：Fork 分支内使用 `DataOp::MutexLock { id }`
必须同时声明对应资源 `Resource::Fd(id)`（占用模式任取互斥模式），以触发静态层
`can_parallel` 冲突检测并降级串行；若未声明，静态层对该互斥不可见，动态层 arbiter
是唯一防线——批 5 已由 A5 `TokioExecutor` 将 `op_mutex_lock` 接入 `ResourceArbiter`
（`try_claim` 原子占坑 + 8×1ms 有限重试，超限返回 `WouldBlock`，不阻塞等待；
详见 §8 末与 §9 前言）。

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
| A7 无死锁 | 静态层由上述冲突矩阵覆盖；动态层由 `arbiter.rs` 的 `try_claim` 原子回滚 / Read-Read 共享 / Read-Write 互斥 / 有限重试测试覆盖（ResourceArbiter）；批 7 增补 `arbiter_mutex.rs` 组合验证（占坑-释放周期 / Read 共享 vs Write 独占 × `tokio::sync::Mutex` 对应 / 有限重试上界 WouldBlock 无残留） | resource.rs + tests/arbiter.rs + tests/arbiter_mutex.rs |
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
- s7/a5：D16 落地——`TokioExecutor::op_mutex_lock` 接入 `ResourceArbiter`（MutexLock
  id → `Resource::Fd(id)` × Write 独占占坑 + 8×1ms 有限重试，超限 `WouldBlock`；
  undo 与显式 `MutexUnlock` 均同步释放占坑，`release` 幂等无需标志位）；§2 补
  「Fork 分支内 MutexLock 必须声明对应资源」强制记录（R-1 待办①）；测试
  `crates/algeff-std/tests/executor.rs` 新增 `mutex_lock_arbiter_contention` /
  `mutex_unlock_releases_arbiter`，`mutex_lock_exclusion` 断言同步更新为
  WouldBlock 快速失败；新增 §9「make_mut 物理 COW 评估」（R-3 预研，裁决：推迟）。

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

> **接入状态（批 7/8/9 更新）**：arbiter ↔ `MutexLock`（`DataOp::MutexLock { id }`）
> 的动态层接入为**已接入**（A5 批 7 `254eaf3`：`try_claim` + 8×1ms 有限重试 +
> WouldBlock；批 8 `f897669`：RAII claim guard 覆盖取消路径；批 9 `4c993f3`：
> arbiter 改 `std::sync::Mutex`——Drop 内 async-context panic 消除（blocking_lock
> 在 worker 线程 poll 帧 panic 的风险，blocker-1）；契约 D16 已同步，D-034）。Fork 分支内使用 `MutexLock { id }` 的蓝图**必须**声明对应
> `Resource::Fd(id)` 以触发静态顺序化（强制规则，见 §2；未声明时动态层
> WouldBlock 快速失败是最后防线）。批 7 新增 core 侧组合验证
> `tests/arbiter_mutex.rs`（占坑-释放周期 / Read 共享 vs Write 独占 × tokio
> Mutex 对应 / 有限重试上界 WouldBlock 无残留），作为接入后行为契约的占坑侧预演。

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
- 上限），超限报 `WouldBlock`；本原语**不提供阻塞等待**。同步互斥的
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

**MutexLock 级接入（批 5，D16 落地）**：A5 `TokioExecutor` 的 `op_mutex_lock` 已接入
本原语——`MutexLock { id }` 映射为 `Resource::Fd(id)` × `AccessMode::Write` 独占占坑
（Write 与 Own 在仲裁器同为独占，选 Write 表达「共享互斥锁」而非「终结所有权」）；
`try_claim` 失败 → 8 次 × 1ms 退避有限重试 → 超限 `Err(SysError::WouldBlock)`
（A7：不阻塞等待）；undo 闭包与显式 `MutexUnlock` 均释放占坑，`release` 幂等
（未占坑资源 no-op）故无需标志位。测试：`algeff-std/tests/executor.rs`
`mutex_lock_exclusion`（竞争 WouldBlock 快速失败）/ `mutex_lock_arbiter_contention`
（双任务至多一个持有，不挂死）/ `mutex_unlock_releases_arbiter`（unlock 释放占坑
后可重锁）。

## 9. make_mut 物理 COW 评估（R-3 预研，批 5）

> 对应 G4 残余 R-3 与 pdr.md §9.2 / 命题 P3「工程上通过 `Arc::make_mut`（延迟复制）
> 实现」。本批仅做接入点 / 成本 / 决策分析，**不实施**（裁决建议见 9.4）。

### 9.1 目标与现状

pdr.md §9.2 的「Fork 内存行为」：ReadOnly 分支共享 `Arc<[u8]>`（引用计数零拷贝）；
Mutable 延迟复制——仅克隆 Arc 句柄，首次写入时触发 `clone_data()`。命题 P3 的工程
载体是 `Arc::make_mut`：strong_count > 1 时先深拷贝再 `&mut`（整块复制换跨平台
一致性），否则原地借用。

现状：A5 语义层已闭环——registry 经 D13 Clone 隔离（consumed/owned_consumed 独立，
`fork_same_fd_write`/`parallel_runs_isolated_state` 已验证）；物理层共享的只是
`ResourceHandle` 的 Arc（File/TcpStream/管道半端等），`make_mut` **未接入**。

### 9.2 接入点分析

`Arc::make_mut` 的正确用法要求 `Arc<T>` 是唯一可变访问路径。对 Algeff 的
`ResourceRegistry` + `TokioExecutor`：

1. **句柄存储**：registry 持有的是**不可变** Arc（`ResourceHandle::File(Arc<File>)`
   等），而 tokio 的 AsyncRead/AsyncWrite 只对 `&mut T` 实现——这正是 executor 侧
   用 `Arc<tokio::sync::Mutex<File>>` 工作对象、registry 侧放簿记 token 的双表结构
   原因（RFC-05）。若引入 `make_mut`，可把双表收敛为 registry 单一 Arc 所有权，
   子分支首次破坏性操作前 `make_mut` 私有化副本。
2. **Close 拒绝路径（R-3 原文缺口）**：registry 层经 D13 Clone 已隔离（子分支
   take/remove 只影响克隆体），但物理工作对象在共享 executor 的 `files` 表中——
   Fork 并行（D17）下子分支 `Close` 会移除共享工作对象，父分支后续 IO 受影响，
   当前无拒绝路径、无测试。`make_mut` 语义下应改为：子分支破坏性操作前先
   `make_mut` 得到私有副本，Close 只终结副本；无法私有化时新增拒绝路径。
3. **与 Dup 的语义冲突（关键障碍）**：File 的 `try_clone` 提供廉价「簿记 token」
   共享同一 OS 描述，Dup 契约是**真共享**（游标/描述共享，`dup_shares_handle`
   测试依赖）。`make_mut` 的深拷贝无法在不破坏 Dup 契约的前提下区分「Fork 隔离
   需要复制」与「Dup 需要共享」——必须引入额外的分支/克隆代际标记。

### 9.3 成本

1. **语义成本**：A4 线性状态已由 registry Clone 精确隔离，物理复制是语义层的
   **冗余第二道保险**——当前无任何测试暴露其缺失（R-3 原文亦注明「无拒绝路径
   测试」），实施前必须先行补契约测试。
2. **实现成本**：改动 registry 句柄存储结构（影响 D1/D13/D17 冻结面）、executor
   双表结构（RFC-05 关联）、`op_close`/`op_truncate` 等破坏性操作与全部共享路径
   测试；另需处理 9.2-3 的代际标记——属冻结面外设计，需 RFC + CTO 裁决。
3. **运行成本**：破坏性分支付出整块复制代价（与 §9.2「整块复制换一致性」一致），
   而阶段 1 顺序 / 受限并行路径下分支间物理写冲突本就不可达。

### 9.4 CTO 决策建议

**建议：推迟（随阶段 3 并行化一并实施，G4 批 5 判定维持不变）**，理由：

1. 语义层已闭环（D13 Clone 隔离 + D17 合并回父），当前阶段不产生分支间物理写
   冲突，物理 COW 是阶段 3「并行 Fork 真并发写」的前置载体而非当前缺陷；
2. make_mut 与 Dup 真共享语义冲突（9.2-3）需要专门设计，仓促接入破坏冻结面；
3. 实施前置条件：先补「分支内破坏性操作（Close/Truncate）」契约测试（R-3 原文
   缺失项），作为阶段 3 验收前置。

不选择「放弃」：pdr.md §9.2 与命题 P3 明确将 make_mut 列为物理载体，阶段 3
并行写需要它。不选择「立即实施」：冻结面内无测试暴露缺口，性价比不足。

## 10. R2 审计已知缺陷登记（RFC-06 / RFC-07）

> R2 对抗审计（`crates/algeff-std/tests/adversarial_r2.rs`）新确认的两项已知缺陷。
> 均不在冻结面（runtime.rs / executor.rs / resource.rs）允许的最小修复范围内，
> 以「断言偏差可复现」测试记录，阶段 3+ 修复。

### RFC-06：Fork 右分支分配使父 next_fd 二次增长（fd 区间归一化失效）

右分支在 `k<<48` 预留区间（`offset_next_fd`，F1/S6/A2 全局唯一区间）**实际分配
fd** 后，`merge` 的归一化分支失效（`fork_region` 已记录但 `next_fd != base+offset`），
父 `next_fd` 被抬高 `k·2^48`；连续多轮后二次增长（Σk·2^48），~360 轮溢出 u64
（debug panic / release 回绕 → fd 碰撞）。修复点：`ResourceRegistry::offset_next_fd`
/ `merge` 的区间归一化（resource.rs，冻结面外）。

测试记录：`adversarial_r2.rs::fd_region_quadratic_growth_known_deviation`（断言
`next_fd ≥ 2^50` 的爆涨行为可复现）+ `fd_region_seq_overflow_panics_under_500_rounds`
（debug 下 catch_unwind 捕获 ~360 轮溢出 panic）。修复后两测试会失败，提醒更新。

### RFC-07：管道半端经 Fork registry Clone 共享 Arc → 分支内管道 IO InvalidInput

registry 经 D13 Clone 做分支隔离时，`ResourceHandle::PipeReader/PipeWriter` 的 Arc
被**共享**（strong_count > 1）。executor 管道路径依赖 `Arc::get_mut`（take/put_back
轮换，`op_read`/`op_write`），共享下必然失败 → `InvalidInput`。文件工作对象是
`Arc<tokio::sync::Mutex<File>>`（双表结构，RFC-05 关联），共享下 lock 可用，不受
影响——管道是唯一未受双表保护的半端类。

用户视角：未 Dup 的管道在 Fork 分支内 IO 被错误拒绝（executor 无法区分「用户 Dup」
与「Fork registry 克隆」产生的共享）。修复点：executor 管道双表改造（文件式
Arc<Mutex> 覆盖管道半端，或 §9 的 make_mut + 代际标记），executor 属 A5 域，
冻结面外。§9.3.1「无测试暴露其缺失」已被本项修正。

测试记录：`adversarial_r2.rs` 分支冲突负载改用文件（`fd_1000_conflict_forks_region_*`
`、`fd_region_quadratic_growth_*`），保留 fd 分配属性覆盖；修复后可将负载改回管道。

### RFC-08：Timeout 内并行 Fork 的孤儿分支副作用不可撤销（P4/A6 义务边界反例）

`Action::Timeout{action=Fork{...}, ...}` 超时触发时，inner future 被
`tokio::time::timeout` 丢弃——已 spawn 的并行分支任务（spawn_blocking 线程上
current-thread runtime 驱动）**继续执行**：其物理副作用（如 Open 创建文件）发生、
undo 栈为空（分支 undo 未合并）、`Replace`/`recover` 无法恢复 → w;w̄=1 在
Timeout+Fork 组合边界**不成立**（部分可撤销性反例）。对 P5/A7 无影响（孤儿独立
完成、无等待链，属资源泄漏非死锁）。A6/P4 陈述建议加范围限定「仅运行时已追踪
（trackΓ）的操作」——R2 数学审计建议（编号修正：R2 语境曾误称本项为 RFC-07，
正式登记为 RFC-08）。

测试记录：`adversarial_r2.rs::time_timeout_parallel_fork_orphan_effects_unrecoverable`
（断言孤儿 Open 副作用物理发生；修复方向 = 超时传播/分支取消协议，阶段 3+）。

### RFC-06 的 D1 边界影响（R2 数学审计）

release 下 u64 回绕 → fd 复用 = D1「单调不复用」**违反**（debug 下 catch_unwind
panic）。数学核验：Σ_{k=1..n} k·2^48 ≥ 2^64 ⟺ n≈362 轮。模型承诺（D1）无界，
故非 pdr §17 类固有局限，而是 merge 归一化的实现缺陷——已登记（§10 RFC-06），
建议 D1 契约行加边界注（承诺范围为不溢出前缀）或提升修复优先级（阶段 3+ 中
优先）。对 P2/P3 语义本体无直接证伪（交换律/隔离是 trace 语义命题，与 fd 值域
无关）。
