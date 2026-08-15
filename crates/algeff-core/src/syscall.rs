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
/// 逆操作：执行 `() -> ()` 的异步闭包。LIFO 撤销。
pub type UndoOp = Pin<Box<dyn Future<Output = ()> + Send>>;

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
    /// 执行一个 DataOp，返回（结果值, 可选逆操作）。
    /// 只有可逆操作（撤销策略 Full，pdr.md §11.2）返回逆操作。
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>>;

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
