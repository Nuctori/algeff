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
