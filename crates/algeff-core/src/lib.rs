//! Algeff 核心契约库（阶段 0 冻结）。
//!
//! 分层（pdr.md §1.2）：本 crate 承载「运行动态数层」与「API 辅助层」，
//! 不包含任何物理 IO 实现——物理执行由 `algeff-std` 的 `SyscallExecutor` 提供。

pub mod action;
pub mod error;
pub mod resource;
pub mod runtime;
pub mod syscall;

#[cfg(feature = "coeffects")]
pub mod coeffects;

#[cfg(feature = "virtual-clock")]
pub mod virtual_clock;

pub use action::*;
pub use error::SysError;
pub use resource::*;
pub use runtime::{Context, Runtime, UndoStack};
pub use syscall::{BoxFuture, SyscallExecutor, UndoOp};

/// 常用入口（pdr.md §14 示例风格）。
pub mod prelude {
    pub use crate::action::{
        Action, CombineFn, CondFn, DataOp, Fd, HandlerFn, Id, NextFn, Pid, Signal, Value,
    };
    pub use crate::error::SysError;
    pub use crate::resource::{
        AccessMode, AppendOnly, Owned, ReadOnly, Resource, ResourceArbiter, ResourceHandle,
        ResourceInner, ResourceRegistry, ResourceSet, ResourceUsage, TypedResource, WriteOnly,
    };
    pub use crate::runtime::{Context, Runtime, UndoStack};
    pub use crate::syscall::{SyscallExecutor, UndoOp};
}
