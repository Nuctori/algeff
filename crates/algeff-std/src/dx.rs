//! DX 语法糖层（迭代 1→2）：命令式 `do_!` 宏的运行时支撑 + 资源自动推导 +
//! 错误处理（`dx::catch`）。
//!
//! 本模块是 **A5 域纯增量层**：冻结面（algeff-core 的 action/error/syscall/
//! lib、contracts.md、pdr.md）零改动。哲学底线不变：
//!
//! 1. **蓝图 = 不可变数据**——`do_!` 宏展开后只是 `Action` 值的 CPS 构造
//!    （`and_then` 链），不引入任何新节点、不做任何类型魔法；
//! 2. **资源声明显式化**——展开后的每个 `Action::Syscall` 仍携带完整的
//!    `ResourceSet`（冲突检测 / 撤销跟踪的根基）。`infer_usage` 只是把
//!    「按 DataOp 推默认资源」的重复劳动自动化：自动推导 = 默认值，
//!    显式声明（手写 `Action::Syscall` 或 `adapters`）永远可以覆盖；
//! 3. **执行与构造分离**——本模块只构造 `Action`（不碰真实系统），
//!    执行仍由 `Runtime` + `TokioExecutor` 完成；
//! 4. 设计论证见 `docs/src/dx-design.md`。
//!
//! # 用法
//!
//! ```rust
//! use algeff_core::prelude::*;
//! use algeff_core::OpenFlags;
//! use algeff_macro::do_;
//! use algeff_std::dx;
//! use algeff_std::TokioExecutor;
//!
//! let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
//! let path = std::path::PathBuf::from("hello.txt");
//!
//! // 资源声明（写路径）与值传递（fd 贯穿）全部自动推导
//! let blueprint = do_! {
//!     let fd = dx::open(&path, OpenFlags { read: true, write: true, create: true, ..Default::default() });
//!     dx::write(&fd, b"hello dx".to_vec());
//!     dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
//!     let data = dx::read(&fd, 64);
//!     dx::close(&fd);
//!     data // 尾表达式 = 链的最终值
//! };
//!
//! assert!(matches!(blueprint, Action::Sequential { .. }));
//! let _ = rt; // 不实际执行（无副作用；构造期不触碰真实文件系统）
//! ```
//!
//! `do_!` 块内 **任何返回 `Action` 的表达式** 都可用作语句
//! （含 `plan!`/`scope!`/`choose!`/嵌套 `do_!`/`dx::catch(...)`），不限于本模块的操作；
//! 本模块提供的是「按 `DataOp` 自动推导资源」的预包装操作 + `&Value`
//! 句柄传参（do_! 的 `let` 绑定的是原始 `Value`）。
//! 迭代 2：补齐 DataOp 全覆盖（munmap/send_file/dup2）+ `dx::catch` 错误处理。

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use algeff_core::{
    AccessMode, Action, Bytes, DataOp, Fd, MmapProt, OpenFlags, Pid, PipeFlags, Resource,
    ResourceSet, ResourceUsage, Signal, SysError, Value,
};

pub use crate::adapters::{and_then, seq, then};

// ── 资源自动推导（DataOp → ResourceSet 默认表）─────────────────────────

fn usage(resource: Resource, mode: AccessMode) -> ResourceUsage {
    ResourceUsage { resource, mode }
}

fn path_usage(p: &Path, mode: AccessMode) -> ResourceUsage {
    usage(Resource::Path(p.to_string_lossy().into_owned()), mode)
}

fn fd_usage(fd: Fd, mode: AccessMode) -> ResourceUsage {
    usage(Resource::Fd(fd), mode)
}

fn pid_usage(pid: Pid, mode: AccessMode) -> ResourceUsage {
    usage(Resource::Pid(pid), mode)
}

/// 按操作推导默认资源声明（pdr.md §9 冲突矩阵 / §3 类型状态声明的自动化）。
///
/// 约定（与 `adapters` 手工声明对齐，覆盖更全）：
/// - Open：`flags.write` → Write、否则 `flags.append` → Append、否则 Read（路径）；
/// - Read/Seek → Read(fd)；Write → Write(fd)；Close → Own(fd)；
/// - 目录元操作按路径：Stat/ReadDir → Read，Mkdir/Chmod/Chown/Truncate/Rmdir → Write，
///   Unlink → Own（终结），Rename → Write(from)+Write(to)；
/// - 网络/进程等**运行时才分配句柄**的操作（TcpBind/TcpConnect/UdpBind/PipeOpen/
///   Spawn/GetTime/Munmap）→ 空集（新句柄无法静态声明，pdr.md §18 用户责任域）；
/// - MutexLock → Write(Fd(id))、MutexUnlock → Read(Fd(id))（对齐 adversarial_r2
///   攻击面 2a 的安全声明模式：Write 会被 A4 每资源至多消费一次，unlock 必须降为 Read）；
/// - 覆盖：任何调用方可用 `syscall_with` 或手写 `Action::Syscall` 显式声明覆盖。
pub fn infer_usage(op: &DataOp) -> ResourceSet {
    match op {
        // 文件
        DataOp::Open { path, flags } => {
            let mode = if flags.write {
                AccessMode::Write
            } else if flags.append {
                AccessMode::Append
            } else {
                AccessMode::Read
            };
            vec![path_usage(path, mode)]
        }
        DataOp::Read { fd, .. } => vec![fd_usage(*fd, AccessMode::Read)],
        DataOp::Write { fd, .. } => vec![fd_usage(*fd, AccessMode::Write)],
        DataOp::Close { fd } => vec![fd_usage(*fd, AccessMode::Own)],
        DataOp::Seek { fd, .. } => vec![fd_usage(*fd, AccessMode::Read)],
        DataOp::Stat { path } => vec![path_usage(path, AccessMode::Read)],
        DataOp::Chmod { path, .. } => vec![path_usage(path, AccessMode::Write)],
        DataOp::Chown { path, .. } => vec![path_usage(path, AccessMode::Write)],
        DataOp::Truncate { path, .. } => vec![path_usage(path, AccessMode::Write)],
        DataOp::Unlink { path } => vec![path_usage(path, AccessMode::Own)],
        DataOp::Rename { from, to } => vec![
            path_usage(from, AccessMode::Write),
            path_usage(to, AccessMode::Write),
        ],
        // 目录
        DataOp::Mkdir { path, .. } => vec![path_usage(path, AccessMode::Write)],
        DataOp::Rmdir { path } => vec![path_usage(path, AccessMode::Write)],
        DataOp::ReadDir { path } => vec![path_usage(path, AccessMode::Read)],
        // 网络 TCP
        DataOp::TcpBind { .. } | DataOp::TcpConnect { .. } => vec![],
        DataOp::TcpAccept { listener } => vec![fd_usage(*listener, AccessMode::Read)],
        DataOp::TcpRead { fd, .. } => vec![fd_usage(*fd, AccessMode::Read)],
        DataOp::TcpWrite { fd, .. } => vec![fd_usage(*fd, AccessMode::Write)],
        // 审计 R4-F1 修复：TcpShutdown 声明 Own（终结语义）而非 Write——
        // write→shutdown 是标准半关闭链，Write 声明会与 tcp_write 的 Write
        // 消费冲突（A4 至多一次）→ 误拒合法蓝图（对齐 e2e.rs 的 ow(sfd) 先例）。
        DataOp::TcpShutdown { fd, .. } => vec![fd_usage(*fd, AccessMode::Own)],
        // 网络 UDP
        DataOp::UdpBind { .. } => vec![],
        DataOp::UdpRecvFrom { fd, .. } => vec![fd_usage(*fd, AccessMode::Read)],
        DataOp::UdpSendTo { fd, .. } => vec![fd_usage(*fd, AccessMode::Write)],
        // 管道
        DataOp::PipeOpen { .. } => vec![],
        // 进程
        DataOp::Spawn { .. } => vec![],
        DataOp::Kill { pid, .. } => vec![pid_usage(*pid, AccessMode::Write)],
        DataOp::Wait { pid } => vec![pid_usage(*pid, AccessMode::Own)],
        // 信号
        DataOp::SendSignal { pid, .. } => vec![
            usage(Resource::Signal, AccessMode::Write),
            pid_usage(*pid, AccessMode::Write),
        ],
        // 内存
        DataOp::Mmap { path, prot, .. } => {
            let mode = if prot.write {
                AccessMode::Write
            } else {
                AccessMode::Read
            };
            vec![path_usage(path, mode)]
        }
        DataOp::Munmap { .. } => vec![],
        // 时间
        DataOp::GetTime => vec![],
        // 同步
        // 审计 R4-F2 修复：MutexLock 声明 Read(Fd(id)) 而非 Write——
        // (a) 锁 id 与真实 fd 共享 u64 空间，Write 声明会与同值 fd 的
        //     Write 消费冲突（A4 至多一次）→ 误拒；Read 无消费语义。
        // (b) 同链 lock→unlock→lock 循环：unlock 声明 Read 不撤销 Write
        //     消费 → 第二次 lock 被 A4 误拒；Read 声明下循环可重入。
        //     对齐核心测试实践（adversarial_r2 的 rd(id) 重入场景）。
        DataOp::MutexLock { id } => vec![fd_usage(*id, AccessMode::Read)],
        DataOp::MutexUnlock { id } => vec![fd_usage(*id, AccessMode::Read)],
        // 其他
        DataOp::SendFile { out, input, .. } => vec![
            fd_usage(*out, AccessMode::Write),
            fd_usage(*input, AccessMode::Read),
        ],
        DataOp::Dup { fd } => vec![fd_usage(*fd, AccessMode::Write)],
        DataOp::Dup2 { old_fd, new_fd } => vec![
            fd_usage(*new_fd, AccessMode::Write),
            fd_usage(*old_fd, AccessMode::Read),
        ],
    }
}

// ── Action 构造：自动推导 + 显式覆盖 ──────────────────────────────────

/// 按 `infer_usage` 自动推导资源声明，构造 `Action::Syscall` 节点
/// （next 收敛为 `Pure`，值经 `and_then` 交付）。
pub fn syscall(op: DataOp) -> Action {
    let resources = infer_usage(&op);
    syscall_with(op, resources)
}

/// 显式指定资源声明构造 Syscall 节点——**覆盖**自动推导（手动覆盖入口）。
pub fn syscall_with(op: DataOp, resources: ResourceSet) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(Action::Pure),
    }
}

/// `Pure(v)` 便捷构造。
pub fn pure(v: Value) -> Action {
    Action::Pure(v)
}

/// `Pure(Unit)` 便捷构造。
pub fn unit() -> Action {
    Action::Pure(Value::Unit)
}

// ── 值提取（do_! 的 `let` 绑定的是原始 `Value`，使用处按类型提取）─────

/// 从 `Value` 提取 Fd（borrow 语义：多处使用同一绑定不移动）。
pub fn expect_fd(v: &Value) -> Fd {
    match v {
        Value::Fd(fd) => *fd,
        other => panic!("dx: 期望 Fd，得到 {other:?}"),
    }
}

/// 从 `Value` 提取 Pid。
pub fn expect_pid(v: &Value) -> Pid {
    match v {
        Value::Pid(pid) => *pid,
        other => panic!("dx: 期望 Pid，得到 {other:?}"),
    }
}

/// 从 `Value` 提取 Bytes（克隆）。
pub fn expect_bytes(v: &Value) -> Bytes {
    match v {
        Value::Bytes(b) => b.clone(),
        other => panic!("dx: 期望 Bytes，得到 {other:?}"),
    }
}

/// 从 `Value` 提取 u64。
pub fn expect_u64(v: &Value) -> u64 {
    match v {
        Value::U64(n) => *n,
        other => panic!("dx: 期望 U64，得到 {other:?}"),
    }
}

/// 从 `Value` 提取 String（克隆）。
pub fn expect_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => panic!("dx: 期望 Str，得到 {other:?}"),
    }
}

/// 从 `Value` 提取 SocketAddr。
pub fn expect_addr(v: &Value) -> SocketAddr {
    match v {
        Value::Addr(a) => *a,
        other => panic!("dx: 期望 Addr，得到 {other:?}"),
    }
}

/// 从 `Value` 提取 List（克隆）。
pub fn expect_list(v: &Value) -> Vec<Value> {
    match v {
        Value::List(l) => l.clone(),
        other => panic!("dx: 期望 List，得到 {other:?}"),
    }
}

// ── 预包装操作（全部经 infer_usage 自动推导资源）───────────────────────

// 文件

/// 打开文件（资源按 flags 推导：write → Write、append → Append、否则 Read）。
pub fn open(path: impl AsRef<Path>, flags: OpenFlags) -> Action {
    syscall(DataOp::Open {
        path: path.as_ref().to_path_buf(),
        flags,
    })
}

/// 从 fd 读取至多 len 字节（返回 Value::Bytes）。
pub fn read(fd: &Value, len: usize) -> Action {
    syscall(DataOp::Read {
        fd: expect_fd(fd),
        len,
    })
}

/// 向 fd 写入数据（返回 Unit）。
pub fn write(fd: &Value, data: impl Into<Bytes>) -> Action {
    syscall(DataOp::Write {
        fd: expect_fd(fd),
        data: data.into(),
    })
}

/// 移动 fd 的文件偏移（返回 Unit）。
pub fn seek(fd: &Value, offset: i64, whence: std::io::SeekFrom) -> Action {
    syscall(DataOp::Seek {
        fd: expect_fd(fd),
        offset,
        whence,
    })
}

/// 关闭 fd（Own 语义：唯一持有者释放物理资源）。
pub fn close(fd: &Value) -> Action {
    syscall(DataOp::Close { fd: expect_fd(fd) })
}

/// 文件元数据（返回 Value::List([len, is_dir, is_file])）。
pub fn stat(path: impl AsRef<Path>) -> Action {
    syscall(DataOp::Stat {
        path: path.as_ref().to_path_buf(),
    })
}

/// 修改文件权限。
pub fn chmod(path: impl AsRef<Path>, mode: u32) -> Action {
    syscall(DataOp::Chmod {
        path: path.as_ref().to_path_buf(),
        mode,
    })
}

/// 修改文件属主。
pub fn chown(path: impl AsRef<Path>, uid: u32, gid: u32) -> Action {
    syscall(DataOp::Chown {
        path: path.as_ref().to_path_buf(),
        uid,
        gid,
    })
}

/// 截断文件（<1MB 时可逆）。
pub fn truncate(path: impl AsRef<Path>, len: usize) -> Action {
    syscall(DataOp::Truncate {
        path: path.as_ref().to_path_buf(),
        len,
    })
}

/// 删除文件（不可逆；补偿挂钩由用户提供）。
pub fn unlink(path: impl AsRef<Path>) -> Action {
    syscall(DataOp::Unlink {
        path: path.as_ref().to_path_buf(),
    })
}

/// 重命名（可逆：undo 反向 Rename）。
pub fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Action {
    syscall(DataOp::Rename {
        from: from.as_ref().to_path_buf(),
        to: to.as_ref().to_path_buf(),
    })
}

/// 内存映射文件（返回 Value::Bytes；prot.write 时资源按 Write 推导）。
pub fn mmap(path: impl AsRef<Path>, len: usize, prot: MmapProt) -> Action {
    syscall(DataOp::Mmap {
        path: path.as_ref().to_path_buf(),
        len,
        prot,
    })
}

/// 解除内存映射（no-op 语义，资源空集）。
pub fn munmap(addr: usize, len: usize) -> Action {
    syscall(DataOp::Munmap { addr, len })
}

/// 复制 fd（共享同一句柄）。
pub fn dup(fd: &Value) -> Action {
    syscall(DataOp::Dup { fd: expect_fd(fd) })
}

/// 把 `old_fd` 复制到 `new_fd`（决策 D1 降级语义：fd 单调分配永不复用，
/// 结果 fd 恒 ≠ new_fd——「先关 new_fd，再复制到新 fd」，见 tests/executor.rs a2）。
pub fn dup2(old_fd: &Value, new_fd: Fd) -> Action {
    syscall(DataOp::Dup2 {
        old_fd: expect_fd(old_fd),
        new_fd,
    })
}

/// 零拷贝：从 input fd 向 out fd 复制 offset 起 len 字节（返回 Unit）。
pub fn send_file(out: &Value, input: &Value, offset: usize, len: usize) -> Action {
    syscall(DataOp::SendFile {
        out: expect_fd(out),
        input: expect_fd(input),
        offset,
        len,
    })
}

// 目录

/// 创建目录。
pub fn mkdir(path: impl AsRef<Path>, mode: u32) -> Action {
    syscall(DataOp::Mkdir {
        path: path.as_ref().to_path_buf(),
        mode,
    })
}

/// 删除空目录。
pub fn rmdir(path: impl AsRef<Path>) -> Action {
    syscall(DataOp::Rmdir {
        path: path.as_ref().to_path_buf(),
    })
}

/// 列出目录项（返回 Value::List(Str)）。
pub fn read_dir(path: impl AsRef<Path>) -> Action {
    syscall(DataOp::ReadDir {
        path: path.as_ref().to_path_buf(),
    })
}

// 网络 TCP

/// 绑定 TCP 监听（返回 Value::Fd）。
pub fn open_tcp(addr: SocketAddr) -> Action {
    syscall(DataOp::TcpBind { addr })
}

/// 接受一个连接（listener 为 `open_tcp` 的返回值；返回 Value::Fd）。
pub fn accept(listener: &Value) -> Action {
    syscall(DataOp::TcpAccept {
        listener: expect_fd(listener),
    })
}

/// 建立 TCP 连接（返回 Value::Fd）。
pub fn connect(addr: SocketAddr) -> Action {
    syscall(DataOp::TcpConnect { addr })
}

/// 从 TCP 连接读取至多 len 字节（返回 Value::Bytes）。
pub fn tcp_read(fd: &Value, len: usize) -> Action {
    syscall(DataOp::TcpRead {
        fd: expect_fd(fd),
        len,
    })
}

/// 向 TCP 连接写入数据。
pub fn tcp_write(fd: &Value, data: impl Into<Bytes>) -> Action {
    syscall(DataOp::TcpWrite {
        fd: expect_fd(fd),
        data: data.into(),
    })
}

/// 半关闭 TCP 连接。
pub fn tcp_shutdown(fd: &Value, how: std::net::Shutdown) -> Action {
    syscall(DataOp::TcpShutdown {
        fd: expect_fd(fd),
        how,
    })
}

// 网络 UDP

/// 绑定 UDP 套接字（返回 Value::Fd）。
pub fn udp_bind(addr: SocketAddr) -> Action {
    syscall(DataOp::UdpBind { addr })
}

/// 从 UDP 套接字接收（返回 Value::Bytes）。
pub fn udp_recv_from(fd: &Value, len: usize) -> Action {
    syscall(DataOp::UdpRecvFrom {
        fd: expect_fd(fd),
        len,
    })
}

/// 向 UDP 套接字发送。
pub fn udp_send_to(fd: &Value, data: impl Into<Bytes>, addr: SocketAddr) -> Action {
    syscall(DataOp::UdpSendTo {
        fd: expect_fd(fd),
        data: data.into(),
        addr,
    })
}

// 管道

/// 打开内存管道（返回 Value::List([reader_fd, writer_fd])）。
pub fn pipe_open() -> Action {
    syscall(DataOp::PipeOpen {
        flags: PipeFlags::default(),
    })
}

// 进程

/// 派生子进程（返回 Value::Pid）。
pub fn spawn(cmd: std::process::Command) -> Action {
    syscall(DataOp::Spawn { cmd })
}

/// 等待子进程退出（返回 Value::U64(exit code)，Own 语义）。
pub fn wait(pid: &Value) -> Action {
    syscall(DataOp::Wait {
        pid: expect_pid(pid),
    })
}

/// 向子进程发送信号（仅 SIGKILL 跨平台可行）。
pub fn kill(pid: &Value, signal: Signal) -> Action {
    syscall(DataOp::Kill {
        pid: expect_pid(pid),
        signal,
    })
}

/// 发送信号（不可逆；非 SIGKILL 由物理层拒绝，需用户补偿挂钩）。
pub fn send_signal(signal: Signal, pid: &Value) -> Action {
    syscall(DataOp::SendSignal {
        signal,
        pid: expect_pid(pid),
    })
}

// 同步

/// 获取互斥锁（动态仲裁；同 id 的 MutexLock 声明 Read(Fd(id))——审计
/// R4-F2：Write 声明会与同值 fd 的 Write 消费冲突且锁循环不可重入）。
pub fn mutex_lock(id: u64) -> Action {
    syscall(DataOp::MutexLock { id })
}

/// 释放互斥锁（Read 声明：不撤销任何消费，锁循环可重入）。
pub fn mutex_unlock(id: u64) -> Action {
    syscall(DataOp::MutexUnlock { id })
}

// 时间

/// 读取墙上时钟毫秒（非确定性，虚拟时钟见 A2 feature）。
pub fn get_time() -> Action {
    syscall(DataOp::GetTime)
}

/// 等待一段时间（Action::Sleep 节点，非 DataOp）。
pub fn sleep(duration: Duration) -> Action {
    Action::Sleep {
        duration,
        next: Box::new(Action::Pure),
    }
}

// ── 错误处理（Action::Catch 的 dx 级便捷包装）─────────────────────────

/// 捕获链内错误：`action` 失败时把 `SysError` 交给 `handler` 生成替代 Action
/// 继续，成功时值原样贯穿（handler 不执行）。
///
/// 语义与手写 `Action::Catch { action, handler }` 完全一致（纯构造包装，
/// 无新节点、运行时零改动）：Catch **仅处理错误值**，不触碰撤销栈
/// （recover 语义仍在 Replace/recover 路径，见 runtime.rs）。
///
/// 用法（do_! 内）：
/// ```rust
/// # use algeff_core::prelude::*;
/// # use algeff_core::OpenFlags;
/// # use algeff_macro::do_;
/// # use algeff_std::dx;
/// let path = std::path::PathBuf::from("missing.txt");
/// let fallback = std::path::PathBuf::from("fallback.txt");
/// let blueprint: Action = do_! {
///     let fd = dx::catch(
///         dx::open(&path, OpenFlags { read: true, ..Default::default() }),
///         move |_e| dx::open(fallback.clone(), OpenFlags {
///             read: true,
///             write: true,
///             create: true,
///             ..Default::default()
///         }),
///     );
///     dx::close(&fd);
///     Value::Unit
/// };
/// assert!(matches!(blueprint, Action::Sequential { .. }));
/// ```
///
/// handler 需 `FnOnce(SysError) -> Action + Send + 'static`（捕获的外部数据
/// 请 clone 后 `move` 进闭包）；替代 Action 可为 `do_!`/`plan!`/`dx::unit()`
/// 等任意返回 `Action` 的表达式。
pub fn catch(action: Action, handler: impl FnOnce(SysError) -> Action + Send + 'static) -> Action {
    Action::Catch {
        action: Box::new(action),
        handler: Box::new(handler),
    }
}
