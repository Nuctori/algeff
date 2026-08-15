//! 预包装适配器（A5 交付）：以类型安全方式构造 Action 链。
//!
//! 目标 API（pdr.md §14 示例）：
//! - `open_tcp(addr) -> Action`、`read(fd, len) -> Action`、`write(fd, data) -> Action`、
//!   `close(fd) -> Action` 等；返回的 Action 可直接被 `Runtime::run` 执行，
//!   也可作为 `Sequential` 的片段组合。
