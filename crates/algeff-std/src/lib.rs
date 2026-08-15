//! Algeff 标准适配层：tokio 物理后端。
//!
//! A5 拥有本目录：`executor.rs`（TokioExecutor，实现全部 DataOp）、
//! `adapters.rs`（预包装适配器 open_tcp/read/write/close…，pdr.md §14 风格）。

pub mod adapters;
pub mod executor;

pub use executor::TokioExecutor;

pub mod prelude {

    pub use crate::executor::TokioExecutor;
}
