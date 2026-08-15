//! 运行时内核 —— 契约冻结（pdr.md §5.1 / §12.3）。
//!
//! A2 拥有本文件的实现：`interpret` 解释器（trampoline）、UndoStack 撤销、
//! trackΓ/recoverΓ、Sleep/Timeout/Fork/Scope/Catch 等节点的运行时语义。
//! 基础骨架由 CTO 冻结（contracts.md §类型冻结），方法体为 A2 交付。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::action::{Action, Value};
use crate::error::SysError;
use crate::resource::ResourceRegistry;
use crate::syscall::{BoxFuture, SyscallExecutor, UndoOp};

/// 效果上下文 Γ（pdr.md §5.1.1）：当前状态（cwd + 环境变量）。
#[derive(Debug, Clone)]
pub struct Context {
    pub cwd: PathBuf,
    pub env: HashMap<String, String>,
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
        Self { cwd, env }
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
use crate::coeffects::{CoeffectStore, Component};
#[cfg(feature = "virtual-clock")]
use crate::virtual_clock::VirtualClock;

/// Algeff 运行时（pdr.md §12.3）。
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
    /// 逻辑时钟（可选 feature `virtual-clock`）。
    #[cfg(feature = "virtual-clock")]
    virtual_clock: Option<VirtualClock>,
    /// 自持 tokio reactor。注意：`Runtime::new` 需在 tokio 上下文之外调用。
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
            #[cfg(feature = "virtual-clock")]
            virtual_clock: Some(VirtualClock::new()),
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

    #[cfg(feature = "virtual-clock")]
    pub fn virtual_clock(&mut self) -> Option<&mut VirtualClock> {
        self.virtual_clock.as_mut()
    }

    /// 执行蓝图（阻塞直至完成或出错）。
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

    /// 恢复效果上下文：执行全部累积逆操作（pdr.md §5.1.3 recoverΓ）。
    pub async fn recover(&mut self) {
        self.undo_stack.recover().await;
    }
}

/// 解释器：Action AST → 运行时语义（A2 交付）。
///
/// 语义要点（pdr.md §2.1 / §4）：
/// - `Pure`：A2 单位元，直接产生值；
/// - `Syscall`：线性检查 → 执行器执行 → 逆操作压栈；
/// - `Choose`：A5 分支隔离，取一分支执行；
/// - `Fork`：A3 冲突检测（`ResourceRegistry::can_parallel`），资源不相交则 tokio::spawn 并行；
/// - `Scope`：路径上下文压栈/弹栈；
/// - `Timeout`/`Catch`：超时与错误捕获；
/// - `Replace`：先 recover 再执行 target（安全默认，可讨论）。
pub async fn interpret(
    _action: Action,
    _ctx: &mut Context,
    _undo: &mut UndoStack,
    _reg: &mut ResourceRegistry,
    _ex: &mut dyn SyscallExecutor,
) -> Result<Value, SysError> {
    todo!("A2: 实现 Action 解释器（contracts.md §任务 A2）")
}

// 供外部使用的类型别名
pub type _BoxFutureAlias<'a, T> = BoxFuture<'a, T>;
