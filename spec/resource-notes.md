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
| A7 无死锁 | 静态层由上述冲突矩阵覆盖；动态层 TLA+ 模型 `tla/scheduler.tla`（A6 拥有） | — |
| §5.2.2 notify | `registry_sync_activation_sequence`、`registry_sync_multi_component_and_neutral`、`notify_states` | coeffects.rs |

## 6. 变更记录

- s1/a3：check_linear 细化（Write 至多一次 / Own 终结 / Read·Append 不限）；
  新增 `ResourceRegistry::clear`（供 A2 Replace）；Component 生命周期回调 +
  ComponentRegistry::sync；冲突矩阵穷举测试。
