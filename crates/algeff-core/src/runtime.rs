//! 运行时内核 —— 契约冻结（pdr.md §5.1 / §12.3）。
//!
//! A2 拥有本文件的实现：`interpret` 解释器（trampoline）、UndoStack 撤销、
//! trackΓ/recoverΓ、Sleep/Timeout/Fork/Scope/Catch 等节点的运行时语义。
//! 基础骨架由 CTO 冻结（contracts.md §类型冻结），方法体为 A2 交付。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use crate::action::{Action, Value};
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
    executor: Box<dyn SyscallExecutor>,
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
            executor,
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
    pub async fn set_dependency(
        &mut self,
        k: DepKey,
        v: Value,
    ) -> Result<UndoOp, SysError> {
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
    /// - `Fork`：阶段 1（D14）= 静态冲突检测（`fork_conflict`）+ **顺序执行**
    ///   （left → right → combine）；分支状态以 registry Clone 隔离（D13）；
    /// - `Replace`：**先 `recover()`**——LIFO 执行全部累积逆操作并清空撤销栈——
    ///   再执行 target，以其结果结束（D10，安全默认：资源不泄漏）；
    /// - `Scope`：cwd 词法规范化入栈，退出时（含 inner 出错）无条件恢复；
    /// - `Timeout`：`tokio::time::timeout`，`Elapsed` 走 `on_timeout` 分支；
    /// - `Catch`：仅处理错误值，不触碰撤销栈（recover 语义在 Replace/recover 路径）；
    /// - `WatchSignal`/`Invoke`：委托执行器，默认 ENOSYS（`Other(38)`）原样透传。
    ///
    /// 注意：`interpret` 的 future 非 `Send`（冻结签名 `&mut dyn SyscallExecutor`
    /// 无 Send 超 trait），直接 `.await` 时需在非 Send 要求上下文中进行
    /// （如 `run_blocking`）。
    pub async fn run(&mut self, action: Action) -> Result<Value, SysError> {
        interpret(
            action,
            &mut self.context,
            &mut self.undo_stack,
            &mut self.resource_registry,
            self.executor.as_mut(),
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
        let executor = self.executor.as_mut();
        reactor.block_on(interpret(
            action,
            context,
            undo_stack,
            resource_registry,
            executor,
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
fn run_sub<'a>(
    action: Action,
    ctx: &'a mut Context,
    undo: &'a mut UndoStack,
    reg: &'a mut ResourceRegistry,
    ex: &'a mut dyn SyscallExecutor,
) -> LocalBoxFuture<'a, Result<Value, SysError>> {
    Box::pin(async move { interpret(action, ctx, undo, reg, ex).await })
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
            action,
            on_timeout,
            ..
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
/// 阶段 1 仅作检测（执行恒为顺序化）；阶段 3 将据此决定是否
/// `tokio::spawn` 并行（并行化方案见 RFC）。
pub fn fork_conflict(reg: &ResourceRegistry, left: &Action, right: &Action) -> bool {
    let mut l_res = ResourceSet::new();
    let mut r_res = ResourceSet::new();
    collect_syscall_resources(left, &mut l_res);
    collect_syscall_resources(right, &mut r_res);
    !reg.can_parallel(&l_res, &r_res)
}

/// 解释器：Action AST → 运行时语义（A2 交付）。
///
/// 语义要点（pdr.md §2.1 / §4 / §5.1，contracts.md D2/D10/D11/D14）：
/// - 主循环为 trampoline；`cur`（初始 `Unit`）作为贯穿节点的「当前值」，
///   每个节点产出 `(下一 cur, 下一 Action)`；
/// - `Pure`：单位元（公理 A2），直接产生值；
/// - `Syscall`：逐资源 `check_linear`（公理 A4，失败立即返回）→ 执行器执行
///   → `Option<UndoOp>` 压入撤销栈 → `next(v)`；
/// - `Choose`：`cond(&cur)` 选分支，分支结果继续主循环（A5 分支隔离）；
/// - `Fork`：阶段 1（D14）= 静态冲突检测（`fork_conflict`）+ 顺序执行；
///   分支状态以 registry Clone 隔离（D13），合并回主 registry 的 API 缺失 → RFC；
/// - `Scope`：cwd 压栈/弹栈（inner 出错时同样恢复）；
/// - `Replace`：先 `recover()`（清空撤销栈）再执行 target，以其结果结束（D10）；
/// - `Sleep`：feature `virtual-clock` 时推进逻辑时钟（不真实等待），否则真实等待；
/// - `Timeout`：`tokio::time::timeout`，`Elapsed` → 执行 on_timeout；
/// - `Catch`：Err → handler(e)，Ok 原样返回；不触碰撤销栈（recover 语义在
///   Replace/recover 路径）；
/// - `WatchSignal`/`Invoke`：委托执行器；默认执行器返回 ENOSYS
///   （`SysError::Other(38)`），解释器原样透传错误；
pub async fn interpret(
    action: Action,
    ctx: &mut Context,
    undo: &mut UndoStack,
    reg: &mut ResourceRegistry,
    ex: &mut dyn SyscallExecutor,
) -> Result<Value, SysError> {
    let mut cur = Value::Unit;
    let mut action = action;
    loop {
        let (next_cur, next_action) = match action {
            Action::Pure(v) => return Ok(v),

            Action::Sequential { current, next } => {
                let v = run_sub(*current, ctx, undo, reg, ex).await?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Syscall { op, resources, next } => {
                for u in &resources {
                    reg.check_linear(u)?;
                }
                let (v, maybe_undo) = ex.execute(&op, reg).await?;
                if let Some(u) = maybe_undo {
                    undo.push(u);
                }
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Choose { cond, then_branch, else_branch } => {
                let chosen = if cond(&cur) { *then_branch } else { *else_branch };
                (cur, chosen)
            }

            Action::Fork { left, right, combine } => {
                // 阶段 1（D14）：静态冲突检测 + 顺序执行。检测结果暂不改变调度。
                let _conflict = fork_conflict(reg, &left, &right);
                // 分支 registry 隔离（D13：Clone），避免两分支在共享 consumed 集上
                // 互相触发 A4 线性拒绝；合并回主 registry 的 API 缺失 → RFC。
                let mut l_reg = reg.clone();
                let mut r_reg = reg.clone();
                let lv = run_sub(*left, ctx, undo, &mut l_reg, ex).await?;
                let rv = run_sub(*right, ctx, undo, &mut r_reg, ex).await?;
                let na = combine(lv, rv);
                (Value::Unit, na)
            }

            Action::Scope { base, inner, next } => {
                let old = ctx.cwd.clone();
                ctx.cwd = reg.canonicalize_path(&base, &old);
                // finally 模式：inner 无论成功/失败，先恢复 cwd 再传播结果，
                // 保证异常路径同样恢复（RAII 守卫因 ctx 双重可变借用不可行）。
                let v = run_sub(*inner, ctx, undo, reg, ex).await;
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
                // D10：先 recover（清空撤销栈），再执行 target，以其结果结束（不回原流）。
                undo.recover().await;
                return run_sub(*target, ctx, undo, reg, ex).await;
            }

            Action::Invoke {
                foreign_id,
                captures,
                yields: _,
                deterministic,
                next,
            } => {
                let v = ex.invoke(foreign_id, &captures, deterministic).await?;
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
                let v = ex.watch_signal(signal, reg).await?;
                let na = next(v);
                (Value::Unit, na)
            }

            Action::Timeout {
                action: inner,
                duration,
                on_timeout,
            } => match tokio::time::timeout(duration, run_sub(*inner, ctx, undo, reg, ex)).await
            {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) => return Err(e),
                Err(_elapsed) => return run_sub(*on_timeout, ctx, undo, reg, ex).await,
            },

            Action::Catch { action: inner, handler } => {
                match run_sub(*inner, ctx, undo, reg, ex).await {
                    Ok(v) => return Ok(v),
                    Err(e) => return run_sub(handler(e), ctx, undo, reg, ex).await,
                }
            }
        };
        cur = next_cur;
        action = next_action;
    }
}

// 供外部使用的类型别名
pub type _BoxFutureAlias<'a, T> = BoxFuture<'a, T>;
