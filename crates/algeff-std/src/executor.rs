//! Tokio 物理执行器 —— A5 交付（pdr.md §2.2 / §11 / §12.2）。
//!
//! 实现 `SyscallExecutor`：每个 DataOp 在 tokio 上执行，返回
//! `(Value, Option<UndoOp>)`。可逆操作（撤销策略 Full）返回逆操作；
//! 不可逆操作（UdpSendTo/Kill/SendSignal）不返回逆操作（补偿挂钩由用户提供）。

use algeff_core::action::{DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::ResourceRegistry;
use algeff_core::syscall::{SyscallExecutor, UndoOp};

/// 默认物理执行器（无内部状态；句柄都存于 ResourceRegistry）。
#[derive(Debug, Default)]
pub struct TokioExecutor;

impl TokioExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl SyscallExecutor for TokioExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> algeff_core::syscall::BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async { todo!("A5: 实现 DataOp 物理执行（contracts.md §任务 A5）") })
    }
}
