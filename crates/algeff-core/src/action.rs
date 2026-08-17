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

/// 蓝图 CPS 闭包（D18 契约修订，CTO 裁决）：Fork 线程级并行
/// （pdr.md §19.2「Fork → tokio::spawn 合法」）要求 `Action` 可跨线程
/// （`spawn_blocking` 闭包须 `Send`），四类闭包统一加 `+ Send`；
/// 捕获数据须为 `Send`（仓库内全部现有蓝图闭包捕获均 Send，已验证）。
pub type NextFn = Box<dyn FnOnce(Value) -> Action + Send>;
pub type CondFn = Box<dyn FnOnce(&Value) -> bool + Send>;
pub type CombineFn = Box<dyn FnOnce(Value, Value) -> Action + Send>;
pub type HandlerFn = Box<dyn FnOnce(SysError) -> Action + Send>;

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
    Open {
        path: PathBuf,
        flags: OpenFlags,
    },
    Read {
        fd: Fd,
        len: usize,
    },
    Write {
        fd: Fd,
        data: Bytes,
    },
    Close {
        fd: Fd,
    },
    Seek {
        fd: Fd,
        offset: i64,
        whence: std::io::SeekFrom,
    },
    Stat {
        path: PathBuf,
    },
    Chmod {
        path: PathBuf,
        mode: u32,
    },
    Chown {
        path: PathBuf,
        uid: u32,
        gid: u32,
    },
    Truncate {
        path: PathBuf,
        len: usize,
    },
    Unlink {
        path: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    // 目录
    Mkdir {
        path: PathBuf,
        mode: u32,
    },
    Rmdir {
        path: PathBuf,
    },
    ReadDir {
        path: PathBuf,
    },
    // 网络 TCP
    TcpBind {
        addr: SocketAddr,
    },
    TcpAccept {
        listener: Fd,
    },
    TcpConnect {
        addr: SocketAddr,
    },
    TcpRead {
        fd: Fd,
        len: usize,
    },
    TcpWrite {
        fd: Fd,
        data: Bytes,
    },
    TcpShutdown {
        fd: Fd,
        how: std::net::Shutdown,
    },
    // 网络 UDP
    UdpBind {
        addr: SocketAddr,
    },
    UdpRecvFrom {
        fd: Fd,
        len: usize,
    },
    UdpSendTo {
        fd: Fd,
        data: Bytes,
        addr: SocketAddr,
    },
    // 管道（跨平台实现：tokio duplex，见 contracts.md 决策 D5）
    PipeOpen {
        flags: PipeFlags,
    },
    // 进程
    Spawn {
        cmd: std::process::Command,
    },
    Kill {
        pid: Pid,
        signal: Signal,
    },
    Wait {
        pid: Pid,
    },
    // 信号
    SendSignal {
        signal: Signal,
        pid: Pid,
    },
    // 内存
    Mmap {
        path: PathBuf,
        len: usize,
        prot: MmapProt,
    },
    Munmap {
        addr: usize,
        len: usize,
    },
    // 时间
    GetTime,
    // 同步
    MutexLock {
        id: u64,
    },
    MutexUnlock {
        id: u64,
    },
    // 其他
    SendFile {
        out: Fd,
        input: Fd,
        offset: usize,
        len: usize,
    }, // 契约 D8：in→input（保留字）
    Dup {
        fd: Fd,
    },
    Dup2 {
        old_fd: Fd,
        new_fd: Fd,
    },
}

/// 静态代数角色（D-0xx P2）：操作 w ∈ M 的可逆性分类，构造期可查询。
/// 与运行时 `UndoCapability` 的关系：`role()` 是静态提示（最优情况），
/// 实际以 `execute` 返回为准（如 Open(truncate 大文件) 静态 Invertible
/// 但运行时可能 NonInvertible）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoRole {
    /// 单位元：w = id_M（无副作用）。
    Identity,
    /// 可逆：∃w̄, w;w̄ = 1（逆的构造可能依赖运行时条件）。
    Invertible,
    /// 不可逆：无逆元（投递/消费/删除语义不可回滚）。
    NonInvertible,
}

impl DataOp {
    /// 静态代数角色（D-0xx P2）：无需执行即可知的可逆性分类。
    /// 用于构造期检查（do_! 宏 warning / dx 显式声明），非运行时保证。
    pub const fn role(&self) -> UndoRole {
        use DataOp::*;
        match self {
            // 单位元（无外部可观察副作用）：读/查询/时间/资源生命周期
            // （close/dup/bind/connect 的副作用随 reg.clear/drop 回归 → Identity）。
            Read { .. }
            | Stat { .. }
            | ReadDir { .. }
            | GetTime
            | Mmap { .. }
            | Munmap { .. }
            | Close { .. }
            | Dup { .. }
            | Dup2 { .. }
            | MutexUnlock { .. }
            | PipeOpen { .. }
            | TcpBind { .. }
            | TcpAccept { .. }
            | TcpConnect { .. }
            | UdpBind { .. } => UndoRole::Identity,
            // 可逆：内容/游标/元数据变更（逆构造可能依赖运行时条件）
            Write { .. }
            | Seek { .. }
            | Rename { .. }
            | Truncate { .. }
            | Mkdir { .. }
            | Open { .. }
            | SendFile { .. }
            | Chmod { .. }
            | Chown { .. }
            | MutexLock { .. } => UndoRole::Invertible,
            // 不可逆：投递/消费/删除/信号
            Unlink { .. }
            | Rmdir { .. }
            | TcpRead { .. }
            | TcpWrite { .. }
            | TcpShutdown { .. }
            | UdpRecvFrom { .. }
            | UdpSendTo { .. }
            | Spawn { .. }
            | Kill { .. }
            | Wait { .. }
            | SendSignal { .. } => UndoRole::NonInvertible,
        }
    }

    /// 确定性（D-0xx P3）：可重放性维度——墙钟时间/网络投递/外部进程
    /// 结果不确定，virtual-clock feature 下不可重放。
    pub const fn is_deterministic(&self) -> bool {
        use DataOp::*;
        // 墙钟时间/网络投递/外部进程/网络消费结果不确定（virtual-clock 下不可重放）。
        !matches!(
            self,
            GetTime
                | UdpRecvFrom { .. }
                | UdpSendTo { .. }
                | TcpRead { .. }
                | TcpWrite { .. }
                | TcpAccept { .. }
                | Spawn { .. }
                | Wait { .. }
        )
    }
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
    /// 幂等执行（D-0xx 幂等键状态机）：带全局幂等键的副作用段。
    ///
    /// 状态机：`COMMITTED → REVERTED`（PENDING 留待全局共享注册表升级，
    /// per-Runtime 单线程天然串行，无需中间态）。
    /// - 执行时查键：COMMITTED 未 REVERTED → 返回缓存结果，**不执行 inner**；
    /// - 未命中/REVERTED → 执行 inner（undo 压栈），成功后键置 COMMITTED 并缓存结果
    ///   （含 Fd/Pid 的结果不缓存，重试 fallback Unit）；
    /// - 该段的 undo 被 recover 执行（Replace/Scope 退出）→ 键置 REVERTED，
    ///   允许未来重新执行（恰好一次语义：生命周期内副作用只真正发生一次）。
    /// - inner 内含 Replace（自清理副作用）→ 不 COMMIT（undo 长度回落检测），
    ///   重试重新执行。
    Idempotent {
        key: String,
        inner: Box<Action>,
        next: NextFn,
    },
}

/// 便捷构造：`Pure(())`。
pub fn unit() -> Action {
    Action::Pure(Value::Unit)
}
