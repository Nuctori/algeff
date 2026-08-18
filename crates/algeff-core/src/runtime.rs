//! 运行时内核 —— 契约冻结（pdr.md §5.1 / §12.3）。
//!
//! A2 拥有本文件的实现：`interpret` 解释器（trampoline）、UndoStack 撤销、
//! trackΓ/recoverΓ、Sleep/Timeout/Fork/Scope/Catch 等节点的运行时语义。
//! 基础骨架由 CTO 冻结（contracts.md §类型冻结），方法体为 A2 交付。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::action::{Action, DataOp, Id, Signal, Value};
use crate::cost::EffectCost;
use crate::error::SysError;
use crate::resource::{ResourceRegistry, ResourceSet};
use crate::syscall::{BoxFuture, SyscallExecutor, UndoCapability, UndoOp};

#[cfg(feature = "virtual-clock")]
use crate::virtual_clock::VirtualClock;
/// 单次分配/IO 长度上界（审计 R1 契约-F7 修复）：`vec![0u8; len]` 在 debug
/// 下分配失败 = 进程级 abort（handle_alloc_error 不可捕获），release 下 OOM
/// abort —— 不受信任蓝图可崩溃宿主进程（与 RFC-11 修复前的栈溢出同族拒绝
/// 服务面）。取 64MB：远超真实单次 IO/分配需求，远低于危险分配量级。
/// 超限返回 `SysError::InvalidInput`（可被外层 Catch 捕获）。
pub const MAX_IO_LEN: usize = 64 * 1024 * 1024;

/// 判断 Value 是否含资源句柄（Fd/Pid，含嵌套 List）——含则结果不可缓存
/// （句柄可能随 Replace 的 reg.clear / Fork 分支丢弃失效，D-103）。
fn value_contains_handle(v: &Value) -> bool {
    match v {
        Value::Fd(_) | Value::Pid(_) => true,
        Value::List(items) => items.iter().any(value_contains_handle),
        _ => false,
    }
}

/// 幂等键注册表（D-0xx 幂等键状态机）：键 → 状态机（COMMITTED/REVERTED）。
///
/// 跨执行持久（不随 Replace 的 reg.clear 清除）：恰好一次语义要求"副作用在
/// 生命周期内只真正发生一次"——REVERTED 允许重执行，COMMITTED 返回缓存。
/// **注意**：无淘汰/容量上限——动态 key（如按请求/订单号）场景下随 Runtime
/// 生命周期无界累积，属设计取舍（Note-1）；容量上限留待全局共享升级。
#[derive(Debug, Default, Clone)]
pub struct IdempotencyRegistry {
    states: HashMap<String, IdempotencyState>,
}

/// 幂等键状态。
#[derive(Debug, Clone)]
pub struct IdempotencyState {
    /// COMMITTED：副作用已发生且未撤销（重试返回缓存，不重执行）。
    /// REVERTED：副作用已撤销（允许未来重新执行）。
    pub status: IdempotencyStatus,
    /// COMMITTED 时的缓存结果（REVERTED 后置 None）。
    pub value: Option<Value>,
}

/// 幂等状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdempotencyStatus {
    Committed,
    Reverted,
}

impl IdempotencyRegistry {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
        }
    }

    /// 查键：COMMITTED 未 REVERTED → 返回缓存结果（重试去重）。
    /// 含资源句柄（Fd/Pid）的结果不缓存（可能随 Replace/分支丢弃失效）——
    /// COMMITTED 但无缓存 → fallback 返回 `Unit`（副作用已发生，结果不可再得，
    /// D-103 承诺）；未记录/REVERTED → None（重执行）。
    pub fn lookup_committed(&self, key: &str) -> Option<Value> {
        match self.states.get(key) {
            Some(s) if s.status == IdempotencyStatus::Committed => match &s.value {
                Some(v) => Some(v.clone()),
                None => Some(Value::Unit), // fallback：已发生但结果不可缓存
            },
            _ => None,
        }
    }

    /// 执行成功后提交：键置 COMMITTED + 缓存结果（幂等，重复 commit 不覆盖）。
    /// 含资源句柄（Fd/Pid）的结果**不缓存**（COMMITTED 但 value=None）——
    /// 句柄可能随 Replace 的 reg.clear / Fork 分支丢弃而失效，缓存命中会返回
    /// use-after-release 的死句柄（D-103：Fd 失效风险 + fallback）。
    pub fn commit(&mut self, key: &str, value: Value) {
        let entry = self
            .states
            .entry(key.to_string())
            .or_insert_with(|| IdempotencyState {
                status: IdempotencyStatus::Reverted,
                value: None,
            });
        if entry.status != IdempotencyStatus::Committed {
            entry.status = IdempotencyStatus::Committed;
            entry.value = if value_contains_handle(&value) {
                None // 含 Fd/Pid：不缓存（句柄易失效），重试 fallback Unit
            } else {
                Some(value)
            };
        }
    }

    /// 撤销后标记 REVERTED：键释放，允许未来重新执行（恰好一次语义的
    /// "卸载不删日志，只执行逆函数"）。
    pub fn revert(&mut self, key: &str) {
        if let Some(s) = self.states.get_mut(key) {
            s.status = IdempotencyStatus::Reverted;
            s.value = None;
        }
    }

    /// 当前键的状态（测试/诊断用）。
    pub fn status_of(&self, key: &str) -> Option<IdempotencyStatus> {
        self.states.get(key).map(|s| s.status)
    }
}

/// 效果上下文 Γ（pdr.md §5.1.1）：当前状态（cwd + 环境变量）。
///
/// 逻辑时钟放在 Context 内（而非 Runtime）：`interpret` 的签名冻结为
/// `(&mut Context, ...)`，只有从这里才能让 Sleep 节点访问时钟。
#[derive(Debug, Clone)]
pub struct Context {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
    /// 幂等键注册表（D-0xx）：跨执行持久，undo 闭包经 Arc 共享访问。
    pub idempotency: std::sync::Arc<std::sync::Mutex<IdempotencyRegistry>>,
    /// 逻辑时钟（可选 feature `virtual-clock`，pdr.md §5.2 / §12.1）。
    #[cfg(feature = "virtual-clock")]
    virtual_clock: Option<VirtualClock>,
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    pub fn new() -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let env = std::env::vars().collect();
        Self {
            cwd,
            env,
            idempotency: std::sync::Arc::new(std::sync::Mutex::new(IdempotencyRegistry::new())),
            #[cfg(feature = "virtual-clock")]
            virtual_clock: Some(VirtualClock::new()),
        }
    }

    /// 逻辑时钟可变访问（`Runtime::virtual_clock()` 的底层载体）。
    #[cfg(feature = "virtual-clock")]
    pub fn virtual_clock_mut(&mut self) -> Option<&mut VirtualClock> {
        self.virtual_clock.as_mut()
    }
}

/// 撤销栈：LIFO 逆操作（pdr.md §5.1.4 / §11）。
#[derive(Default)]
pub struct UndoStack {
    ops: Vec<UndoOp>,
    /// 已发生不可逆副作用（NonInvertible，无逆元操作被真实执行）。
    /// 置位后不因 `rollback_from` 清除（不可逆副作用无法撤销，回滚只是
    /// 尽力恢复可逆部分）；仅 Replace 成功完成恢复后重置（新执行段起点）。
    irreversible: bool,
    /// 运行时累计开销（D-104 落点 a，文档 v0.1 审计结论）：与义务效应同源
    /// 累加——每个经 `exec_via` 真实执行的 DataOp 在此落点记账。幂等键命中
    /// （`Action::Idempotent` 缓存命中不调 `exec_via`）→ 开销自动不累计，
    /// 满足文档 C1（幂等段 inner 开销塌缩为 ~0）。纯计算（无 DataOp）开销为
    /// 零。代数结构见 `crate::cost`：三原语 [min,max] 区间，顺序组合逐分量加法。
    accrued: EffectCost,
}

impl UndoStack {
    pub fn new() -> Self {
        Self {
            ops: Vec::new(),
            irreversible: false,
            accrued: EffectCost::ZERO,
        }
    }

    pub fn push(&mut self, op: UndoOp) {
        self.ops.push(op);
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// 累加一次真实执行的效应开销（与 `push` 同源落点，D-104）。
    pub fn add_cost(&mut self, cost: EffectCost) {
        self.accrued = self.accrued.plus(&cost);
    }

    /// 运行时累计开销查询（可审计：跑完拿到 cost trace，文档 §1 规约落地）。
    pub fn accrued_cost(&self) -> EffectCost {
        self.accrued
    }

    /// 记录一次不可逆副作用（NonInvertible 操作被真实执行）。
    pub fn mark_irreversible(&mut self) {
        self.irreversible = true;
    }

    /// 执行段内是否已发生不可逆副作用（Replace 闸门检查，真回归前提）。
    pub fn has_irreversible(&self) -> bool {
        self.irreversible
    }

    /// 重置不可逆标记（Replace 检查通过后 / RuntimeException 恢复完成后：
    /// 新执行段起点）。
    pub fn reset_irreversible(&mut self) {
        self.irreversible = false;
    }

    /// 把另一个栈的全部逆操作按序追加到本栈（Fork 并行合并用，D14）。
    /// 调用方按 **left → right** 顺序 append（栈底到栈顶），使 LIFO recover
    /// 先执行 **right** 的逆操作、再执行 left 的 —— 与顺序路径「left 先执行、
    /// right 后执行」的观察序一致（right 的效果后发生、先撤销）。
    pub fn append(&mut self, other: UndoStack) {
        self.ops.extend(other.ops);
        self.irreversible |= other.irreversible;
        self.accrued = self.accrued.plus(&other.accrued);
    }

    /// recoverΓ：按 LIFO 顺序执行全部逆操作（pdr.md §5.1.3）。
    /// 每个逆操作返回 Result——撤销失败必须上报（语义真回归，D-098）：
    /// 继续执行剩余逆操作（尽力回滚），聚合首个错误返回。
    pub async fn recover(&mut self) -> Result<(), SysError> {
        let mut first_err = None;
        while let Some(op) = self.ops.pop() {
            if let Err(e) = op.await {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// 回滚 `mark` 之后压入的逆操作（取消传播协议，Timeout 取消用，RFC-08/09）：
    /// 按 LIFO 顺序执行 `ops[mark..]`（与 `recover` 同序）并把它们弹出；
    /// `ops[..mark]` 保留——外层效果（Timeout 之前已入栈的 undo）不属于本次
    /// 回滚范围。`mark >= len` 时为空操作（防御：无内层效果需要回滚）。
    /// undo 可为异步 IO（决策 D4），故本方法为 async。
    /// 撤销失败同样上报（聚合首个错误）；不可逆标记不清除（副作用无法撤销）。
    pub async fn rollback_from(&mut self, mark: usize) -> Result<(), SysError> {
        let mut first_err = None;
        while self.ops.len() > mark {
            let op = self.ops.pop().expect("len > mark 保证栈非空");
            if let Err(e) = op.await {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(feature = "coeffects")]
use crate::coeffects::{Activation, CoeffectStore, Component, ComponentState, DepKey};

/// Algeff 运行时（pdr.md §12.3）。
///
/// 注意：逻辑时钟已下沉至 `Context`（`Runtime::virtual_clock()` 委托
/// `Context::virtual_clock_mut()`），使冻结签名的 `interpret` 可访问。
pub struct Runtime {
    /// 当前上下文 Γ。
    pub context: Context,
    undo_stack: UndoStack,
    resource_registry: ResourceRegistry,
    /// 共享执行器（CTO 批准方向，D14 阶段 3 + D19）：公开 API `Runtime::new(Box<dyn>)`
    /// 不变，内部以 `Arc<Mutex>` 包装 —— Fork 并行分支各持 Arc 克隆，经锁
    /// 互斥串行化执行器调用（锁仅保护执行器内部状态，物理 IO 本身异步）。
    /// `SyscallExecutor: Send` 超 trait（D19）使 `Mutex<T>: Sync` 成立，
    /// 编译期强制 executor Send，无需 unsafe 包装。
    executor: SharedExecutor,
    /// 余效应上下文（可选 feature `coeffects`，pdr.md §5.2）。
    #[cfg(feature = "coeffects")]
    dependency_table: Option<CoeffectStore>,
    #[cfg(feature = "coeffects")]
    loaded_components: Option<Vec<Component>>,
    /// 组件满足状态机（与 `loaded_components` 索引对齐，pdr.md §5.2.2 notify）。
    #[cfg(feature = "coeffects")]
    component_state: Option<ComponentState>,
    /// 自持 tokio reactor。注意：`Runtime::new` 需在 tokio 上下文之外调用（D9）。
    reactor: tokio::runtime::Runtime,
}

impl Runtime {
    pub fn new(executor: Box<dyn SyscallExecutor + Send>) -> Self {
        Self {
            context: Context::new(),
            undo_stack: UndoStack::new(),
            resource_registry: ResourceRegistry::new(),
            executor: Arc::new(tokio::sync::Mutex::new(executor)),
            #[cfg(feature = "coeffects")]
            dependency_table: Some(CoeffectStore::new()),
            #[cfg(feature = "coeffects")]
            loaded_components: Some(Vec::new()),
            #[cfg(feature = "coeffects")]
            component_state: Some(ComponentState::new()),
            reactor: tokio::runtime::Runtime::new()
                .expect("Runtime::new: 无法创建 tokio reactor（已在 tokio 上下文中？）"),
        }
    }

    pub fn registry(&mut self) -> &mut ResourceRegistry {
        &mut self.resource_registry
    }

    pub fn undo_stack(&mut self) -> &mut UndoStack {
        &mut self.undo_stack
    }

    pub fn context(&mut self) -> &mut Context {
        &mut self.context
    }

    #[cfg(feature = "coeffects")]
    pub fn dependency_table(&mut self) -> Option<&CoeffectStore> {
        self.dependency_table.as_ref()
    }

    /// `loaded_components` 的公开可变存取（pdr.md §12.3）：注册组件 = push。
    #[cfg(feature = "coeffects")]
    pub fn components(&mut self) -> Option<&mut Vec<Component>> {
        self.loaded_components.as_mut()
    }

    /// 组件同步（pdr.md §5.2.2 notify 的运行时载体）：以 `dependency_table`
    /// 当前快照驱动全部已加载组件的激活/停用状态机，状态翻转时触发组件
    /// 生命周期回调，返回 (组件索引, Activation) 事件列表（仅翻转事件）。
    /// 依赖表或组件列表未初始化（None）时返回空列表。
    #[cfg(feature = "coeffects")]
    pub async fn sync_components(&mut self) -> Vec<(usize, Activation)> {
        let Some(store) = self.dependency_table.as_ref() else {
            return Vec::new();
        };
        let Some(state) = self.component_state.as_mut() else {
            return Vec::new();
        };
        let Some(comps) = self.loaded_components.as_mut() else {
            return Vec::new();
        };
        state.sync(comps, store).await
    }

    /// 注册依赖 k ↦ v（pdr.md §5.2.3 依赖作为效果）：包装 `CoeffectStore::set`，
    /// 逆操作压入撤销栈——`recover()` 一并撤销依赖，实现效果与余效应的统一。
    /// 返回一份等价逆操作供调用方即时撤销；栈内副本由 `recover()` 消费，
    /// 两份只应执行其中一份（pdr.md §5.2.3 可逆性保证）。
    /// 依赖表未初始化（None）时返回 ENOSYS（`SysError::Other(38)`）。
    #[cfg(feature = "coeffects")]
    pub async fn set_dependency(&mut self, k: DepKey, v: Value) -> Result<UndoOp, SysError> {
        let Some(store) = self.dependency_table.as_ref() else {
            return Err(SysError::Other(38));
        };
        let (undo, handed) = store.set_replicated(k, v).await;
        self.undo_stack.push(undo);
        Ok(handed)
    }

    #[cfg(feature = "virtual-clock")]
    pub fn virtual_clock(&mut self) -> Option<&mut VirtualClock> {
        self.context.virtual_clock_mut()
    }

    /// 执行蓝图（异步），完整路径语义（contracts.md D10/D14）：
    ///
    /// - 主循环为 `interpret` trampoline：`cur` 贯穿节点，`Pure` 单位元直接收敛；
    /// - `Fork`：静态冲突检测（`fork_conflict`）+ **can_parallel 时真并行**
    ///   （D14 阶段 3，pdr.md §2.1「并发分叉」/ §19.2「Fork→spawn 合法」）：
    ///   两分支各自持有 registry/context 隔离副本（D13）+ 独立 UndoStack，经
    ///   共享执行器 Arc<Mutex> 通道在独立阻塞线程上并发驱动，完成后合并回父
    ///   （registry `merge`：handles/consumed/owned_consumed 并集 + next_fd=max
    ///   归一化；F1 修复：spawn 前右分支取全局唯一 fd 区间避免两分支撞 fd
    ///   （S6/A2：嵌套 Fork 任意深度下并发分支区间同样互斥）；
    ///   undo：right 先、left 后，保持 LIFO 与观察序）；can_parallel=false
    ///   保持顺序执行（left→right→combine），完成后同样 merge 回父（F2 修复：
    ///   分支 fd 与线性标记不泄漏）；
    /// - `Replace`：**先 `recover()` + `reg.clear()`**（LIFO 执行全部累积逆操作
    ///   并清空撤销栈与 registry 句柄/线性标记，next_fd 保留 D1）再执行 target，
    ///   以其结果结束（D10，安全默认：资源不泄漏）；
    /// - `Scope`：cwd 词法规范化入栈，退出时（含 inner 出错）无条件恢复；
    /// - `Timeout`：取消传播协议（RFC-08/09/12 残余）——超时触发时广播取消
    ///   给并行 Fork 分支（结构化并发近似）、有界宽限等待分支 join、回滚
    ///   inner 已入栈 undo 与新增线性标记，再执行 on_timeout；
    /// - `Catch`：仅处理错误值，不触碰撤销栈（recover 语义在 Replace/recover 路径）；
    /// - `WatchSignal`/`Invoke`：委托执行器，默认 ENOSYS（`Other(38)`）原样透传。
    ///
    /// 注意：`interpret` 的递归 future 已 Send 化（迭代 3-A1，见下方
    /// `LocalBoxFuture` 别名注）；Fork 并行分支经 `Handle::spawn` 调度到
    /// Runtime 自持的多线程 reactor（替代旧 `spawn_blocking` + current-thread
    /// runtime 构建）。
    pub async fn run(&mut self, action: Action) -> Result<Value, SysError> {
        interpret_impl(
            action,
            &mut self.context,
            &mut self.undo_stack,
            &mut self.resource_registry,
            ExecAccess::Shared {
                executor: self.executor.clone(),
                reactor: self.reactor.handle().clone(),
            },
            0,
            None,
        )
        .await
    }

    /// 阻塞执行蓝图：`reactor.block_on(self.run(action))`。
    /// 与 `Runtime::new` 相同，须在 tokio 上下文之外调用（tokio 的
    /// `block_on` 不支持在 runtime 线程内嵌套）。
    pub fn run_blocking(&mut self, action: Action) -> Result<Value, SysError> {
        let reactor = &self.reactor;
        let context = &mut self.context;
        let undo_stack = &mut self.undo_stack;
        let resource_registry = &mut self.resource_registry;
        let executor = self.executor.clone();
        reactor.block_on(interpret_impl(
            action,
            context,
            undo_stack,
            resource_registry,
            ExecAccess::Shared {
                executor,
                reactor: reactor.handle().clone(),
            },
            0,
            None,
        ))
    }

    /// 恢复效果上下文：执行全部累积逆操作（pdr.md §5.1.3 recoverΓ）。
    /// 撤销失败上报（语义真回归）；成功后重置不可逆标记（恢复完成 = 段结束）。
    pub async fn recover(&mut self) -> Result<(), SysError> {
        let r = self.undo_stack.recover().await;
        if r.is_ok() {
            self.undo_stack.reset_irreversible();
        }
        r
    }
}

/// 子 Action 递归用的 future 包装（`Box::pin` 堆分配，解释器状态机帧只存
/// 指针 —— 保护 RFC-11 深度守卫的栈预算）。
///
/// **Send 化（迭代 3-A1）**：本别名原为非 Send（区分 syscall.rs 的
/// `BoxFuture` 强制 `+ Send`），D19（`SyscallExecutor: Send`）后解释器捕获的
/// 全部状态（`&mut Context/UndoStack/ResourceRegistry`、`ExecAccess`、
/// `Action` 闭包 `+ Send`、`CancelToken`、coeffects 的 `Arc<Mutex>` 型状态）
/// 均已 Send —— 迭代 3-A1 将递归 future 提升为 `+ Send`，使 Fork 并行分支
/// 可直接 `spawn` 到 Runtime 自持的多线程 reactor（替代逐节点
/// `spawn_blocking` + current-thread runtime 构建，D9 契约保持：分支在
/// tokio 上下文内运行）。编译器静态验证全 feature 组合下无 Send 泄漏。
type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 取消传播协议（RFC-08/09/12 残余修复）的取消令牌：`watch<bool>` 接收端。
///
/// 每个 `Action::Timeout` 臂为 inner 子树建立一个取消域：臂持有发送端
/// （超时触发时 `send(true)` 广播），inner 子树（含 Fork 并行分支）持有
/// 本接收端克隆。分支在**每个 action 处理前**检查 `is_cancelled()`
/// （结构化并发近似），`Sleep` 臂额外与 `changed()` 竞速实现可取消等待。
///
/// 选择 `watch` 而非 `Notify`：`changed()` 在「值自上次观察后已变更」时
/// 立即完成——无「先 send 后注册 waiter」的丢失唤醒窗口（Notify 有）；
/// 且发送端丢弃后接收端仍保留最后值（取消标志粘性：取消一旦广播，迟到
/// 的分支/恢复执行的分支仍能观察到）。
///
/// 每个分支经 `Receiver: Clone` 获得独立接收端（各自 seen 状态，多分支
/// 并发等待互不干扰）。
#[derive(Clone)]
struct CancelToken {
    rx: tokio::sync::watch::Receiver<bool>,
}

impl CancelToken {
    /// 是否已取消（发送端已广播 `true`）。
    fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }
}

/// 取消哨兵错误：`SysError::Other(125)`（ECANCELED=125，「操作被取消」语义）。
/// 冻结面 `SysError` 无专用取消变体（14+Other，pdr.md §10.1），复用
/// `Other(125)` 并在文档注明。该错误只在 Timeout 取消传播的子树内出现，
/// 由 Timeout 臂消费（丢弃 inner 结果后执行 on_timeout），通常不逃逸到用户；
/// 仅当用户在取消子树内显式 `Catch` 时才可观察（handler 执行一次后，下个
/// action 仍被取消——有界）。
const CANCELLED_ERR: SysError = SysError::Other(125);

/// 取消后等待并行分支 join 的宽限（取消传播协议，RFC-08/09）：
/// 响应取消的分支（下一 op 边界检查）应毫秒级 join；仅阻塞于不可取消 IO
/// （如 TcpRead 无数据）的分支会耗尽宽限——此时丢弃 inner（与旧语义近似：
/// 该分支成为孤儿，但取消标志粘性使它在 IO 完成后于下一 op 边界中止），
/// 已入栈 undo 与线性标记仍按超时路径回滚。
const CANCEL_JOIN_GRACE: Duration = Duration::from_millis(500);

/// 递归深度守卫阈值（RFC-11 修复，A2 批 7，CTO 批准 96；迭代 1 复测裁决 64——取消传播帧膨胀，见 resource-notes RFC-11 段）。
///
/// `run_sub_impl` → `interpret_impl` 每层递归在 debug 下消耗 ~13-20KB 栈帧
/// （Windows 默认 2MB 测试线程栈）。**实测崩溃边界**：本机（Windows debug，
/// 2MB 栈）深度 100/104 通过、108 即 STATUS_STACK_OVERFLOW 进程级 abort
/// （R4c 审计记录 ~110-120 同量级）；release 深度 1000 同样溢出；Linux 8MB
/// 栈约 3-4 倍余量。不受信任蓝图 ~百层嵌套即可使宿主进程崩溃（拒绝服务面）。
///
/// 守卫在 `interpret_impl` 递归入口检查，超限返回可捕获错误替代栈溢出。
/// 阈值初取 96（比实测边界 ~104 留 ~8% 余量）；迭代 1 复测（取消传播帧膨胀实测 80 OK/88 崩）裁决 64。
/// 波动，保留安全边际），且 r4c 深度 64 安全回归不受影响；统一保守取值保证
/// 最弱平台（Windows 2MB 栈）安全 —— Linux 8MB 栈下 64 帧余量更大。
///
/// **保证范围（文档化限制）**：守卫保证限于 **≥2MB 栈**（Rust 测试线程/tokio
/// worker/`std::thread::spawn` 默认 2MB）——64 帧 × ~20-23KB ≈ 1.3-1.5MB，在
/// 2MB 栈下留有余量；若宿主在更小栈（如 1MB 主线程栈：实测崩溃边界 ~50-54 帧）
/// 上执行深嵌套蓝图，**正确缓解**：a. 链接器 /STACK 提升主线程栈；b. 将解释器
/// 运行在 spawn 线程（受 `RUST_MIN_STACK` 控制）；c. Catch Other(105)。注意
/// `RUST_MIN_STACK` 只影响 `std::thread::spawn` 新线程、**不影响主线程**（审查
/// 修正，与 spec/resource-notes.md RFC-11 一致）。否则属用户责任（阈值不随栈
/// 尺寸动态调整，迭代 1 裁决后保持 64）。
const MAX_NESTING_DEPTH: usize = 64;

/// 深度超限错误：`SysError::Other(105)`（ENOBUFS=105，「嵌套资源耗尽」语义
/// 近似）。无专用哨兵变体 —— `SysError` 冻结为 14+Other（pdr.md §10.1），
/// 此处复用 Other(105) 并在文档注明。
const NESTING_DEPTH_EXCEEDED: SysError = SysError::Other(105);

/// GetTime 虚拟化辅助（审计 R1 契约-F2）：virtual-clock 下 `DataOp::GetTime`
/// 读逻辑时钟（确定性重放承诺，pdr.md §12.1）——不达物理执行器、不推进
/// 时钟（读取非推进）；墙钟路径恒 None。
///
/// 栈帧纪律：`#[inline(never)]` + Option<Value> 临时不落入解释器帧 ——
/// RFC-11 深度守卫的嵌套边界取决于每帧栈用量（r5a 边界测试 63/64/65），
/// 解释器帧越大边界越低；提取后 Syscall 臂只做 match 操作数调用。
#[cfg(feature = "virtual-clock")]
#[inline(never)]
fn virtual_get_time(ctx: &mut Context, op: &DataOp) -> Option<Value> {
    if matches!(op, DataOp::GetTime) && ctx.virtual_clock_mut().is_some() {
        ctx.virtual_clock_mut()
            .map(|vc| Value::U64(vc.now().as_millis() as u64))
    } else {
        None
    }
}

/// 非 virtual-clock 构建的占位（匹配 Syscall 臂的调用点，恒 None）。
#[cfg(not(feature = "virtual-clock"))]
#[inline(never)]
fn virtual_get_time(_ctx: &mut Context, _op: &DataOp) -> Option<Value> {
    None
}

/// 虚拟时钟下 `Action::Timeout` 的判定实现（审计 R1 红灯根因修复，时域统一）：
/// **双通道判定**——墙钟通道（`tokio::time::timeout`，防御真实执行超时：慢
/// syscall/IO）与虚拟通道（inner 完成后虚拟流逝 ≥ deadline，覆盖 Sleep 的
/// 瞬时虚拟推进），任一超限即执行 on_timeout：
/// - `Sleep(10s)` 内层（虚拟推进 10s ≥ 50ms）→ 虚拟通道触发（红灯
///   `err_timeout_keeps_undo_stack_and_registry` 的场景，此前墙钟竞速虚拟
///   Sleep 永不触发）；
/// - 慢 syscall（墙钟 100ms > 10ms）→ 墙钟通道触发（原 `timeout_fires_on_timeout`
///   语义，VC 下保持）；
/// - 瞬时完成的 inner（虚拟 0ms < deadline）→ 返回 inner 结果（错误原样透传）。
///
/// 注意：墙钟通道有**回滚语义**（真实超时丢弃飞行中 future 后回滚其已入栈
/// undo 与线性标记——审计 R3 修复，RFC-08/09/12 残余的锁/标记面）；但 inner
/// 已被丢弃、无法 join，并行分支成为孤儿继续执行（残余缺口：宽限耗尽路径
/// 的既有效果回收，见 resource-notes §11 R7-A/B）。虚拟通道无「飞行中」
/// 状态、无取消、效果保留（post-check）。
///
/// 栈帧纪律：deadline/elapsed 跨 await 存活，必须放在本独立 async fn 内 ——
/// 若内联进 `interpret_impl` 状态机，会撑大每层递归的状态机帧、压低 RFC-11
/// 深度守卫的嵌套边界（r5a 边界测试 63/64/65 实测：内联时 63 层即栈溢出）。
/// 返回 `LocalBoxFuture`（`Box::pin` 堆分配）：解释器状态机帧只存 8B 指针，
/// 子状态机（含 deadline/elapsed）在堆上，不参与每层递归的栈帧预算。
#[cfg(feature = "virtual-clock")]
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn run_virtual_timeout<'a>(
    inner: Action,
    on_timeout: Action,
    duration: Duration,
    ctx: &'a mut Context,
    undo: &'a mut UndoStack,
    reg: &'a mut ResourceRegistry,
    access: ExecAccess<'a>,
    depth: usize,
    cancel: Option<&'a mut CancelToken>,
) -> LocalBoxFuture<'a, Result<Value, SysError>> {
    Box::pin(async move {
        let mut access = access;
        let mut cancel = cancel;
        let t0 = match ctx.virtual_clock_mut() {
            Some(vc) => vc.now(),
            // 无时钟（理论不可达：Context::new 恒 Some）：退化为纯墙钟路径。
            None => {
                return run_sub_impl(
                    inner,
                    ctx,
                    undo,
                    reg,
                    access.reborrow(),
                    depth,
                    cancel.as_deref_mut(),
                )
                .await
            }
        };
        let deadline = t0.saturating_add(duration);
        // 审计 R3 修复（VC 墙钟通道回滚）：inner 前记录 undo/线性快照——
        // 墙钟通道真实超时（慢 syscall）时 inner 已被丢弃、无法 join，但已
        // 入栈 undo 与预插线性标记可回滚（RFC-08/09/12 残余：锁释放、标记
        // 不残留、同路径可重试）。虚拟通道（post-check）效果保留不受影响。
        let undo_mark = undo.len();
        let linear_snap = reg.snapshot_linear();
        match tokio::time::timeout(
            duration,
            run_sub_impl(
                inner,
                ctx,
                undo,
                reg,
                access.reborrow(),
                depth,
                cancel.as_deref_mut(),
            ),
        )
        .await
        {
            Err(_elapsed) => {
                // 墙钟通道超时：inner 已 drop（无分支可 join），回滚其已入栈
                // undo 与线性标记后执行 on_timeout。
                // 语义真回归：回滚失败（撤销 Err）→ 传播错误，不执行 on_timeout
                // （状态未恢复时执行替代方案会在不一致基础上继续）。
                undo.rollback_from(undo_mark).await?;
                reg.rollback_linear_to(&linear_snap);
                run_sub_impl(
                    on_timeout,
                    ctx,
                    undo,
                    reg,
                    access.reborrow(),
                    depth,
                    cancel.as_deref_mut(),
                )
                .await
            }
            Ok(r) => {
                let elapsed = ctx.virtual_clock_mut().map(|vc| vc.now()).unwrap_or(t0);
                if elapsed >= deadline {
                    run_sub_impl(on_timeout, ctx, undo, reg, access.reborrow(), depth, cancel).await
                } else {
                    r
                }
            }
        }
    })
}
/// 递归执行子 Action（async fn 不可直接自递归，统一 `Box::pin`）。
/// 非 Send 约束同 `LocalBoxFuture`（见上）。
///
/// `depth`：当前递归深度（解释器维护的嵌套深度计数器，RFC-11 守卫载体）。
/// 调用方传入自身深度，本函数以 `depth + 1` 进入子解释器 —— 递归入口处
/// 深度 +1，超 `MAX_NESTING_DEPTH` 时 `interpret_impl` 返回可捕获错误。
///
/// `cancel`：取消传播协议（RFC-08/09/12 残余）的取消令牌（可空——非
/// Timeout 子树为 `None`），向子解释器传递取消域。
fn run_sub_impl<'a>(
    action: Action,
    ctx: &'a mut Context,
    undo: &'a mut UndoStack,
    reg: &'a mut ResourceRegistry,
    access: ExecAccess<'a>,
    depth: usize,
    cancel: Option<&'a mut CancelToken>,
) -> LocalBoxFuture<'a, Result<Value, SysError>> {
    Box::pin(async move { interpret_impl(action, ctx, undo, reg, access, depth + 1, cancel).await })
}

/// 可取消 Sleep（取消传播协议，RFC-08/09/12 残余）：已取消 → 立即返回
/// （由调用方循环顶判定返回 CANCELLED_ERR）；未取消 → sleep 与取消信号竞速，
/// 取消先到则提前醒来。`changed()` 在值自上次观察后已变更时立即完成（无
/// 丢失唤醒窗口）；发送端丢弃时返回 Err，同样提前醒来交由循环顶判定
/// （取消标志粘性保证正确性）。
///
/// **提取为独立 async fn 的原因**：`tokio::select!` 会生成大量轮询期栈
/// 临时量；若内联在 `interpret_impl` 的 match 臂内，会放大解释器每层递归
/// 的轮询栈帧，压缩 RFC-11 深度守卫（阈值 64，Windows 2MB 栈）的余量
/// （实测边界从 ~104 降至 ~92 的回归）。独立函数把 select! 轮询栈隔离到
/// 自身 coroutine，解释器轮询帧保持精简。
#[cfg(not(feature = "virtual-clock"))]
async fn cancellable_sleep(duration: Duration, token: &mut CancelToken) {
    if token.is_cancelled() {
        return;
    }
    tokio::select! {
        _ = tokio::time::sleep(duration) => {}
        _ = token.rx.changed() => {}
    }
}

/// 超时等待（取消传播协议核心，RFC-08/09/12 残余）：
///
/// 返回 `(是否超时, inner 结果)`：
/// - `false`：inner 在期限内完成（效果保留，原语义）；
/// - `true`：已广播取消并完成分支 join 宽限等待——inner 结果被丢弃，
///   调用方负责回滚 undo 与线性标记后执行 on_timeout。
///
/// 超时触发流程：先广播取消（watch 令牌，并行 Fork 分支在下一 op 边界
/// 检查并快速返回、把部分 registry/undo 合并回父），再有界宽限
/// （`CANCEL_JOIN_GRACE`）等待 inner join——分支阻塞于不可取消 IO 时耗尽
/// 宽限后返回 `CANCELLED_ERR`（近似同旧语义，但取消标志粘性使该分支在
/// IO 完成后于下一 op 边界中止）。
///
/// `outer`（审计 R3-B 修复）：嵌套 Timeout 的外层取消接收端——外层广播
/// 经本 OR 臂打断本层 wait（事件驱动、两跳亚毫秒），取消穿透结构化嵌套；
/// 无外层（顶层 Timeout）时该臂恒 pending。
///
/// **提取为独立 async fn 的原因**同 `cancellable_sleep`：隔离 `select!`
/// 轮询栈帧，保护 RFC-11 深度守卫的栈预算。
async fn wait_timeout<'a>(
    mut inner: LocalBoxFuture<'a, Result<Value, SysError>>,
    duration: Duration,
    cancel_tx: &tokio::sync::watch::Sender<bool>,
    outer: Option<&tokio::sync::watch::Receiver<bool>>,
) -> (bool, Result<Value, SysError>) {
    let sleep = tokio::time::sleep(duration);
    tokio::pin!(sleep);
    // 外层取消 OR 臂：有外层令牌时等其 changed()（粘性：已广播则立即 Ready）。
    // changed() 需 &mut self——克隆接收端（Clone：独立 seen 状态，互不干扰）。
    let cancel_outer = async {
        match outer {
            Some(o) => {
                let mut o = o.clone();
                let _ = o.changed().await;
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(cancel_outer);
    // 单次 select：各臂均直接返回（clippy never_loop 提示下不包 loop——
    // 语义等价：inner 先完成 → 未超时；sleep/外层取消先到 → 广播后宽限等待）。
    tokio::select! {
        r = &mut inner => (false, r),
        _ = &mut sleep => {
            // 广播取消：并行 Fork 分支检查后快速返回（join 见下）。
            let _ = cancel_tx.send(true);
            // 有界宽限：等待并行分支把部分状态/undo 合并回父。
            let grace = tokio::time::sleep(CANCEL_JOIN_GRACE);
            tokio::pin!(grace);
            tokio::select! {
                r = &mut inner => (true, r),
                _ = &mut grace => (true, Err(CANCELLED_ERR)),
            }
        }
        _ = &mut cancel_outer => {
            // 外层取消（R3-B）：嵌套 Timeout 被外层广播打断——同超时路径：
            // 广播自身取消 + 有界宽限 join。
            let _ = cancel_tx.send(true);
            let grace = tokio::time::sleep(CANCEL_JOIN_GRACE);
            tokio::pin!(grace);
            tokio::select! {
                r = &mut inner => (true, r),
                _ = &mut grace => (true, Err(CANCELLED_ERR)),
            }
        }
    }
}
/// 墙钟 Timeout 取消传播实现（RFC-08/09/12 残余统一修复）。
///
/// 独立 async fn（非解释器状态机内联）：取消协议的局部状态（watch 通道、
/// CancelToken、线性快照）若留在 `interpret_impl` 的 match 臂内，会撑大
/// **每层递归**的状态机帧（RFC-11 深度守卫 63/64/65 边界实测会栈溢出）——
/// 提取后解释器帧只持一个 BoxFuture 指针。VC 路径见 `run_virtual_timeout`。
#[allow(clippy::too_many_arguments)]
async fn run_wall_timeout<'a>(
    inner: Action,
    on_timeout: Action,
    duration: Duration,
    ctx: &'a mut Context,
    undo: &'a mut UndoStack,
    reg: &'a mut ResourceRegistry,
    mut access: ExecAccess<'a>,
    depth: usize,
    cancel: Option<&'a mut CancelToken>,
) -> Result<Value, SysError> {
    // 超时触发时不再直接丢弃 inner future（旧行为：已 spawn 的 Fork 分支
    // 成为孤儿继续执行、持锁分支永不 Unlock、飞行中 Write 的线性标记不
    // 回滚），而是：
    //   a) 先广播取消（watch 令牌）——并行 Fork 分支在下一 op 边界检查并
    //      快速返回，把部分 registry/undo 合并回父（结构化并发近似）；
    //   b) 有界宽限（CANCEL_JOIN_GRACE）等待 inner join——分支阻塞于不可
    //      取消 IO 时耗尽宽限后丢弃 inner（取消标志粘性使该分支在 IO 完成
    //      后于下一 op 边界中止）；
    //   c) 回滚 inner 已入栈 undo（`rollback_from`，异步可含 IO）——RFC-09：
    //      持锁分支的 MutexLock undo 被立即执行，锁与仲裁占坑释放，同 id
    //      立即可重入（不饥饿至 recover）；RFC-08：已合并回父的分支 undo
    //      一并撤销；
    //   d) 回滚 inner 期间新增的 A4 线性标记（`rollback_linear_to` 快照差）
    //      ——RFC-12 残余：飞行中 Write/Own 的预插标记不残留，同路径可重试；
    //   e) 再执行 on_timeout（原语义：inner 结果被丢弃）。
    // 超时前完成的 inner：效果全部保留（原语义，回滚不触发）。
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let mut token = CancelToken { rx: cancel_rx };
    let undo_mark = undo.len();
    let linear_snap = reg.snapshot_linear();
    let inner_fut = run_sub_impl(
        inner,
        ctx,
        undo,
        reg,
        access.reborrow(),
        depth,
        Some(&mut token),
    );
    // 审计 R3-B 修复：外层取消接收端透传给 wait_timeout（嵌套 Timeout 的
    // wait 被外层广播打断——取消穿透结构化嵌套）。外层 token 的 rx 与
    // 本层 token 同源类型（watch::Receiver），直接取 `cancel.rx` 引用。
    let outer_rx = cancel.as_deref().map(|c| &c.rx);
    let (timed_out, inner_result) = wait_timeout(inner_fut, duration, &cancel_tx, outer_rx).await;
    if !timed_out {
        // 超时前完成：inner 效果全部保留（原语义）。
        return inner_result;
    }
    // 超时取消：先回滚 inner 已入栈 undo（异步，可含 IO），
    // 再回滚 inner 新增的线性标记，最后执行 on_timeout。
    // 语义真回归：inner 回滚失败（撤销 Err）→ 传播错误，不执行 on_timeout
    // （状态未恢复时执行替代方案会在不一致基础上继续）。
    drop(inner_result);
    undo.rollback_from(undo_mark).await?;
    reg.rollback_linear_to(&linear_snap);
    run_sub_impl(on_timeout, ctx, undo, reg, access.reborrow(), depth, cancel).await
}

/// 共享执行器通道（CTO 批准方向，D14 阶段 3）：`Runtime` 内部以
/// `Arc<tokio::sync::Mutex<…>>` 包装执行器（公开 API `Runtime::new(Box<dyn>)`
/// 不变）；Fork 并行子任务各持 Arc 克隆，经锁互斥串行化执行器调用（锁仅
/// 保护执行器内部状态，物理 IO 本身异步）。
type SharedExecutor = Arc<tokio::sync::Mutex<Box<dyn SyscallExecutor + Send>>>;

/// 执行器访问通道（运行时内部）：
/// - `Direct`：冻结公共签名 `interpret(action, ..., ex: &mut dyn SyscallExecutor)`
///   路径 —— 无 Fork 并行能力（并行需要共享互斥通道）；
/// - `Shared`：`Runtime::run`/`run_blocking` 路径 —— 每个 Syscall 调用经锁互斥，
///   Fork 并行分支可用（见 `run_fork_parallel`）。迭代 3-A1：携带 Runtime
///   自持 reactor 的 `Handle`（D9：Runtime 自持 reactor；分支经 `Handle::spawn`
///   直接投递到该多线程 reactor 的 worker 线程，替代逐节点 spawn_blocking +
///   current-thread runtime 构建）。
enum ExecAccess<'a> {
    Direct(&'a mut dyn SyscallExecutor),
    Shared {
        executor: SharedExecutor,
        reactor: tokio::runtime::Handle,
    },
}

impl<'a> ExecAccess<'a> {
    /// 重借用：`Direct` 重借用内部 `&mut`，`Shared` 克隆 Arc/Handle（递归/分支复用）。
    fn reborrow<'b>(&'b mut self) -> ExecAccess<'b> {
        match self {
            ExecAccess::Direct(ex) => ExecAccess::Direct(&mut **ex),
            ExecAccess::Shared { executor, reactor } => ExecAccess::Shared {
                executor: executor.clone(),
                reactor: reactor.clone(),
            },
        }
    }
}

/// 经执行器访问通道执行 DataOp。
/// `Shared` 路径：每调用加锁互斥（Fork 并行时两子任务互斥串行化执行器调用）。
/// R-6 锁边界收窄：并行 Fork 分支改用 `run_fork_parallel` 内的**分支执行器快照**
/// （`SyscallExecutor::fork_snapshot`，状态 Arc 共享）独占驱动——物理 IO await 移出
/// 共享锁外真并行；不支持快照的执行器回退本通道（D17 原行为，零语义变化）。
async fn exec_via(
    access: &mut ExecAccess<'_>,
    op: &DataOp,
    reg: &mut ResourceRegistry,
) -> Result<(Value, UndoCapability), SysError> {
    match access {
        ExecAccess::Direct(ex) => (**ex).execute(op, reg).await,
        ExecAccess::Shared { executor, .. } => {
            let mut guard = executor.lock().await;
            guard.execute(op, reg).await
        }
    }
}

/// 经执行器访问通道转发 `watch_signal`（默认 ENOSYS 透传）。
async fn watch_signal_via(
    access: &mut ExecAccess<'_>,
    signal: Signal,
    reg: &mut ResourceRegistry,
) -> Result<Value, SysError> {
    match access {
        ExecAccess::Direct(ex) => (**ex).watch_signal(signal, reg).await,
        ExecAccess::Shared { executor, .. } => {
            let mut guard = executor.lock().await;
            guard.watch_signal(signal, reg).await
        }
    }
}

/// 经执行器访问通道转发 `invoke`（默认 ENOSYS 透传）。
async fn invoke_via(
    access: &mut ExecAccess<'_>,
    foreign_id: Id,
    captures: &ResourceSet,
    deterministic: bool,
) -> Result<Value, SysError> {
    match access {
        ExecAccess::Direct(ex) => (**ex).invoke(foreign_id, captures, deterministic).await,
        ExecAccess::Shared { executor, .. } => {
            let mut guard = executor.lock().await;
            guard.invoke(foreign_id, captures, deterministic).await
        }
    }
}

/// 静态收集子树内所有 `Syscall.resources`（Fork 冲突检测用，D14）。
///
/// 局限：`next`/`handler`/`cond`/`combine` 是不透明闭包，其内部嵌套的
/// Syscall 无法静态看到（闭包内容在编译期不可检查）——阶段 1 接受此近似。
fn collect_syscall_resources(action: &Action, out: &mut ResourceSet) {
    match action {
        Action::Syscall { resources, .. } => out.extend(resources.iter().cloned()),
        Action::Choose {
            then_branch,
            else_branch,
            ..
        } => {
            collect_syscall_resources(then_branch, out);
            collect_syscall_resources(else_branch, out);
        }
        Action::Fork { left, right, .. } => {
            collect_syscall_resources(left, out);
            collect_syscall_resources(right, out);
        }
        Action::Scope { inner, .. } => collect_syscall_resources(inner, out),
        Action::Replace { target } => collect_syscall_resources(target, out),
        Action::Timeout {
            action, on_timeout, ..
        } => {
            collect_syscall_resources(action, out);
            collect_syscall_resources(on_timeout, out);
        }
        Action::Catch { action, .. } => collect_syscall_resources(action, out),
        Action::Sequential { current, .. } => collect_syscall_resources(current, out),
        // D-0xx 幂等：inner 的副作用资源纳入冲突收集（键命中时 inner 不执行，
        // 但静态判定保守——声明冲突仍可并行性判定）。
        Action::Idempotent { inner, .. } => collect_syscall_resources(inner, out),
        // 审计 R7-D 修复：Invoke.captures 声明资源纳入静态冲突收集——此前
        // 落 `_ => {}` 被静默丢弃，Fork 分支含 Invoke 且 captures 与兄弟分支
        // 冲突时 can_parallel 误判 true → 真并行（captures 不经 check_linear，
        // 影响限于 executor 侧物理效果冲突不可静态判定）。
        Action::Invoke { captures, .. } => out.extend(captures.iter().cloned()),
        _ => {}
    }
}

/// 收集 Action 树中的幂等键（D-103）：遍历找 `Action::Idempotent` 的 key。
/// **注意**：只遍历可见 AST 节点——`next`/`cond`/`combine`/`handler` 闭包内的
/// Idempotent 不可见（与 `collect_syscall_resources` 的资源盲区同族，R3 审计；
/// 闭包内同 key + Fork 并行时"恰好一次"可能被双执行——已知模式，阶段 1 接受）。
fn collect_idempotency_keys(action: &Action, out: &mut std::collections::HashSet<String>) {
    match action {
        Action::Idempotent { key, inner, .. } => {
            out.insert(key.clone());
            collect_idempotency_keys(inner, out);
        }
        Action::Choose {
            then_branch,
            else_branch,
            ..
        } => {
            collect_idempotency_keys(then_branch, out);
            collect_idempotency_keys(else_branch, out);
        }
        Action::Fork { left, right, .. } => {
            collect_idempotency_keys(left, out);
            collect_idempotency_keys(right, out);
        }
        Action::Scope { inner, .. } => collect_idempotency_keys(inner, out),
        Action::Replace { target } => collect_idempotency_keys(target, out),
        Action::Timeout {
            action, on_timeout, ..
        } => {
            collect_idempotency_keys(action, out);
            collect_idempotency_keys(on_timeout, out);
        }
        Action::Catch { action, .. } => collect_idempotency_keys(action, out),
        Action::Sequential { current, .. } => collect_idempotency_keys(current, out),
        _ => {}
    }
}

/// Fork 静态冲突检测（D14 + D-103）：收集左右子树 Syscall 资源，查询冲突矩阵；
/// 且两分支含**同幂等键** → 冲突（串行，防止并行同 key 重复执行——幂等键
/// 语义要求"副作用只发生一次"，并行下两个分支都可能查到未 COMMITTED 都执行）。
///
/// `can_parallel=false`（冲突）→ 顺序执行；`can_parallel=true` →
/// `Runtime` 路径（Shared 通道）下真并行（`run_fork_parallel`）。
pub fn fork_conflict(reg: &ResourceRegistry, left: &Action, right: &Action) -> bool {
    let mut l_res = ResourceSet::new();
    let mut r_res = ResourceSet::new();
    collect_syscall_resources(left, &mut l_res);
    collect_syscall_resources(right, &mut r_res);
    if !reg.can_parallel(&l_res, &r_res) {
        return true;
    }
    // D-103：幂等键冲突——同 key 的幂等段在两个分支并行会破坏"恰好一次"。
    let mut l_keys = std::collections::HashSet::new();
    let mut r_keys = std::collections::HashSet::new();
    collect_idempotency_keys(left, &mut l_keys);
    collect_idempotency_keys(right, &mut r_keys);
    !l_keys.is_disjoint(&r_keys)
}

/// Fork 分支 fd 全局唯一区间序号（HIGH 嵌套碰撞修复，CTO 批准方向）。
///
/// 每次进入 Fork 右分支（无论并行/顺序、任意嵌套深度）`fetch_add(1)` 取全局
/// 唯一序号 k ∈ [1, 2^16)，区间偏移 = `k << 48`（从 1 起跳：0 偏移会与继承
/// 基线的左分支同区）。区间绝对位置 = 偏移 + **未偏移基线**（见
/// `ResourceRegistry::offset_next_fd` 的基线复用：本 registry 已记录
/// `fork_region` 时沿用旧基线，而非当前已偏移游标）—— 只由全局唯一 k 与根
/// 基线决定，**与路径无关**：任意嵌套深度下所有并发分支区间互斥（修复批 5
/// 「相对当前 next_fd 偏移 2^48」在嵌套 Fork 下内层右分支与外层右分支同基线
/// 碰撞的 HIGH；基线复用同时消除「两条路径的序号和相等」的累加碰撞）。
///
/// 上限 2^16 区间：2^16 × 2^48 = 2^64 恰满 u64 地址空间，序号不溢出；区间
/// 基数 2^48 远超实际 fd 规模（分配数 ≪ 2^48），且 `merge` 归一化（未分配
/// 区间游标收敛回基线）使父 `next_fd` 不被抬高；右分支**实际分配**时
/// `merge` 锚点吸收（RFC-06 修复）使后续轮次偏移锚定根基线，父 `next_fd`
/// 线性增长而非 Σk·2^48 二次增长。**注意（审查 LOW-2）：序号
/// 进程级全局、永不回收**——每进程每次 Fork 右分支（含顺序路径）+1，超过
/// 2^16-1 次即 assert panic（fail-fast 硬限制，长驻高频 Fork 进程的理论边界）。
/// 进程级全局
/// （`static AtomicU64`）：多 Runtime 并存时区间消耗共享，正确性无碍（仅序号
/// 消耗更快）；`interpret` 冻结签名不带 Runtime 引用，static 是唯一不破坏
/// 签名冻结的选择。
static FORK_FD_REGION_SEQ: AtomicU64 = AtomicU64::new(1);

/// 取下一个全局唯一 fd 区间偏移（`k << 48`，k 全局唯一递增）。见
/// [`FORK_FD_REGION_SEQ`] 的注释（上限 2^16 区间，超出即 u64 溢出）。
///
/// 审计（收敛轮）：耗尽从 assert panic（进程级 abort、Catch 不可捕获，与
/// RFC-11 修复前同类拒绝服务面）改为**可捕获错误** `Other(28)`（ENOSPC
/// 「fd 区间空间耗尽」语义近似；`SysError` 冻结 14+Other，无专用哨兵）——
/// 不受信任蓝图批量构造 Fork 不再能崩溃宿主进程。
///
/// 纯函数（测试直接构造 k 验证边界，不消耗全局 static 预算）。
fn fork_region_offset_for(k: u64) -> Result<u64, SysError> {
    if k >= (1 << 16) {
        return Err(SysError::Other(28));
    }
    Ok(k << 48)
}

fn fork_fd_region_offset() -> Result<u64, SysError> {
    let k = FORK_FD_REGION_SEQ.fetch_add(1, Ordering::Relaxed);
    fork_region_offset_for(k)
}

/// R-6：分支执行器访问通道（锁内短临界区取快照）。
///
/// 锁内调用 `fork_snapshot`：
/// - `Some(branch_ex)`：为分支建独立通道（Arc<Mutex> 包裹快照）——分支独占
///   驱动，物理 IO 移出共享锁外真并行；嵌套 Fork 在分支通道上递归取快照，
///   任意深度保持并行（`interpret_impl` 的 `parallel` 判定要求 Shared 通道）；
/// - `None`（默认）：回退共享通道克隆（D17 原行为——分支经同一锁互斥串行化，
///   Mock/自定义执行器调用序列语义不变）。
///
/// 快照与父共享全部内部状态（per-fd 锁表 / 映射 / 仲裁器），分支的映射变更
/// 自动可见于父，无需合并步骤；同一 fd 的物理 IO 仍在共享 per-fd 锁上串行。
async fn branch_snapshot_access(
    shared: &SharedExecutor,
    reactor: &tokio::runtime::Handle,
) -> ExecAccess<'static> {
    let mut guard = shared.lock().await;
    match guard.fork_snapshot() {
        Some(branch_ex) => {
            let channel: SharedExecutor = Arc::new(tokio::sync::Mutex::new(branch_ex));
            ExecAccess::Shared {
                executor: channel,
                reactor: reactor.clone(),
            }
        }
        None => ExecAccess::Shared {
            executor: shared.clone(),
            reactor: reactor.clone(),
        },
    }
}

/// Fork 并行分支产物：(分支结果, 隔离 registry, 独立撤销栈, 隔离 ctx)。
/// 由 `run_fork_parallel` 的 spawn_blocking 任务带回，供完成后合并回父。
type BranchOutcome = (
    Result<Value, SysError>,
    ResourceRegistry,
    UndoStack,
    Context,
);

/// Fork 并行分支 join 合并守卫（R7-B 修复，A3 核销）。
///
/// 职责：轮询两个分支 `JoinHandle`，分支**完成即合并**隔离状态回父——
/// - 合并顺序保持 [left, right]（与顺序路径/原实现一致）：右分支先完成而
///   左分支未完成时暂存于 `r_stash`，待左分支合并后按序补合并；
/// - 整个 Fork future 被丢弃（Timeout 宽限耗尽 / VC 墙钟通道超时）时，
///   `Drop` 把「已完成但未合并」的分支状态合并回父——不再随局部变量丢失。
///   （旧行为：两分支都 await 完才合并；宽限耗尽丢弃 inner future → 已完成
///   持锁分支的释放 undo 随之消失 → arbiter 占坑 + 物理锁永久残留，A3 锁定
///   R7-B：Replace/recover 均够不到，仅显式 MutexUnlock 可逃逸。）
/// - 未完成分支：`JoinHandle` 随守卫丢弃而**脱离运行**（保持取消语义：分支
///   阻塞 IO 完成后于下一 op 边界按取消粘性快速返回），其隔离状态不可达
///   （spawn_blocking 任务结果无处投递）——持锁占坑残余按
///   spec/resource-notes.md R7-B 登记处理（部分修复：join 路径闭、耗尽路径
///   登记）。
struct ForkJoinMerge<'a> {
    reg: &'a mut ResourceRegistry,
    undo: &'a mut UndoStack,
    /// 父 ctx（virtual-clock 合并用；墙钟构建下无需读取，仅持有以保签名统一）。
    #[cfg_attr(not(feature = "virtual-clock"), allow(dead_code))]
    ctx: &'a mut Context,
    l_task: tokio::task::JoinHandle<BranchOutcome>,
    r_task: tokio::task::JoinHandle<BranchOutcome>,
    /// 左分支已合并（左分支完成即合并，不依赖右分支状态）。
    l_merged: bool,
    /// 右分支已完成、待按序合并（左分支未完成时暂存；Fork future 被丢弃时
    /// 由 `Drop` 合并回父）。
    r_stash: Option<BranchOutcome>,
    /// Fork 时父时钟基线（virtual-clock 合并用；分支时钟同源克隆自此基线）。
    #[cfg(feature = "virtual-clock")]
    clock_base: Duration,
}

impl<'a> ForkJoinMerge<'a> {
    fn new(
        reg: &'a mut ResourceRegistry,
        undo: &'a mut UndoStack,
        ctx: &'a mut Context,
        l_task: tokio::task::JoinHandle<BranchOutcome>,
        r_task: tokio::task::JoinHandle<BranchOutcome>,
    ) -> Self {
        #[cfg(feature = "virtual-clock")]
        let clock_base = ctx
            .virtual_clock_mut()
            .map(|vc| vc.now())
            .unwrap_or_default();
        Self {
            reg,
            undo,
            ctx,
            l_task,
            r_task,
            l_merged: false,
            r_stash: None,
            #[cfg(feature = "virtual-clock")]
            clock_base,
        }
    }

    /// 轮询两分支至全部完成，返回 (left 结果, right 结果)。
    /// Fork future 被丢弃（宽限耗尽）时本 future 被 drop → `Drop` 把已完成
    /// 但未合并的分支状态合并回父（R7-B）。
    async fn join(mut self) -> (Result<Value, SysError>, Result<Value, SysError>) {
        let (mut l_res, mut r_res) = (None, None);
        loop {
            if l_res.is_some() && r_res.is_some() {
                break;
            }
            tokio::select! {
                res = &mut self.l_task, if l_res.is_none() => {
                    let (v, l_reg, l_undo, mut l_ctx) =
                        res.expect("Fork 并行左分支任务 panic");
                    l_res = Some(v);
                    self.merge_left(l_reg, l_undo, &mut l_ctx);
                    // 右分支若已先完成（暂存中）→ 按序补合并。
                    if let Some((rv, r_reg, r_undo, mut r_ctx)) = self.r_stash.take() {
                        r_res = Some(rv);
                        self.merge_right(r_reg, r_undo, &mut r_ctx);
                    }
                }
                res = &mut self.r_task, if r_res.is_none() && self.r_stash.is_none() => {
                    let outcome = res.expect("Fork 并行右分支任务 panic");
                    if self.l_merged {
                        let (v, r_reg, r_undo, mut r_ctx) = outcome;
                        r_res = Some(v);
                        self.merge_right(r_reg, r_undo, &mut r_ctx);
                    } else {
                        self.r_stash = Some(outcome);
                    }
                }
            }
        }
        (
            l_res.expect("left 分支已完成"),
            r_res.expect("right 分支已完成"),
        )
    }

    /// 合并左分支（完成即合并；顺序 [left, right]）。
    fn merge_left(&mut self, l_reg: ResourceRegistry, l_undo: UndoStack, l_ctx: &mut Context) {
        self.reg.merge(l_reg);
        self.undo.append(l_undo);
        self.merge_clock(l_ctx);
        self.l_merged = true;
    }

    /// 合并右分支。
    fn merge_right(&mut self, r_reg: ResourceRegistry, r_undo: UndoStack, r_ctx: &mut Context) {
        self.reg.merge(r_reg);
        self.undo.append(r_undo);
        self.merge_clock(r_ctx);
    }

    /// 分支虚拟时钟推进合并回父（sum，与顺序路径「分支依次推进父时钟」观察
    /// 等价；审计 R1 状态-MEDIUM-1 修复的逐分支版——基线取 Fork 时父时钟
    /// `clock_base`，避免「先合并分支的推进被误作下一分支的基线」致总和偏小）。
    #[cfg(feature = "virtual-clock")]
    fn merge_clock(&mut self, branch: &mut Context) {
        let base = self.clock_base;
        let now = branch
            .virtual_clock_mut()
            .map(|vc| vc.now())
            .unwrap_or(base);
        if let Some(vc) = self.ctx.virtual_clock_mut() {
            vc.advance(now.saturating_sub(base));
        }
    }

    #[cfg(not(feature = "virtual-clock"))]
    fn merge_clock(&mut self, _branch: &mut Context) {}
}

impl Drop for ForkJoinMerge<'_> {
    fn drop(&mut self) {
        // Fork future 被丢弃（Timeout 宽限耗尽 / VC 墙钟通道超时）：把已完成
        // 但未合并的分支状态合并回父（R7-B）——只有右分支可能暂存（左分支
        // 完成即合并）。未完成分支的 JoinHandle 随守卫字段丢弃 → 脱离运行，
        // 完成后按取消粘性快速返回（其持锁占坑残余不可达，见登记）。
        if let Some((_, r_reg, r_undo, mut r_ctx)) = self.r_stash.take() {
            self.reg.merge(r_reg);
            self.undo.append(r_undo);
            self.merge_clock(&mut r_ctx);
        }
    }
}

/// Fork 并行分支执行（D14 阶段 3，CTO 批准方向）：
///
/// 两分支各自持有 registry/context 隔离副本（D13：Clone）+ 独立 UndoStack，
/// 经共享执行器通道（Arc<Mutex>，每 Syscall 调用互斥 —— 锁仅保护执行器内部
/// 状态，物理 IO 本身异步）并发驱动。
///
/// ## 迭代 3-A1：分支直接 spawn 到 Runtime 自持 reactor（替代逐节点线程/驱动）
/// R-6 前实现经 `spawn_blocking` × 2 + current-thread runtime `drive` × 2 驱动
/// 分支（每 Fork 节点 2 个阻塞线程 + 2 次 runtime 构建）。迭代 3-A1 将解释器
/// 递归 future Send 化（`LocalBoxFuture` + Send，D19 后全部捕获状态已 Send）
/// 并把 Runtime 自持 reactor 的 `Handle` 经 `ExecAccess::Shared` 传递下来——
/// 分支直接 `Handle::spawn` 到多线程 reactor 的 worker 线程：零新线程、零新
/// runtime 构建，调度为 tokio 任务投递（亚微秒级）。D9 契约保持：分支在 tokio
/// 上下文（Runtime 自持 reactor）内运行；分支深度计数器从 0 起算、栈预算在
/// tokio worker 线程（默认 2MB，与旧阻塞线程同量级）上独立生效（RFC-11 守卫
/// 语义不变）。
///
/// ## R-6 锁边界重构（阶段 3 并行兑现）
/// 每个分支在锁内短临界区取**执行器快照**（`SyscallExecutor::fork_snapshot`，
/// 默认 `None`）：`TokioExecutor` 覆盖返回状态 Arc 共享的独立实例——分支对自身
/// 实例持 `&mut` 无跨分支竞争，物理 IO await 移出共享锁外、跨分支真并行；
/// 快照与父共享全部内部状态（per-fd 锁表 / 映射 / 仲裁器），同一 fd 的物理 IO
/// 仍在共享 per-fd 锁上串行（游标语义不变），映射变更经共享状态表自动可见于父
/// （无需合并步骤）；不支持快照的执行器（None）回退共享锁通道（D17 原行为，
/// Mock 测试的调用序列语义不变）。
///
/// 完成后合并回父（R7-B 修复：分支**完成即合并**，见 `ForkJoinMerge`）：
/// - registry：子 handles 以原 fd 并入 + consumed/owned_consumed 并集 +
///   `next_fd = max` 归一化（`ResourceRegistry::merge`，RFC-A3-2 / D13「合并
///   回父」）；F1 修复：spawn 前右分支 `offset_next_fd(fork_fd_region_offset())`
///   取全局唯一 fd 区间（S6/A2 嵌套修复：任意嵌套深度并发分支互斥），
///   合并后父 `next_fd = max(父, 左, 右)`（= 全部已分配 fd 最大值 + 1，D1
///   单调不复用）;
/// - undo：并入顺序为 left 后 right（栈序与顺序路径一致）—— LIFO recover
///   先执行 right 的逆操作再执行 left 的（right 的效果后发生、先撤销，
///   与「left 先执行」的观察序一致）。
///
/// 子任务错误：两分支均跑完后仍合并 registry/undo（部分效果可被外层
/// Catch/recover 撤销），再返回错误（left 优先）。
///
/// 取消传播（RFC-08/09）：`cancel` 为所在 Timeout 域的取消令牌（可空）。
/// 分支把令牌克隆进 spawn 任务——取消广播后，分支在下一 op 边界检查到并
/// 快速返回（部分 registry/undo 照常合并回父，由 Timeout 臂统一回滚），
/// 实现「分支取消时传播给并行子任务」的结构化并发近似。超时路径丢弃 inner
/// future 时分支任务分离（orphan 继续执行，下一 op 边界经取消标志中止）——
/// 与旧 `spawn_blocking` 的分离语义一致（任务未被 join，runtime drop 同样
/// 等待在途任务/阻塞任务收尾）。
///
/// **返回 `LocalBoxFuture`（Send）而非 async fn 的原因**：`interpret_impl`
/// （async fn，opaque）与 `run_fork_parallel` 互为递归——若本函数也是 opaque
/// async fn，其 future 的 Send 判定与 `interpret_impl` 的 Send 判定构成
/// 循环义务（E0391）。Box 化（`Pin<Box<dyn Future + Send>>`）把 Send 边界
/// 固化在 trait object 上，切断循环（与 `run_sub_impl` 同模式）。
#[allow(clippy::too_many_arguments)]
fn run_fork_parallel<'a>(
    left: Action,
    right: Action,
    ctx: &'a mut Context,
    reg: &'a mut ResourceRegistry,
    undo: &'a mut UndoStack,
    shared: SharedExecutor,
    reactor: tokio::runtime::Handle,
    cancel: Option<&'a mut CancelToken>,
) -> LocalBoxFuture<'a, Result<(Value, Value), SysError>> {
    Box::pin(async move {
        // 子任务隔离副本（D13）与独立撤销栈。
        let mut l_ctx = ctx.clone();
        let mut r_ctx = ctx.clone();
        let mut l_reg = reg.clone();
        let mut r_reg = reg.clone();
        // F1 修复：spawn 前取全局唯一 fd 区间 —— 右分支偏移 `k<<48`（k 全局唯一，
        // 见 `FORK_FD_REGION_SEQ` 注释），任意嵌套深度下并发分支区间互斥。
        r_reg.offset_next_fd(fork_fd_region_offset()?);
        let mut l_undo = UndoStack::new();
        let mut r_undo = UndoStack::new();
        let l_shared = shared.clone();
        let r_shared = shared.clone();
        // 取消令牌克隆进分支（watch Receiver 克隆：每分支独立 seen 状态，
        // 并发等待互不干扰；取消广播后分支在下一 op 边界快速返回）。
        let mut l_cancel = cancel.map(|c| CancelToken::clone(c));
        let mut r_cancel = l_cancel.clone();

        // R-6：分支执行器快照（锁内短临界区，O(1) Arc 克隆）。
        // - 快照成功（Some）：分支经独立通道独占驱动（物理 IO 真并行，嵌套 Fork
        //   在分支通道上递归快照，任意深度保持并行）；
        // - 快照 None（默认）：回退共享通道（D17 原行为，Mock/自定义执行器不变）。
        let l_access = branch_snapshot_access(&l_shared, &reactor).await;
        let r_access = branch_snapshot_access(&r_shared, &reactor).await;

        // 两个分支任务直接投递到 Runtime 自持 reactor（worker 线程并发驱动；
        // 执行器调用经锁互斥串行化 —— 回退通道；快照通道无共享锁竞争）。子任务把
        // （结果, 隔离 registry, 独立撤销栈, 隔离 ctx）带回，供完成后合并 —— ctx
        // 带回用于虚拟时钟合并（审计 R1 状态-MEDIUM-1，见下）。分支深度计数器从
        // 0 重新起算（tokio worker 线程独立栈预算；RFC-11 守卫按线程栈独立生效，
        // 与旧 spawn_blocking 全新阻塞线程一致）。
        let l_task = reactor.spawn(async move {
            let v = interpret_impl(
                left,
                &mut l_ctx,
                &mut l_undo,
                &mut l_reg,
                l_access,
                0,
                l_cancel.as_mut(),
            )
            .await;
            (v, l_reg, l_undo, l_ctx)
        });
        let r_task = reactor.spawn(async move {
            let v = interpret_impl(
                right,
                &mut r_ctx,
                &mut r_undo,
                &mut r_reg,
                r_access,
                0,
                r_cancel.as_mut(),
            )
            .await;
            (v, r_reg, r_undo, r_ctx)
        });

        // R7-B 修复（A3 核销）：分支完成即合并——轮询两个 JoinHandle（见
        // ForkJoinMerge），任一分支完成立即把隔离 registry/undo 合并回父
        // （合并顺序保持 [left, right]，与顺序路径一致）。旧行为两分支都
        // await 完才合并：Timeout 宽限耗尽丢弃 Fork future 时，已完成分支
        // （如已持锁）的状态随局部变量一并丢弃 → 释放 undo 永不并入父 →
        // arbiter 占坑 + 物理锁永久残留（Replace/recover 够不到，A3 锁定）。
        // 未完成分支在丢弃时保持取消语义（JoinHandle 丢弃 → 脱离运行，阻塞
        // IO 完成后按取消粘性快速返回；持锁占坑残余见 resource-notes R7-B）。
        let joiner = ForkJoinMerge::new(reg, undo, ctx, l_task, r_task);
        let (l_res, r_res) = joiner.join().await;
        Ok((l_res?, r_res?))
    })
}

/// 解释器内核：Action AST → 运行时语义（A2 交付，`interpret_impl`）。
///
/// 语义要点（pdr.md §2.1 / §4 / §5.1，contracts.md D2/D10/D11/D14）：
/// - 主循环为 trampoline；`cur`（初始 `Unit`）作为贯穿节点的「当前值」，
///   每个节点产出 `(下一 cur, 下一 Action)`；
/// - `Pure`：单位元（公理 A2），直接产生值；
/// - `Syscall`：逐资源 `check_linear`（公理 A4，失败立即返回）→ 执行器执行
///   （经 `access` 通道）→ `Option<UndoOp>` 压入撤销栈 → `next(v)`；物理执行
///   失败时回滚本次预插入的 Write/Own 线性标记（RFC-12，恢复同路径可重试
///   语义），错误原样透传且不压 undo（现有契约）；
/// - `Choose`：`cond(&cur)` 选分支，分支结果继续主循环（A5 分支隔离）；
/// - `Fork`：D14 阶段 3 —— 静态冲突检测（`fork_conflict`）；`can_parallel=true`
///   且为 `Shared` 通道时真并行（`run_fork_parallel`：registry/ctx 隔离 + 独立
///   UndoStack + 共享执行器，完成后合并回父），否则顺序执行（left→right→combine，
///   阶段 1 语义保持）；两条路径完成后均 merge 回父（F2：顺序路径不丢分支 fd/
///   线性标记），且右分支均取全局唯一 fd 区间（F1+S6/A2：任意嵌套深度下
///   两分支不撞 fd）；
/// - `Scope`：cwd 压栈/弹栈（inner 出错时同样恢复）；
/// - `Replace`：先 `recover()`（清空撤销栈）+ `reg.clear()`（释放 handles 与
///   线性标记，next_fd 保留 D1），再执行 target，以其结果结束（D10）；
/// - `Sleep`：feature `virtual-clock` 时推进逻辑时钟（不真实等待），否则真实等待；
/// - `Timeout`：取消传播协议（RFC-08/09/12 残余）——超时触发时先广播取消
///   （watch 令牌，并行 Fork 分支在下一 op 边界检查）、有界宽限等待分支
///   join、回滚 inner 已入栈 undo（`UndoStack::rollback_from`，异步可含 IO）、
///   回滚 inner 新增的 A4 线性标记（`rollback_linear_to` 快照差），再执行
///   on_timeout；超时前完成的 inner 效果全部保留（原语义）；
/// - `Catch`：Err → handler(e)，Ok 原样返回；不触碰撤销栈（recover 语义在
///   Replace/recover 路径）；取消子树内 handler 至多执行一次（下个 action
///   仍被取消，有界）；
/// - `WatchSignal`/`Invoke`：委托执行器；默认执行器返回 ENOSYS
///   （`SysError::Other(38)`），解释器原样透传错误。
///
/// `cancel`：取消传播协议的取消令牌（可空——非 Timeout 子树为 `None`）。
async fn interpret_impl(
    action: Action,
    ctx: &mut Context,
    undo: &mut UndoStack,
    reg: &mut ResourceRegistry,
    mut access: ExecAccess<'_>,
    depth: usize,
    mut cancel: Option<&mut CancelToken>,
) -> Result<Value, SysError> {
    // RFC-11 深度守卫：递归入口检查嵌套深度，超限返回可捕获错误（`Other(105)`
    // ENOBUFS 语义）替代栈溢出。阈值依据见 `MAX_NESTING_DEPTH`。守卫在
    // **栈溢出之前**触发（阈值 64 < 实测崩溃边界 ~80-88），进程不 abort；错误
    // 沿调用链上抛，可被外层 Catch 捕获（拒绝服务面转为可恢复错误）。
    if depth >= MAX_NESTING_DEPTH {
        return Err(NESTING_DEPTH_EXCEEDED);
    }
    let mut cur = Value::Unit;
    let mut action = action;
    loop {
        // 取消传播协议（RFC-08/09/12 残余）：Timeout 取消子树内，每个 action
        // 处理前检查取消标志（结构化并发近似）——已取消则立即返回取消哨兵
        // 错误，由 Timeout 臂消费（回滚 + on_timeout）。已入栈 undo 与线性
        // 标记不在此处回滚（留给 Timeout 臂按 `undo_mark`/线性快照统一处理）。
        if let Some(tok) = cancel.as_deref() {
            if tok.is_cancelled() {
                return Err(CANCELLED_ERR);
            }
        }
        let (next_cur, next_action) = match action {
            Action::Pure(v) => return Ok(v),

            Action::Sequential { current, next } => {
                let v = run_sub_impl(
                    *current,
                    ctx,
                    undo,
                    reg,
                    access.reborrow(),
                    depth,
                    cancel.as_deref_mut(),
                )
                .await?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Syscall {
                op,
                resources,
                next,
            } => {
                // 批内部分失败原子性（审计 B2）：check_linear 逐资源插入 Write/Own
                // 消费标记，若后续资源检查失败（`?` 提前返回），前缀已插入的标记
                // 会残留——只回滚成功前缀（resources[..i]），不得动更早的合法消费记录。
                for (i, u) in resources.iter().enumerate() {
                    if let Err(e) = reg.check_linear(u) {
                        reg.rollback_linear(&resources[..i]);
                        return Err(e);
                    }
                }
                // 审计 R1 契约-F2 修复：virtual-clock 下 GetTime 路由到逻辑
                // 时钟（确定性重放承诺，pdr.md §12.1；executor 注释「确定性
                // 方案由 virtual-clock feature 提供」同源）——不达物理执行器、
                // 不推进时钟（读取非推进）。墙钟路径（无 feature）行为不变。
                // 栈帧纪律：`virtual_get_time` 为 `#[inline(never)]`，临时值
                // 直接作为 match 操作数（不绑定解释器帧局部变量）——解释器
                // 帧越小，RFC-11 深度守卫的嵌套边界越高（r5a 边界测试）。
                match virtual_get_time(ctx, &op) {
                    Some(v) => {
                        let na = next(v);
                        (Value::Unit, na)
                    }
                    // RFC-12（R6-F2）：物理执行失败时回滚本次预插入的线性消费
                    // 标记（Write/Own），恢复同路径可重试语义——否则失败后同路径
                    // 再以 Write 模式打开会被 A4 误拒（InvalidInput，标记残留
                    // 毒化）。成功路径 A4 语义不变（恰好消费一次），错误不压 undo
                    // （现有契约）。
                    None => match exec_via(&mut access, &op, reg).await {
                        Ok((v, capability)) => {
                            // D-104（落点 a）：真实执行的效应在此落点记账——与
                            // `undo.push` 同源，幂等命中不调用 `exec_via` 故不计。
                            undo.add_cost(EffectCost::for_op(&op));
                            match capability {
                                UndoCapability::Identity => {}
                                UndoCapability::Invertible(u) => undo.push(u),
                                UndoCapability::NonInvertible => undo.mark_irreversible(),
                            }
                            let na = next(v);
                            (Value::Unit, na)
                        }
                        Err(e) => {
                            reg.rollback_linear(&resources);
                            return Err(e);
                        }
                    },
                }
            }

            Action::Choose {
                cond,
                then_branch,
                else_branch,
            } => {
                let chosen = if cond(&cur) {
                    *then_branch
                } else {
                    *else_branch
                };
                (cur, chosen)
            }

            Action::Fork {
                left,
                right,
                combine,
            } => {
                // D14 阶段 3：静态冲突检测决定调度；can_parallel=true 且存在共享
                // 执行器通道（Runtime 路径）时真并行，否则保持顺序执行。
                let conflict = fork_conflict(reg, &left, &right);
                let parallel = !conflict && matches!(&access, ExecAccess::Shared { .. });
                if !parallel {
                    // 顺序路径（阶段 1 语义保持 + F1/F2 修复）：分支 registry 隔离
                    // （D13 Clone），共享同一 undo 栈与 ctx（left 先、right 后，观察序
                    // 即压栈序）；完成后**同样 merge 回父**（left 先、right 后，merge
                    // 顺序与观察序一致）—— 分支新分配的 fd 与线性标记（Write 消费 /
                    // Own 终结）并入父，修复分支 fd 泄漏与「冲突型 Fork 后父级同资源
                    // Write 被 A4 错误放行」（F2）。右分支同样取全局唯一 fd 区间
                    // （`fork_fd_region_offset`，两分支同源父 next_fd，若都分配新 fd
                    // 会撞 fd，F1；嵌套任意深度下区间依然互斥，S6/A2）。
                    // 成功/失败均合并（同并行路径「子任务错误仍合并」）。
                    let mut l_reg = reg.clone();
                    let mut r_reg = reg.clone();
                    r_reg.offset_next_fd(fork_fd_region_offset()?);
                    let lv = run_sub_impl(
                        *left,
                        ctx,
                        undo,
                        &mut l_reg,
                        access.reborrow(),
                        depth,
                        cancel.as_deref_mut(),
                    )
                    .await;
                    let rv = run_sub_impl(
                        *right,
                        ctx,
                        undo,
                        &mut r_reg,
                        access.reborrow(),
                        depth,
                        cancel.as_deref_mut(),
                    )
                    .await;
                    reg.merge(l_reg);
                    reg.merge(r_reg);
                    let na = combine(lv?, rv?);
                    (Value::Unit, na)
                } else {
                    // 并行路径：子任务隔离副本 + 共享执行器 + 自持 reactor，
                    // 完成后合并回父。
                    let (shared, reactor) = match &access {
                        ExecAccess::Shared { executor, reactor } => {
                            (executor.clone(), reactor.clone())
                        }
                        ExecAccess::Direct(_) => unreachable!("并行分支仅 Shared 通道可达"),
                    };
                    let (lv, rv) = run_fork_parallel(
                        *left,
                        *right,
                        ctx,
                        reg,
                        undo,
                        shared,
                        reactor,
                        cancel.as_deref_mut(),
                    )
                    .await?;
                    let na = combine(lv, rv);
                    (Value::Unit, na)
                }
            }

            Action::Scope { base, inner, next } => {
                let old = ctx.cwd.clone();
                ctx.cwd = reg.canonicalize_path(&base, &old);
                // finally 模式：inner 无论成功/失败，先恢复 cwd 再传播结果，
                // 保证异常路径同样恢复（RAII 守卫因 ctx 双重可变借用不可行）。
                let v = run_sub_impl(
                    *inner,
                    ctx,
                    undo,
                    reg,
                    access.reborrow(),
                    depth,
                    cancel.as_deref_mut(),
                )
                .await;
                ctx.cwd = old;
                let v = v?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Alloc { len, next } => {
                // 审计 R1 契约-F7 修复：无界分配 → debug 下分配失败 = 进程级
                // abort（handle_alloc_error 不可捕获）/ release 下 OOM abort，
                // 不受信任蓝图可崩溃宿主进程（RFC-11 同族拒绝服务面）。
                // 超上限返回可捕获的 InvalidInput。
                if len > MAX_IO_LEN {
                    return Err(SysError::InvalidInput);
                }
                let na = next(Value::Bytes(vec![0u8; len]));
                (Value::Unit, na)
            }

            Action::Replace { target } => {
                // D10：先 recover（清空撤销栈）+ reg.clear()（释放 handles 与线性
                // 标记，next_fd 保留 D1 单调），再执行 target，以其结果结束（不回原流）。
                // 语义真回归闸门（D-0xx）：
                // 1. recover 任一逆操作失败 → 传播错误，不执行 target（状态未真回归
                //    时继续会在不一致基础上叠加）；
                // 2. 执行段含不可逆副作用（has_irreversible）→ 不可真回归，拒绝，
                //    显式报错（而不是静默假回滚）。
                // 无论 recover 成败/闸门拒绝，irreversible flag 都重置——recover
                // 已尽力（可逆部分恢复），执行段结束，新段从干净状态开始
                // （MEDIUM-2 + LOW-3：失败路径不永久楔死）。
                let recover_result = undo.recover().await;
                let irreversible = undo.has_irreversible();
                undo.reset_irreversible();
                recover_result?;
                if irreversible {
                    return Err(SysError::PermissionDenied);
                }
                reg.clear();
                return run_sub_impl(
                    *target,
                    ctx,
                    undo,
                    reg,
                    access.reborrow(),
                    depth,
                    cancel.as_deref_mut(),
                )
                .await;
            }

            Action::Invoke {
                foreign_id,
                captures,
                yields: _, // Invoke 的 yields 由执行器语义解释，运行时不消费
                deterministic,
                next,
            } => {
                let v = invoke_via(&mut access, foreign_id, &captures, deterministic).await?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Sleep { duration, next } => {
                // 性能/语义修复（分解实测）：`Sleep(0)` 短路立即完成——Windows
                // 定时器 tick（15.6ms）下 `tokio::time::sleep(ZERO)` 实测吃满
                // 一个 tick（bench 分解：顺序 10×Sleep(0) = 158ms），语义应为
                // 立即完成。零时长不触发取消/虚拟时钟推进（无效果）。
                if duration.is_zero() {
                    let na = next(Value::Unit);
                    (Value::Unit, na)
                } else {
                    #[cfg(feature = "virtual-clock")]
                    {
                        if let Some(vc) = ctx.virtual_clock_mut() {
                            vc.advance(duration);
                        } else {
                            tokio::time::sleep(duration).await;
                        }
                    }
                    #[cfg(not(feature = "virtual-clock"))]
                    {
                        if let Some(tok) = cancel.as_deref_mut() {
                            // 取消传播协议：Sleep 可被取消打断（结构化并发近似）。
                            // select! 轮询栈已隔离到 `cancellable_sleep` 独立
                            // coroutine（保护 RFC-11 深度守卫栈预算，见该函数注释）。
                            cancellable_sleep(duration, tok).await;
                        } else {
                            tokio::time::sleep(duration).await;
                        }
                    }
                    let na = next(Value::Unit);
                    (Value::Unit, na)
                }
            }

            Action::WatchSignal { signal, next } => {
                let v = watch_signal_via(&mut access, signal, reg).await?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Timeout {
                action: inner,
                duration,
                on_timeout,
            } => {
                #[cfg(feature = "virtual-clock")]
                {
                    // 审计 R1 红灯根因修复（Timeout×virtual-clock 时域统一）：
                    // 虚拟时钟下 Timeout 以虚拟时间判定（post-check：inner 完成
                    // 时虚拟流逝 ≥ deadline 即视作超时，执行 on_timeout），与墙钟
                    // 路径「future 在 deadline 后完成 → Elapsed」同构。此前用墙钟
                    // 竞速虚拟 Sleep（瞬时完成）→ on_timeout 永不触发（红灯
                    // err_timeout_keeps_undo_stack_and_registry）。注意：VC 下
                    // Sleep 瞬时完成、无「飞行中」状态，故本路径无取消语义
                    // （RFC-12 残余缺口仅墙钟路径适用）。
                    // 栈帧纪律：判定逻辑在独立 async fn（`run_virtual_timeout`）
                    // 内——deadline/elapsed 跨 await 存活，若内联进解释器状态机
                    // 会撑大**每层递归**的状态机帧（r5a 边界测试 63/64/65 实测
                    // 崩溃），提取后解释器帧恢复原尺寸。
                    return run_virtual_timeout(
                        *inner,
                        *on_timeout,
                        duration,
                        ctx,
                        undo,
                        reg,
                        access.reborrow(),
                        depth,
                        cancel.as_deref_mut(),
                    )
                    .await;
                }
                // ── 取消传播协议（RFC-08/09/12 残余统一修复）──
                // 超时触发时不再直接丢弃 inner future（旧行为：已 spawn 的
                // Fork 分支成为孤儿继续执行、持锁分支永不 Unlock、飞行中
                // Write 的线性标记不回滚），而是：
                //   a) 先广播取消（watch 令牌）——并行 Fork 分支在下一 op
                //      边界检查并快速返回，把部分 registry/undo 合并回父
                //      （结构化并发近似：分支内检查取消标志）；
                //   b) 有界宽限（CANCEL_JOIN_GRACE）等待 inner join——分支
                //      阻塞于不可取消 IO（如 TcpRead 无数据）时耗尽宽限后
                //      丢弃 inner（近似同旧语义，但取消标志粘性使该分支在
                //      IO 完成后于下一 op 边界中止）；
                //   c) 回滚 inner 已入栈 undo（`rollback_from`，异步可含 IO）
                //      ——RFC-09：持锁分支的 MutexLock undo 被立即执行，锁与
                //      仲裁占坑释放，同 id 立即可重入（不饥饿至 recover）；
                //      RFC-08：已合并回父的分支 undo 一并撤销；
                //   d) 回滚 inner 期间新增的 A4 线性标记（`rollback_linear_to`
                //      快照差）——RFC-12 残余：飞行中 Write/Own 的预插标记
                //      不残留，同路径可重试；
                //   e) 再执行 on_timeout（原语义：inner 结果被丢弃）。
                // 超时前完成的 inner：效果全部保留（原语义，回滚不触发）。
                // 墙钟 Timeout：取消传播协议在独立 async fn
                // （`run_wall_timeout`）内实现——局部状态不撑大解释器状态机帧
                // （RFC-11 守卫栈预算，见该函数注释）。
                // VC 构建下上方恒 return（`#[allow]`：cfg 剥离后不可达性编译器
                // 无法跨 feature 感知）。
                #[allow(unreachable_code)]
                return run_wall_timeout(
                    *inner,
                    *on_timeout,
                    duration,
                    ctx,
                    undo,
                    reg,
                    access.reborrow(),
                    depth,
                    cancel.as_deref_mut(),
                )
                .await;
            }

            Action::Catch {
                action: inner,
                handler,
            } => match run_sub_impl(
                *inner,
                ctx,
                undo,
                reg,
                access.reborrow(),
                depth,
                cancel.as_deref_mut(),
            )
            .await
            {
                Ok(v) => return Ok(v),
                Err(e) => {
                    return run_sub_impl(
                        handler(e),
                        ctx,
                        undo,
                        reg,
                        access.reborrow(),
                        depth,
                        cancel.as_deref_mut(),
                    )
                    .await
                }
            },
            Action::Idempotent { key, inner, next } => {
                // 幂等键状态机（D-0xx）：COMMITTED 未 REVERTED → 返回缓存结果，
                // 不执行 inner（重试去重）；否则执行 inner，成功后 COMMIT + 缓存，
                // 并压入 REVERT undo（recover 撤销该段副作用时键置 REVERTED，
                // 允许未来重执行——恰好一次语义）。
                let idem = ctx.idempotency.clone();
                let cached = {
                    let guard = idem.lock().expect("幂等注册表锁中毒不可达");
                    guard.lookup_committed(&key)
                };
                if let Some(cached) = cached {
                    // 命中：重试去重，不触发逆函数（从未真正"新执行"）。
                    let na = next(cached);
                    (Value::Unit, na)
                } else {
                    // 执行前记录 undo 栈深：inner 内含 Action::Replace 时，
                    // Replace 分支会提前 recover 清空段内（乃至外层）undo——
                    // 副作用已被内部 Replace 撤销。此时不得 COMMIT（否则
                    // key=COMMITTED 但副作用不存在 → 恰好一次语义破坏，
                    // 审计 blocker）。
                    let mark_before = undo.len();
                    let v = run_sub_impl(
                        *inner,
                        ctx,
                        undo,
                        reg,
                        access.reborrow(),
                        depth,
                        cancel.as_deref_mut(),
                    )
                    .await?;
                    // 仅当 inner 副作用仍在撤销栈（未被内部 Replace 清空）→ COMMIT。
                    if undo.len() > mark_before {
                        // 成功后提交：键置 COMMITTED + 缓存结果。
                        idem.lock()
                            .expect("幂等注册表锁中毒不可达")
                            .commit(&key, v.clone());
                        // 压 REVERT undo：该段的逆操作被 recover 执行时键释放。
                        let idem2 = idem.clone();
                        let k = key.clone();
                        undo.push(Box::pin(async move {
                            idem2.lock().expect("幂等注册表锁中毒不可达").revert(&k);
                            Ok(())
                        }));
                    }
                    // else：inner 内部 Replace 已自清理副作用 → 不 COMMIT，
                    // key 保持未记录 → 重试重新执行（inner 的 Replace 保证
                    // 段内自清理，无重复副作用）。
                    let na = next(v);
                    (Value::Unit, na)
                }
            }
        };
        cur = next_cur;
        action = next_action;
    }
}

/// 解释器公共入口（冻结的 5 参签名，A2 交付）：Action AST → 运行时语义。
///
/// 本入口走 `Direct` 通道：无共享执行器（无 Fork 并行能力），Fork 恒按阶段 1
/// （D14）顺序执行。`Runtime::run`/`run_blocking` 走 `Shared` 通道，具备 D14
/// 阶段 3 的真并行 Fork。语义要点见 `interpret_impl`。
pub async fn interpret(
    action: Action,
    ctx: &mut Context,
    undo: &mut UndoStack,
    reg: &mut ResourceRegistry,
    ex: &mut dyn SyscallExecutor,
) -> Result<Value, SysError> {
    // 公开入口：深度从 0 起调（冻结签名不可改，depth 仅为 interpret_impl 私有参数）；
    // 取消域从 None 起调（顶层无 Timeout 取消上下文）。
    interpret_impl(action, ctx, undo, reg, ExecAccess::Direct(ex), 0, None).await
}

// 供外部使用的类型别名
pub type _BoxFutureAlias<'a, T> = BoxFuture<'a, T>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource::{AccessMode, Resource};

    fn usage(r: Resource, m: AccessMode) -> crate::resource::ResourceUsage {
        crate::resource::ResourceUsage {
            resource: r,
            mode: m,
        }
    }

    /// R3-B 单元级防护（终审 Note-3）：直接构造**已广播**的外层取消接收端，
    /// 验证 `wait_timeout` 的 OR 臂打断嵌套 wait——行为回归（OR 臂被删/失效）
    /// 时本测试红（集成层 `timeout_nested_outer_cancel_interrupts_inner_wait`
    /// 为黑盒断言，对「外层取消到达内层」的路径无区分度）。
    #[tokio::test]
    async fn wait_timeout_outer_cancel_interrupts() {
        // 内层：长 sleep（10s）——若 OR 臂失效，本测试会等到宽限/超时。
        let inner: LocalBoxFuture<'_, Result<Value, SysError>> = Box::pin(async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(Value::Unit)
        });
        // 本层通道（未被广播）与**外层已广播**通道。
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let (_outer_tx, outer_rx) = tokio::sync::watch::channel(false);
        let _ = _outer_tx.send(true); // 外层已取消（粘性：changed() 立即 Ready）

        let t0 = std::time::Instant::now();
        let (timed_out, _r) =
            wait_timeout(inner, Duration::from_secs(10), &tx, Some(&outer_rx)).await;
        assert!(timed_out, "外层已广播 → OR 臂应触发（timed_out=true）");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "OR 臂应立即打断（不等到内层 10s/宽限），实测 {:?}",
            t0.elapsed()
        );
        // 本层取消已广播：子树将响应。
        assert!(*tx.borrow(), "OR 臂触发后本层通道应已广播取消");
    }

    /// 收敛轮：fork 区间序号耗尽从 assert panic 改为可捕获错误（Other(28)，
    /// ENOSPC 语义近似）——不受信任蓝图批量构造 Fork 不再能崩溃宿主进程。
    /// 纯函数直接构造 k 验证边界（不消耗全局 static 预算，避免与并行
    /// 运行的 Fork 测试竞争）。
    #[test]
    fn fork_region_seq_exhaustion_returns_error_not_panic() {
        // 正常路径：k < 2^16 → 对齐区间偏移。
        for k in [1u64, 5, (1 << 16) - 1] {
            let off = fork_region_offset_for(k).expect("未耗尽前应 Ok");
            assert_eq!(off, k << 48, "区间偏移对齐 k<<48");
        }
        // 耗尽边界：k ≥ 2^16 → 可捕获错误（而非 assert panic）。
        for k in [1u64 << 16, (1u64 << 16) + 1, u64::MAX] {
            assert_eq!(
                fork_region_offset_for(k),
                Err(SysError::Other(28)),
                "k={k} 应返回可捕获错误（ENOSPC 语义）"
            );
        }
    }

    /// 审计 R7-D 修复：Invoke.captures 声明资源纳入 fork_conflict 静态收集——
    /// 修复前落 `_ => {}` 被静默丢弃，captures 与兄弟分支资源冲突时 can_parallel
    /// 误判 true（真并行执行器侧物理冲突）。
    #[test]
    fn fork_conflict_detects_invoke_captures() {
        let reg = ResourceRegistry::new();
        let inv = Action::Invoke {
            foreign_id: 1,
            captures: ResourceSet::from_iter([usage(Resource::Fd(7), AccessMode::Read)]),
            yields: Default::default(),
            deterministic: true,
            next: Box::new(|_| Action::Pure(Value::Unit)),
        };
        let write_fd7 = Action::Syscall {
            op: DataOp::Write {
                fd: 7,
                data: vec![],
            },
            resources: vec![usage(Resource::Fd(7), AccessMode::Write)],
            next: Box::new(|_| Action::Pure(Value::Unit)),
        };
        // 左分支 Invoke 声明 Fd(7) captures，右分支 Write Fd(7) → 冲突（修复前误判可并行）。
        assert!(
            fork_conflict(&reg, &inv, &write_fd7),
            "Invoke.captures 与兄弟分支冲突应静态检测（修复前误判并行）"
        );
        // 对照组：不同 fd 不冲突。
        let write_fd8 = Action::Syscall {
            op: DataOp::Write {
                fd: 8,
                data: vec![],
            },
            resources: vec![usage(Resource::Fd(8), AccessMode::Write)],
            next: Box::new(|_| Action::Pure(Value::Unit)),
        };
        assert!(!fork_conflict(&reg, &inv, &write_fd8));
    }
}
