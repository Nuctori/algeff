//! 运行时内核 —— 契约冻结（pdr.md §5.1 / §12.3）。
//!
//! A2 拥有本文件的实现：`interpret` 解释器（trampoline）、UndoStack 撤销、
//! trackΓ/recoverΓ、Sleep/Timeout/Fork/Scope/Catch 等节点的运行时语义。
//! 基础骨架由 CTO 冻结（contracts.md §类型冻结），方法体为 A2 交付。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use crate::action::{Action, DataOp, Id, Signal, Value};
use crate::error::SysError;
use crate::resource::{ResourceRegistry, ResourceSet};
use crate::syscall::{BoxFuture, SyscallExecutor, UndoOp};

#[cfg(feature = "virtual-clock")]
use crate::virtual_clock::VirtualClock;

/// 效果上下文 Γ（pdr.md §5.1.1）：当前状态（cwd + 环境变量）。
///
/// 逻辑时钟放在 Context 内（而非 Runtime）：`interpret` 的签名冻结为
/// `(&mut Context, ...)`，只有从这里才能让 Sleep 节点访问时钟。
#[derive(Debug, Clone)]
pub struct Context {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
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
}

impl UndoStack {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
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

    /// 把另一个栈的全部逆操作按序追加到本栈（Fork 并行合并用，D14）。
    /// 调用方按 **left → right** 顺序 append（栈底到栈顶），使 LIFO recover
    /// 先执行 **right** 的逆操作、再执行 left 的 —— 与顺序路径「left 先执行、
    /// right 后执行」的观察序一致（right 的效果后发生、先撤销）。
    pub fn append(&mut self, other: UndoStack) {
        self.ops.extend(other.ops);
    }

    /// recoverΓ：按 LIFO 顺序执行全部逆操作（pdr.md §5.1.3）。
    pub async fn recover(&mut self) {
        while let Some(op) = self.ops.pop() {
            op.await;
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
    /// 共享执行器（CTO 批准方向，D14 阶段 3）：公开 API `Runtime::new(Box<dyn>)`
    /// 不变，内部以 `Arc<Mutex>` 包装 —— Fork 并行分支各持 Arc 克隆，经锁
    /// 互斥串行化执行器调用（锁仅保护执行器内部状态，物理 IO 本身异步）。
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
    pub fn new(executor: Box<dyn SyscallExecutor>) -> Self {
        Self {
            context: Context::new(),
            undo_stack: UndoStack::new(),
            resource_registry: ResourceRegistry::new(),
            executor: Arc::new(tokio::sync::Mutex::new(SendExecutor(executor))),
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
    ///   （registry `merge`：handles/consumed/owned_consumed 并集 + next_fd=max；
    ///   undo：right 先、left 后，保持 LIFO 与观察序）；can_parallel=false
    ///   保持顺序执行（left→right→combine）；
    /// - `Replace`：**先 `recover()` + `reg.clear()`**（LIFO 执行全部累积逆操作
    ///   并清空撤销栈与 registry 句柄/线性标记，next_fd 保留 D1）再执行 target，
    ///   以其结果结束（D10，安全默认：资源不泄漏）；
    /// - `Scope`：cwd 词法规范化入栈，退出时（含 inner 出错）无条件恢复；
    /// - `Timeout`：`tokio::time::timeout`，`Elapsed` 走 `on_timeout` 分支；
    /// - `Catch`：仅处理错误值，不触碰撤销栈（recover 语义在 Replace/recover 路径）；
    /// - `WatchSignal`/`Invoke`：委托执行器，默认 ENOSYS（`Other(38)`）原样透传。
    ///
    /// 注意：`interpret` 的 future 非 `Send`（冻结签名 `&mut dyn SyscallExecutor`
    /// 无 Send 超 trait），直接 `.await` 时需在非 Send 要求上下文中进行
    /// （如 `run_blocking`）；Fork 并行子任务在 `spawn_blocking` 线程内以
    /// current-thread runtime 驱动（`drive`，同 `tests/concurrency_stress.rs`）。
    pub async fn run(&mut self, action: Action) -> Result<Value, SysError> {
        interpret_impl(
            action,
            &mut self.context,
            &mut self.undo_stack,
            &mut self.resource_registry,
            ExecAccess::Shared(self.executor.clone()),
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
            ExecAccess::Shared(executor),
        ))
    }

    /// 恢复效果上下文：执行全部累积逆操作（pdr.md §5.1.3 recoverΓ）。
    pub async fn recover(&mut self) {
        self.undo_stack.recover().await;
    }
}

/// 子 Action 递归用的非 Send future。
///
/// 不能复用 syscall.rs 的 `BoxFuture`（强制 `+ Send`）：冻结签名
/// `&mut dyn SyscallExecutor` 的 trait 对象无 `Send` 超 trait，
/// 因此 `interpret` 及其递归 future 均非 `Send`。
type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// 递归执行子 Action（async fn 不可直接自递归，统一 `Box::pin`）。
/// 非 Send 约束同 `LocalBoxFuture`（见上）。
fn run_sub_impl<'a>(
    action: Action,
    ctx: &'a mut Context,
    undo: &'a mut UndoStack,
    reg: &'a mut ResourceRegistry,
    access: ExecAccess<'a>,
) -> LocalBoxFuture<'a, Result<Value, SysError>> {
    Box::pin(async move { interpret_impl(action, ctx, undo, reg, access).await })
}

/// 共享执行器通道（CTO 批准方向，D14 阶段 3）：`Runtime` 内部以
/// `Arc<tokio::sync::Mutex<…>>` 包装执行器（公开 API `Runtime::new(Box<dyn>)`
/// 不变）；Fork 并行子任务各持 Arc 克隆，经锁互斥串行化执行器调用（锁仅
/// 保护执行器内部状态，物理 IO 本身异步）。
type SharedExecutor = Arc<tokio::sync::Mutex<SendExecutor>>;

/// 执行器 Send 包装（「公开 API 不变、内部包装」的落地）：冻结契约 D3 的
/// `SyscallExecutor` 无 `Send` 超 trait，而共享互斥设计要求跨线程传递
/// `Arc<Mutex<…>>`（`spawn_blocking` 闭包须 `Send`，`Mutex<T>: Sync` 需
/// `T: Send`）。
///
/// 安全性论证：执行器**只在** `Mutex` 独占锁内以 `&mut` 访问（`exec_via` 等
/// 每调用加锁，`Runtime::run`/`run_blocking` 不持有跨调用锁），跨线程表现为
/// 单线程串行语义 —— 与 `Mutex<T: Send>` 的保守界等价。
struct SendExecutor(Box<dyn SyscallExecutor>);

unsafe impl Send for SendExecutor {}

impl std::ops::Deref for SendExecutor {
    type Target = Box<dyn SyscallExecutor>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SendExecutor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// 执行器访问通道（运行时内部）：
/// - `Direct`：冻结公共签名 `interpret(action, ..., ex: &mut dyn SyscallExecutor)`
///   路径 —— 无 Fork 并行能力（并行需要共享互斥通道）；
/// - `Shared`：`Runtime::run`/`run_blocking` 路径 —— 每个 Syscall 调用经锁互斥，
///   Fork 并行分支可用（见 `run_fork_parallel`）。
enum ExecAccess<'a> {
    Direct(&'a mut dyn SyscallExecutor),
    Shared(SharedExecutor),
}

impl<'a> ExecAccess<'a> {
    /// 重借用：`Direct` 重借用内部 `&mut`，`Shared` 克隆 Arc（递归/分支复用）。
    fn reborrow<'b>(&'b mut self) -> ExecAccess<'b> {
        match self {
            ExecAccess::Direct(ex) => ExecAccess::Direct(&mut **ex),
            ExecAccess::Shared(arc) => ExecAccess::Shared(arc.clone()),
        }
    }
}

/// 本地 current-thread runtime 驱动（interpret future 非 Send，只能在阻塞线程内
/// `block_on`；`spawn_blocking` 线程位于 tokio 上下文之外，满足 D9）。参考
/// `tests/concurrency_stress.rs` 已验证的 drive 模式（外层 tokio::spawn N +
/// 内层 spawn_blocking 驱动 current-thread runtime）。
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

/// 经执行器访问通道执行 DataOp。
/// `Shared` 路径：每调用加锁互斥（Fork 并行时两子任务互斥串行化执行器调用）。
async fn exec_via(
    access: &mut ExecAccess<'_>,
    op: &DataOp,
    reg: &mut ResourceRegistry,
) -> Result<(Value, Option<UndoOp>), SysError> {
    match access {
        ExecAccess::Direct(ex) => (**ex).execute(op, reg).await,
        ExecAccess::Shared(arc) => {
            let mut guard = arc.lock().await;
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
        ExecAccess::Shared(arc) => {
            let mut guard = arc.lock().await;
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
        ExecAccess::Shared(arc) => {
            let mut guard = arc.lock().await;
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
        _ => {}
    }
}

/// Fork 静态冲突检测（D14）：收集左右子树 Syscall 资源，查询冲突矩阵。
///
/// `can_parallel=false`（冲突）→ 顺序执行；`can_parallel=true` →
/// `Runtime` 路径（Shared 通道）下真并行（`run_fork_parallel`）。
pub fn fork_conflict(reg: &ResourceRegistry, left: &Action, right: &Action) -> bool {
    let mut l_res = ResourceSet::new();
    let mut r_res = ResourceSet::new();
    collect_syscall_resources(left, &mut l_res);
    collect_syscall_resources(right, &mut r_res);
    !reg.can_parallel(&l_res, &r_res)
}

/// Fork 并行分支执行（D14 阶段 3，CTO 批准方向）：
///
/// 两分支各自持有 registry/context 隔离副本（D13：Clone）+ 独立 UndoStack，
/// 经共享执行器通道（Arc<Mutex>，每 Syscall 调用互斥 —— 锁仅保护执行器内部
/// 状态，物理 IO 本身异步）在**独立阻塞线程**上以 current-thread runtime 并发
/// 驱动（interpret future 非 Send，参考 concurrency_stress.rs 的 drive 模式；
/// pdr.md §19.2「Fork → tokio::spawn 合法」/ 支柱三结构相似性）。
///
/// 完成后合并回父：
/// - registry：子 handles 以原 fd 并入 + consumed/owned_consumed 并集 +
///   `next_fd = max`（`ResourceRegistry::merge`，RFC-A3-2 / D13「合并回父」）；
/// - undo：并入顺序为 left 后 right（栈序与顺序路径一致）—— LIFO recover
///   先执行 right 的逆操作再执行 left 的（right 的效果后发生、先撤销，
///   与「left 先执行」的观察序一致）。
///
/// 子任务错误：两分支均跑完后仍合并 registry/undo（部分效果可被外层
/// Catch/recover 撤销），再返回错误（left 优先）。
async fn run_fork_parallel(
    left: Action,
    right: Action,
    ctx: &Context,
    reg: &mut ResourceRegistry,
    undo: &mut UndoStack,
    shared: SharedExecutor,
) -> Result<(Value, Value), SysError> {
    // 子任务隔离副本（D13）与独立撤销栈。
    let mut l_ctx = ctx.clone();
    let mut r_ctx = ctx.clone();
    let mut l_reg = reg.clone();
    let mut r_reg = reg.clone();
    let mut l_undo = UndoStack::new();
    let mut r_undo = UndoStack::new();
    let l_shared = shared.clone();
    let r_shared = shared.clone();

    // 两个独立阻塞线程并发驱动（真并行；执行器调用经锁互斥串行化）。
    // 子任务把（结果, 隔离 registry, 独立撤销栈）带回，供完成后合并。
    let l_task = tokio::task::spawn_blocking(move || {
        let v = drive(interpret_impl(
            left,
            &mut l_ctx,
            &mut l_undo,
            &mut l_reg,
            ExecAccess::Shared(l_shared),
        ));
        (v, l_reg, l_undo)
    });
    let r_task = tokio::task::spawn_blocking(move || {
        let v = drive(interpret_impl(
            right,
            &mut r_ctx,
            &mut r_undo,
            &mut r_reg,
            ExecAccess::Shared(r_shared),
        ));
        (v, r_reg, r_undo)
    });

    let (l_res, l_reg, l_undo) = l_task.await.expect("Fork 并行左分支任务 panic");
    let (r_res, r_reg, r_undo) = r_task.await.expect("Fork 并行右分支任务 panic");

    // 合并回父（D13「完成后合并回父」/ RFC-A3-2）：
    // fd 不冲突（D1 单调：子从父 clone 的 next_fd 起算，合并时父 next_fd = max）。
    reg.merge(l_reg);
    reg.merge(r_reg);
    // undo：先并入 left 再并入 right —— 栈序 [left, right] 与顺序路径一致
    // （left 先执行先压栈、right 后执行后压栈），LIFO recover 先弹 right 的
    // undo 再弹 left 的（观察序：right 的效果后发生、先撤销）。
    undo.append(l_undo);
    undo.append(r_undo);

    Ok((l_res?, r_res?))
}

/// 解释器内核：Action AST → 运行时语义（A2 交付，`interpret_impl`）。
///
/// 语义要点（pdr.md §2.1 / §4 / §5.1，contracts.md D2/D10/D11/D14）：
/// - 主循环为 trampoline；`cur`（初始 `Unit`）作为贯穿节点的「当前值」，
///   每个节点产出 `(下一 cur, 下一 Action)`；
/// - `Pure`：单位元（公理 A2），直接产生值；
/// - `Syscall`：逐资源 `check_linear`（公理 A4，失败立即返回）→ 执行器执行
///   （经 `access` 通道）→ `Option<UndoOp>` 压入撤销栈 → `next(v)`；
/// - `Choose`：`cond(&cur)` 选分支，分支结果继续主循环（A5 分支隔离）；
/// - `Fork`：D14 阶段 3 —— 静态冲突检测（`fork_conflict`）；`can_parallel=true`
///   且为 `Shared` 通道时真并行（`run_fork_parallel`：registry/ctx 隔离 + 独立
///   UndoStack + 共享执行器，完成后合并回父），否则顺序执行（left→right→combine，
///   阶段 1 语义保持）；
/// - `Scope`：cwd 压栈/弹栈（inner 出错时同样恢复）；
/// - `Replace`：先 `recover()`（清空撤销栈）+ `reg.clear()`（释放 handles 与
///   线性标记，next_fd 保留 D1），再执行 target，以其结果结束（D10）；
/// - `Sleep`：feature `virtual-clock` 时推进逻辑时钟（不真实等待），否则真实等待；
/// - `Timeout`：`tokio::time::timeout`，`Elapsed` → 执行 on_timeout；
/// - `Catch`：Err → handler(e)，Ok 原样返回；不触碰撤销栈（recover 语义在
///   Replace/recover 路径）；
/// - `WatchSignal`/`Invoke`：委托执行器；默认执行器返回 ENOSYS
///   （`SysError::Other(38)`），解释器原样透传错误。
async fn interpret_impl(
    action: Action,
    ctx: &mut Context,
    undo: &mut UndoStack,
    reg: &mut ResourceRegistry,
    mut access: ExecAccess<'_>,
) -> Result<Value, SysError> {
    let mut cur = Value::Unit;
    let mut action = action;
    loop {
        let (next_cur, next_action) = match action {
            Action::Pure(v) => return Ok(v),

            Action::Sequential { current, next } => {
                let v = run_sub_impl(*current, ctx, undo, reg, access.reborrow()).await?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Syscall {
                op,
                resources,
                next,
            } => {
                for u in &resources {
                    reg.check_linear(u)?;
                }
                let (v, maybe_undo) = exec_via(&mut access, &op, reg).await?;
                if let Some(u) = maybe_undo {
                    undo.push(u);
                }
                let na = next(v);
                (Value::Unit, na)
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
                let parallel = !conflict && matches!(&access, ExecAccess::Shared(_));
                if !parallel {
                    // 顺序路径（阶段 1 语义保持）：分支 registry 隔离（D13 Clone），
                    // 共享同一 undo 栈与 ctx（left 先、right 后，观察序即压栈序）。
                    let mut l_reg = reg.clone();
                    let mut r_reg = reg.clone();
                    let lv = run_sub_impl(*left, ctx, undo, &mut l_reg, access.reborrow()).await?;
                    let rv = run_sub_impl(*right, ctx, undo, &mut r_reg, access.reborrow()).await?;
                    let na = combine(lv, rv);
                    (Value::Unit, na)
                } else {
                    // 并行路径：子任务隔离副本 + 共享执行器，完成后合并回父。
                    let shared = match &access {
                        ExecAccess::Shared(arc) => arc.clone(),
                        ExecAccess::Direct(_) => unreachable!("并行分支仅 Shared 通道可达"),
                    };
                    let (lv, rv) = run_fork_parallel(*left, *right, ctx, reg, undo, shared).await?;
                    let na = combine(lv, rv);
                    (Value::Unit, na)
                }
            }

            Action::Scope { base, inner, next } => {
                let old = ctx.cwd.clone();
                ctx.cwd = reg.canonicalize_path(&base, &old);
                // finally 模式：inner 无论成功/失败，先恢复 cwd 再传播结果，
                // 保证异常路径同样恢复（RAII 守卫因 ctx 双重可变借用不可行）。
                let v = run_sub_impl(*inner, ctx, undo, reg, access.reborrow()).await;
                ctx.cwd = old;
                let v = v?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Alloc { len, next } => {
                let na = next(Value::Bytes(vec![0u8; len]));
                (Value::Unit, na)
            }

            Action::Replace { target } => {
                // D10：先 recover（清空撤销栈）+ reg.clear()（释放 handles 与线性
                // 标记，next_fd 保留 D1 单调），再执行 target，以其结果结束（不回原流）。
                undo.recover().await;
                reg.clear();
                return run_sub_impl(*target, ctx, undo, reg, access.reborrow()).await;
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
                    tokio::time::sleep(duration).await;
                }
                let na = next(Value::Unit);
                (Value::Unit, na)
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
                match tokio::time::timeout(
                    duration,
                    run_sub_impl(*inner, ctx, undo, reg, access.reborrow()),
                )
                .await
                {
                    Ok(Ok(v)) => return Ok(v),
                    Ok(Err(e)) => return Err(e),
                    Err(_elapsed) => {
                        return run_sub_impl(*on_timeout, ctx, undo, reg, access.reborrow()).await
                    }
                }
            }

            Action::Catch {
                action: inner,
                handler,
            } => match run_sub_impl(*inner, ctx, undo, reg, access.reborrow()).await {
                Ok(v) => return Ok(v),
                Err(e) => return run_sub_impl(handler(e), ctx, undo, reg, access.reborrow()).await,
            },
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
    interpret_impl(action, ctx, undo, reg, ExecAccess::Direct(ex)).await
}

// 供外部使用的类型别名
pub type _BoxFutureAlias<'a, T> = BoxFuture<'a, T>;
