//! Action AST —— 契约冻结（pdr.md §2，contracts.md §类型冻结）。
//!
//! 所有节点采用 CPS：`next` 一致为 `NextFn`。枚举不含泛型参数，
//! 类型信息由运行时通过 Context 追踪。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::SysError;
use crate::resource::ResourceSet;

/// 运行时分配的全局唯一句柄（非 OS fd，单调分配、永不复用，见 contracts.md）。
pub type Fd = u64;
pub type Pid = u32;
/// 外部调用/组件标识。
pub type Id = u64;
/// OS 信号编号（如 2 = SIGINT）。
pub type Signal = i32;
pub type Bytes = Vec<u8>;

/// 运行时值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Unit,
    Bool(bool),
    U64(u64),
    I64(i64),
    Bytes(Bytes),
    Str(String),
    Fd(Fd),
    Pid(Pid),
    Addr(SocketAddr),
    List(Vec<Value>),
}

pub type NextFn = Box<dyn FnOnce(Value) -> Action>;
pub type CondFn = Box<dyn FnOnce(&Value) -> bool>;
pub type CombineFn = Box<dyn FnOnce(Value, Value) -> Action>;
pub type HandlerFn = Box<dyn FnOnce(SysError) -> Action>;

/// Open 的访问标志（手写结构体，不引入 bitflags 依赖）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub create: bool,
    pub truncate: bool,
    pub exclusive: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipeFlags {
    pub nonblocking: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MmapProt {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

/// 数据平面原语（pdr.md §2.2）。
#[derive(Debug)]
pub enum DataOp {
    // 文件
    Open { path: PathBuf, flags: OpenFlags },
    Read { fd: Fd, len: usize },
    Write { fd: Fd, data: Bytes },
    Close { fd: Fd },
    Seek { fd: Fd, offset: i64, whence: std::io::SeekFrom },
    Stat { path: PathBuf },
    Chmod { path: PathBuf, mode: u32 },
    Chown { path: PathBuf, uid: u32, gid: u32 },
    Truncate { path: PathBuf, len: usize },
    Unlink { path: PathBuf },
    Rename { from: PathBuf, to: PathBuf },
    // 目录
    Mkdir { path: PathBuf, mode: u32 },
    Rmdir { path: PathBuf },
    ReadDir { path: PathBuf },
    // 网络 TCP
    TcpBind { addr: SocketAddr },
    TcpAccept { listener: Fd },
    TcpConnect { addr: SocketAddr },
    TcpRead { fd: Fd, len: usize },
    TcpWrite { fd: Fd, data: Bytes },
    TcpShutdown { fd: Fd, how: std::net::Shutdown },
    // 网络 UDP
    UdpBind { addr: SocketAddr },
    UdpRecvFrom { fd: Fd, len: usize },
    UdpSendTo { fd: Fd, data: Bytes, addr: SocketAddr },
    // 管道（跨平台实现：tokio duplex，见 contracts.md 决策 D5）
    PipeOpen { flags: PipeFlags },
    // 进程
    Spawn { cmd: std::process::Command },
    Kill { pid: Pid, signal: Signal },
    Wait { pid: Pid },
    // 信号
    SendSignal { signal: Signal, pid: Pid },
    // 内存
    Mmap { path: PathBuf, len: usize, prot: MmapProt },
    Munmap { addr: usize, len: usize },
    // 时间
    GetTime,
    // 同步
    MutexLock { id: u64 },
    MutexUnlock { id: u64 },
    // 其他
    SendFile { out: Fd, input: Fd, offset: usize, len: usize }, // 契约 D8：in→input（保留字）
    Dup { fd: Fd },
    Dup2 { old_fd: Fd, new_fd: Fd },
}

/// 蓝图（pdr.md §2.1）。注意：不含 Debug（NextFn 闭包不可 Debug）。
pub enum Action {
    Pure(Value),
    Syscall {
        op: DataOp,
        resources: ResourceSet,
        next: NextFn,
    },
    Choose {
        cond: CondFn,
        then_branch: Box<Action>,
        else_branch: Box<Action>,
    },
    Fork {
        left: Box<Action>,
        right: Box<Action>,
        combine: CombineFn,
    },
    Scope {
        base: PathBuf,
        inner: Box<Action>,
        next: NextFn,
    },
    Alloc {
        len: usize,
        next: NextFn,
    },
    Replace {
        target: Box<Action>,
    },
    Invoke {
        foreign_id: Id,
        captures: ResourceSet,
        yields: ResourceSet,
        deterministic: bool,
        next: NextFn,
    },
    Sleep {
        duration: Duration,
        next: NextFn,
    },
    WatchSignal {
        signal: Signal,
        next: NextFn,
    },
    Timeout {
        action: Box<Action>,
        duration: Duration,
        on_timeout: Box<Action>,
    },
    Sequential {
        current: Box<Action>,
        next: NextFn,
    },
    Catch {
        action: Box<Action>,
        handler: HandlerFn,
    },
}

/// 便捷构造：`Pure(())`。
pub fn unit() -> Action {
    Action::Pure(Value::Unit)
}
