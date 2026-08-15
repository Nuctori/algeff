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
use crate::error::SysError;
use crate::resource::{ResourceRegistry, ResourceSet};
use crate::syscall::{BoxFuture, SyscallExecutor, UndoOp};

#[cfg(feature = "virtual-clock")]
use crate::virtual_clock::VirtualClock;
/// 单次分配/IO 长度上界（审计 R1 契约-F7 修复）：`vec![0u8; len]` 在 debug
/// 下分配失败 = 进程级 abort（handle_alloc_error 不可捕获），release 下 OOM
/// abort —— 不受信任蓝图可崩溃宿主进程（与 RFC-11 修复前的栈溢出同族拒绝
/// 服务面）。取 64MB：远超真实单次 IO/分配需求，远低于危险分配量级。
/// 超限返回 `SysError::InvalidInput`（可被外层 Catch 捕获）。
pub const MAX_IO_LEN: usize = 64 * 1024 * 1024;

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

    /// 回滚 `mark` 之后压入的逆操作（取消传播协议，Timeout 取消用，RFC-08/09）：
    /// 按 LIFO 顺序执行 `ops[mark..]`（与 `recover` 同序）并把它们弹出；
    /// `ops[..mark]` 保留——外层效果（Timeout 之前已入栈的 undo）不属于本次
    /// 回滚范围。`mark >= len` 时为空操作（防御：无内层效果需要回滚）。
    /// undo 可为异步 IO（决策 D4），故本方法为 async。
    pub async fn rollback_from(&mut self, mark: usize) {
        while self.ops.len() > mark {
            let op = self.ops.pop().expect("len > mark 保证栈非空");
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
    /// 注意：`interpret` 的递归 future 经非 Send 的 `LocalBoxFuture` 包装，
    /// 直接 `.await` 时需在非 Send 要求上下文中进行
    /// （如 `run_blocking`）；Fork 并行子任务在 `spawn_blocking` 线程内以
    /// current-thread runtime 驱动（`drive`，同 `tests/concurrency_stress.rs`）。
    pub async fn run(&mut self, action: Action) -> Result<Value, SysError> {
        interpret_impl(
            action,
            &mut self.context,
            &mut self.undo_stack,
            &mut self.resource_registry,
            ExecAccess::Shared(self.executor.clone()),
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
            ExecAccess::Shared(executor),
            0,
            None,
        ))
    }

    /// 恢复效果上下文：执行全部累积逆操作（pdr.md §5.1.3 recoverΓ）。
    pub async fn recover(&mut self) {
        self.undo_stack.recover().await;
    }
}

/// 子 Action 递归用的本地 future 包装。
///
/// 保留非 `Send` 的 `Pin<Box<dyn Future>>` 别名（区别于 syscall.rs 的
/// `BoxFuture` 强制 `+ Send`）：虽 `SyscallExecutor: Send`（D19）后 `&mut dyn`
/// 已可 Send，但保持最小改动，不把递归 future Send 化（Send 化留待后续）。
type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

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
///   Sleep 永不触发）；
/// - 慢 syscall（墙钟 100ms > 10ms）→ 墙钟通道触发（原 `timeout_fires_on_timeout`
///   语义，VC 下保持）；
/// - 瞬时完成的 inner（虚拟 0ms < deadline）→ 返回 inner 结果（错误原样透传）。
///
/// 注意：墙钟通道仍有取消语义（真实超时丢弃飞行中 future，RFC-12 残余缺口
/// 仅此通道适用）；虚拟通道无「飞行中」状态、无取消。
///
/// 栈帧纪律：deadline/elapsed 跨 await 存活，必须放在本独立 async fn 内 ——
/// 若内联进 `interpret_impl` 状态机，会撑大每层递归的状态机帧、压低 RFC-11
/// 深度守卫的嵌套边界（r5a 边界测试 95/96/97 实测：内联时 95 层即栈溢出）。
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
/// **提取为独立 async fn 的原因**同 `cancellable_sleep`：隔离 `select!`
/// 轮询栈帧，保护 RFC-11 深度守卫的栈预算。
async fn wait_timeout<'a>(
    mut inner: LocalBoxFuture<'a, Result<Value, SysError>>,
    duration: Duration,
    cancel_tx: &tokio::sync::watch::Sender<bool>,
) -> (bool, Result<Value, SysError>) {
    let sleep = tokio::time::sleep(duration);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            r = &mut inner => return (false, r),
            _ = &mut sleep => {
                // 广播取消：并行 Fork 分支检查后快速返回（join 见下）。
                let _ = cancel_tx.send(true);
                // 有界宽限：等待并行分支把部分状态/undo 合并回父。
                let grace = tokio::time::sleep(CANCEL_JOIN_GRACE);
                tokio::pin!(grace);
                tokio::select! {
                    r = &mut inner => return (true, r),
                    _ = &mut grace => return (true, Err(CANCELLED_ERR)),
                }
            }
        }
    }
}
/// 墙钟 Timeout 取消传播实现（RFC-08/09/12 残余统一修复）。
///
/// 独立 async fn（非解释器状态机内联）：取消协议的局部状态（watch 通道、
/// CancelToken、线性快照）若留在 `interpret_impl` 的 match 臂内，会撑大
/// **每层递归**的状态机帧（RFC-11 深度守卫 95/96/97 边界实测会栈溢出）——
/// 提取后解释器帧只持一个 BoxFuture 指针。VC 路径见 `run_virtual_timeout`。
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
    let (timed_out, inner_result) = wait_timeout(inner_fut, duration, &cancel_tx).await;
    if !timed_out {
        // 超时前完成：inner 效果全部保留（原语义）。
        return inner_result;
    }
    // 超时取消：先回滚 inner 已入栈 undo（异步，可含 IO），
    // 再回滚 inner 新增的线性标记，最后执行 on_timeout。
    drop(inner_result);
    undo.rollback_from(undo_mark).await;
    reg.rollback_linear_to(&linear_snap);
    run_sub_impl(
        on_timeout,
        ctx,
        undo,
        reg,
        access.reborrow(),
        depth,
        cancel,
    )
    .await
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
fn fork_fd_region_offset() -> u64 {
    let k = FORK_FD_REGION_SEQ.fetch_add(1, Ordering::Relaxed);
    assert!(
        k < (1 << 16),
        "Fork 全局 fd 区间序号耗尽（>2^16-1 个右分支）—— 静态计数上限"
    );
    k << 48
}

/// Fork 并行分支执行（D14 阶段 3，CTO 批准方向）：
///
/// 两分支各自持有 registry/context 隔离副本（D13：Clone）+ 独立 UndoStack，
/// 经共享执行器通道（Arc<Mutex>，每 Syscall 调用互斥 —— 锁仅保护执行器内部
/// 状态，物理 IO 本身异步）在**独立阻塞线程**上以 current-thread runtime 并发
/// 驱动（参考 concurrency_stress.rs 已验证的 drive 模式；外层 tokio::spawn N +
/// 内层 spawn_blocking 驱动 current-thread runtime）。
///
/// 完成后合并回父：
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
/// 分支把令牌克隆进 `spawn_blocking` 闭包——取消广播后，分支在下一 op
/// 边界检查到并快速返回（部分 registry/undo 照常合并回父，由 Timeout 臂
/// 统一回滚），实现「分支取消时传播给并行子任务」的结构化并发近似。
async fn run_fork_parallel(
    left: Action,
    right: Action,
    ctx: &mut Context,
    reg: &mut ResourceRegistry,
    undo: &mut UndoStack,
    shared: SharedExecutor,
    cancel: Option<&mut CancelToken>,
) -> Result<(Value, Value), SysError> {
    // 子任务隔离副本（D13）与独立撤销栈。
    let mut l_ctx = ctx.clone();
    let mut r_ctx = ctx.clone();
    let mut l_reg = reg.clone();
    let mut r_reg = reg.clone();
    // F1 修复：spawn 前取全局唯一 fd 区间 —— 右分支偏移 `k<<48`（k 全局唯一，
    // 见 `FORK_FD_REGION_SEQ` 注释），任意嵌套深度下并发分支区间互斥。
    r_reg.offset_next_fd(fork_fd_region_offset());
    let mut l_undo = UndoStack::new();
    let mut r_undo = UndoStack::new();
    let l_shared = shared.clone();
    let r_shared = shared.clone();
    // 取消令牌克隆进分支（watch Receiver 克隆：每分支独立 seen 状态，
    // 并发等待互不干扰；取消广播后分支在下一 op 边界快速返回）。
    let mut l_cancel = cancel.map(|c| CancelToken::clone(c));
    let mut r_cancel = l_cancel.clone();

    // 两个独立阻塞线程并发驱动（真并行；执行器调用经锁互斥串行化）。
    // 子任务把（结果, 隔离 registry, 独立撤销栈, 隔离 ctx）带回，供完成后合并
    // —— ctx 带回用于虚拟时钟合并（审计 R1 状态-MEDIUM-1，见下）。
    // 子任务在独立阻塞线程（`spawn_blocking`，全新栈）上驱动 —— 深度计数器
    // 从 0 重新起算（栈预算与父线程互不共享；RFC-11 守卫按线程栈独立生效）。
    let l_task = tokio::task::spawn_blocking(move || {
        let v = drive(interpret_impl(
            left,
            &mut l_ctx,
            &mut l_undo,
            &mut l_reg,
            ExecAccess::Shared(l_shared),
            0,
            l_cancel.as_mut(),
        ));
        (v, l_reg, l_undo, l_ctx)
    });
    let r_task = tokio::task::spawn_blocking(move || {
        let v = drive(interpret_impl(
            right,
            &mut r_ctx,
            &mut r_undo,
            &mut r_reg,
            ExecAccess::Shared(r_shared),
            0,
            r_cancel.as_mut(),
        ));
        (v, r_reg, r_undo, r_ctx)
    });

    #[allow(unused_mut, unused_variables)]
    let (l_res, l_reg, l_undo, mut l_ctx) = l_task.await.expect("Fork 并行左分支任务 panic");
    #[allow(unused_mut, unused_variables)]
    let (r_res, r_reg, r_undo, mut r_ctx) = r_task.await.expect("Fork 并行右分支任务 panic");

    // 合并回父（D13「完成后合并回父」/ RFC-A3-2）：
    // fd 不冲突（F1：右分支区间预分割 + D1 单调，子分配 ≥ 自身 next_fd
    // 且两分支区间不相交；合并时父 next_fd = max 归一化 = 全部已分配 fd + 1）。
    reg.merge(l_reg);
    reg.merge(r_reg);
    // undo：先并入 left 再并入 right —— 栈序 [left, right] 与顺序路径一致
    // （left 先执行先压栈、right 后执行后压栈），LIFO recover 先弹 right 的
    // undo 再弹 left 的（观察序：right 的效果后发生、先撤销）。
    undo.append(l_undo);
    undo.append(r_undo);
    // 审计 R1 状态-MEDIUM-1 修复：并行分支的虚拟时钟推进合并回父（sum，与
    // 顺序路径「分支依次推进父时钟」观察等价）——此前分支克隆时钟被丢弃，
    // 同一蓝图并行/顺序两种调度产生不同可观察时钟（确定性重放支柱被破坏）。
    #[cfg(feature = "virtual-clock")]
    {
        let base = ctx.virtual_clock_mut().map(|vc| vc.now()).unwrap_or_default();
        let l_now = l_ctx.virtual_clock_mut().map(|vc| vc.now()).unwrap_or(base);
        let r_now = r_ctx.virtual_clock_mut().map(|vc| vc.now()).unwrap_or(base);
        if let Some(vc) = ctx.virtual_clock_mut() {
            vc.advance(l_now.saturating_sub(base));
            vc.advance(r_now.saturating_sub(base));
        }
    }

    Ok((l_res?, r_res?))
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
                        Ok((v, maybe_undo)) => {
                            if let Some(u) = maybe_undo {
                                undo.push(u);
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
                let parallel = !conflict && matches!(&access, ExecAccess::Shared(_));
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
                    r_reg.offset_next_fd(fork_fd_region_offset());
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
                    // 并行路径：子任务隔离副本 + 共享执行器，完成后合并回父。
                    let shared = match &access {
                        ExecAccess::Shared(arc) => arc.clone(),
                        ExecAccess::Direct(_) => unreachable!("并行分支仅 Shared 通道可达"),
                    };
                    let (lv, rv) = run_fork_parallel(
                        *left,
                        *right,
                        ctx,
                        reg,
                        undo,
                        shared,
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
                undo.recover().await;
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
                    // 会撑大**每层递归**的状态机帧（r5a 边界测试 95/96/97 实测
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
