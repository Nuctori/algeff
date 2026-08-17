//! 系统调用执行器契约 —— 冻结（A2 解释器与 A5 物理后端之间的接口）。
//!
//! 决策 D4（contracts.md）：撤销操作为异步 future（`UndoOp`），
//! 由执行器在执行时动态构造并返回；解释器将其压入 UndoStack。

use std::future::Future;
use std::pin::Pin;

use crate::action::{DataOp, Id, Signal, Value};
use crate::error::SysError;
use crate::resource::{ResourceRegistry, ResourceSet};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// 逆操作：异步撤销闭包，返回 `Result` 供 recover 检查（撤销失败必须上报，
/// 语义真回归，D-098）。LIFO 撤销（(w₁;w₂)⁻¹ = w₂⁻¹;w₁⁻¹，非交换性补偿）。
pub type UndoOp = Pin<Box<dyn Future<Output = Result<(), SysError>> + Send>>;

/// 操作 w ∈ M（状态变换幺半群）的代数角色（撤销能力类型化，D-0xx）。
///
/// 三个角色是数学分类（`pdr.md` §5 可逆性 + A6 撤销双态）：
/// - 单位元（Identity）：w = id_M，无副作用，天然可回归；
/// - 可逆（Invertible）：∃w̄, w;w̄ = 1。逆的构造失败（部分逆定义域外，
///   如写前读失败）→ `execute` 以 Err 返回，**不静默降级**；
/// - 不可逆（NonInvertible）：无逆元（unlink/rmdir/close/tcp/udp/kill/
///   spawn/wait/signal），构造期可静态标注（`DataOp::role`），
///   Replace 闸门将拒绝回滚（组合不可逆性 = 子操作合取）。
pub enum UndoCapability {
    /// 单位元：w = id_M（read/stat/readdir/get_time）。
    Identity,
    /// 可逆元素：∃w̄, w;w̄ = 1。
    Invertible(UndoOp),
    /// 不可逆元素：无逆元。
    NonInvertible,
}

impl UndoCapability {
    pub const fn is_invertible(&self) -> bool {
        matches!(self, Self::Invertible(_))
    }
    pub const fn is_irreversible(&self) -> bool {
        matches!(self, Self::NonInvertible)
    }
}

// 手动实现（Invertible 含 future，无法 derive）：只比较代数角色。
impl PartialEq for UndoCapability {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Identity, Self::Identity)
                | (Self::Invertible(_), Self::Invertible(_))
                | (Self::NonInvertible, Self::NonInvertible)
        )
    }
}
impl Eq for UndoCapability {}
impl std::fmt::Debug for UndoCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Identity => f.write_str("Identity"),
            Self::Invertible(_) => f.write_str("Invertible(_)"),
            Self::NonInvertible => f.write_str("NonInvertible"),
        }
    }
}

/// 物理执行层抽象。`algeff-std` 的 `TokioExecutor` 是默认实现。
///
/// 方法返回 `BoxFuture` 而非 async fn，保证 trait dyn 兼容
/// （Runtime 以 `Box<dyn SyscallExecutor>` 持有）。
///
/// `Send` 超 trait（决策 D19，CTO 授权契约级变更）：`Runtime` 的共享执行器通道
/// （`Arc<Mutex<Box<dyn SyscallExecutor>>>`）要求跨线程传递，`Mutex<T>: Sync`
/// 需 `T: Send` —— 以编译期强制代替此前的 `unsafe impl Send for SendExecutor`
/// 包装（健全性此前依赖外部不变量，F3 审查项）。现有实现（TokioExecutor 与
/// 各测试 Mock）均为无状态/`Arc<Mutex>` 型，天然 Send。
pub trait SyscallExecutor: Send {
    /// 执行一个 DataOp，返回（结果值, 撤销能力）。
    /// 可逆操作（Invertible）返回逆操作；逆不可构造（部分逆定义域外，
    /// 如写前读失败）→ 以 Err 返回（不静默降级）；不可逆 → NonInvertible。
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>>;

    /// R-6 并行 Fork 分支执行器快照（阶段 3 并行兑现，D17 并行收益）。
    ///
    /// 返回一个与 `self` **共享全部内部状态**（per-fd 锁表 / 映射 / 仲裁器）的
    /// 独立执行器实例，供 Fork 并行分支独占驱动：分支对自身实例持 `&mut`
    /// 无跨分支竞争 → 物理 IO await 移出共享锁外，真并行（`run_fork_parallel`）。
    /// 状态共享保证语义不变：同一 fd 的物理 IO 仍在共享 per-fd 锁上串行（游标
    /// 语义），互斥锁/仲裁器/句柄映射跨分支一致（与 D17 共享执行器等价）。
    ///
    /// **默认 `None` = 不支持快照** → 运行时回退共享锁通道（D17 原行为，
    /// 对既有执行器零语义变化）。`TokioExecutor` 覆盖：克隆内部 `Arc` 状态表
    /// （O(1)，不复制物理句柄）。
    ///
    /// 注：本方法为 R-6 新增的**纯增量默认方法**（不改变任何既有方法签名，
    /// 冻结面 `execute` 契约原样保留）；快照执行器运行期间的映射变更经共享
    /// 状态表自动可见于父执行器，无需额外合并步骤。
    fn fork_snapshot(&mut self) -> Option<Box<dyn SyscallExecutor + Send>> {
        None
    }

    /// 信号监听（pdr.md §2.1 WatchSignal）。默认不支持（ENOSYS）。
    fn watch_signal<'a>(
        &'a mut self,
        _signal: Signal,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<Value, SysError>> {
        Box::pin(async { Err(SysError::Other(38)) })
    }

    /// 外部调用（pdr.md §2.1 Invoke）。默认不支持（ENOSYS）。
    fn invoke<'a>(
        &'a mut self,
        _foreign_id: Id,
        _captures: &'a ResourceSet,
        _deterministic: bool,
    ) -> BoxFuture<'a, Result<Value, SysError>> {
        Box::pin(async { Err(SysError::Other(38)) })
    }
}
