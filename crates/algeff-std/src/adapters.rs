//! 预包装适配器（A5 交付）：以类型安全方式构造 Action（pdr.md §14 风格）。
//!
//! 每个函数返回一个可直接被 `Runtime::run` 执行的 `Action`，也可作为
//! `Sequential`/`Fork` 的片段组合。资源声明使用类型状态包装
//! `TypedResource::new_read/new_write/new_owned + into_usage`（pdr.md §3）。
//!
//! 说明：TcpBind/TcpConnect/PipeOpen/Spawn 等运行时才分配 fd 的操作，其
//! `ResourceSet` 无法静态声明新句柄，故为空集（新资源不参与冲突检测）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use algeff_core::{
    Action, Bytes, DataOp, Fd, MmapProt, OpenFlags, Pid, PipeFlags, ResourceInner, ResourceUsage,
    Signal, TypedResource,
};

/// 构造一个 next 为 `Pure` 的 Syscall 节点。
fn syscall(op: DataOp, resources: Vec<ResourceUsage>) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(Action::Pure),
    }
}

// ── 资源声明辅助（TypedResource + into_usage）────────────────────────

fn use_read_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<algeff_core::ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn use_write_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<algeff_core::WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn use_own_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<algeff_core::Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}
fn use_read_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<algeff_core::ReadOnly>::new_read(ResourceInner::Path(path)).into_usage()
}
fn use_write_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<algeff_core::WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}
fn use_own_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<algeff_core::Owned>::new_owned(ResourceInner::Path(path)).into_usage()
}
fn use_own_pid(pid: Pid) -> ResourceUsage {
    TypedResource::<algeff_core::Owned>::new_owned(ResourceInner::Pid(pid)).into_usage()
}
fn use_write_pid(pid: Pid) -> ResourceUsage {
    TypedResource::<algeff_core::WriteOnly>::new_write(ResourceInner::Pid(pid)).into_usage()
}
fn use_write_signal() -> ResourceUsage {
    TypedResource::<algeff_core::WriteOnly>::new_write(ResourceInner::Signal).into_usage()
}

// ── 网络 ──────────────────────────────────────────────────────────────

/// 绑定 TCP 监听（返回 Action::Syscall(TcpBind)）。
pub fn open_tcp(addr: SocketAddr) -> Action {
    syscall(DataOp::TcpBind { addr }, vec![])
}

/// 接受一个连接（listener fd 来自 `open_tcp` 的结果）。
pub fn accept(listener: Fd) -> Action {
    syscall(DataOp::TcpAccept { listener }, vec![use_read_fd(listener)])
}

/// 建立 TCP 连接。
pub fn connect(addr: SocketAddr) -> Action {
    syscall(DataOp::TcpConnect { addr }, vec![])
}

// ── 通用 IO ───────────────────────────────────────────────────────────

/// 从 fd 读取至多 len 字节。
pub fn read(fd: Fd, len: usize) -> Action {
    syscall(DataOp::Read { fd, len }, vec![use_read_fd(fd)])
}

/// 向 fd 写入数据。
pub fn write(fd: Fd, data: Bytes) -> Action {
    syscall(DataOp::Write { fd, data }, vec![use_write_fd(fd)])
}

/// 关闭 fd（Own 语义：唯一持有者释放物理资源）。
pub fn close(fd: Fd) -> Action {
    syscall(DataOp::Close { fd }, vec![use_own_fd(fd)])
}

// ── 文件系统 ──────────────────────────────────────────────────────────

/// 打开文件（flags 中 write=true 时声明 Write 模式，否则 Read）。
pub fn open_file(path: PathBuf, flags: OpenFlags) -> Action {
    let usage = if flags.write {
        use_write_path(path.clone())
    } else {
        use_read_path(path.clone())
    };
    syscall(DataOp::Open { path, flags }, vec![usage])
}

/// 创建目录（mode 在 Unix 上生效）。
pub fn create_dir(path: PathBuf, mode: u32) -> Action {
    let usage = use_write_path(path.clone());
    syscall(DataOp::Mkdir { path, mode }, vec![usage])
}

/// 列出目录项（Value::List(Str)）。
pub fn read_dir(path: PathBuf) -> Action {
    let usage = use_read_path(path.clone());
    syscall(DataOp::ReadDir { path }, vec![usage])
}

/// 文件元数据（Value::List([len, is_dir, is_file])）。
pub fn stat(path: PathBuf) -> Action {
    let usage = use_read_path(path.clone());
    syscall(DataOp::Stat { path }, vec![usage])
}

/// 删除文件（不可逆；补偿挂钩由用户提供）。
pub fn unlink(path: PathBuf) -> Action {
    let usage = use_own_path(path.clone());
    syscall(DataOp::Unlink { path }, vec![usage])
}

/// 重命名（可逆：undo 反向 Rename）。
pub fn rename(from: PathBuf, to: PathBuf) -> Action {
    let u = vec![use_write_path(from.clone()), use_write_path(to.clone())];
    syscall(DataOp::Rename { from, to }, u)
}

/// 截断文件（<1MB 时可逆）。
pub fn truncate(path: PathBuf, len: usize) -> Action {
    let usage = use_write_path(path.clone());
    syscall(DataOp::Truncate { path, len }, vec![usage])
}

/// 内存映射文件（返回 Value::Bytes，用户态 COW 语义）。
pub fn mmap(path: PathBuf, len: usize, prot: MmapProt) -> Action {
    let usage = use_read_path(path.clone());
    syscall(DataOp::Mmap { path, len, prot }, vec![usage])
}

// ── 管道 ──────────────────────────────────────────────────────────────

/// 打开内存管道（决策 D5），返回 Value::List([reader_fd, writer_fd])。
pub fn pipe_open() -> Action {
    syscall(
        DataOp::PipeOpen {
            flags: PipeFlags::default(),
        },
        vec![],
    )
}

// ── 进程 ──────────────────────────────────────────────────────────────

/// 派生子进程（返回 Value::Pid）。
pub fn spawn(cmd: std::process::Command) -> Action {
    syscall(DataOp::Spawn { cmd }, vec![])
}

/// 等待子进程退出（返回 Value::U64(exit code)，Own 语义）。
pub fn wait(pid: Pid) -> Action {
    syscall(DataOp::Wait { pid }, vec![use_own_pid(pid)])
}

/// 向子进程发送信号（仅 SIGKILL 跨平台可行）。
pub fn kill(pid: Pid, signal: Signal) -> Action {
    syscall(DataOp::Kill { pid, signal }, vec![use_write_pid(pid)])
}

/// 发送信号（不可逆；非 SIGKILL 由物理层拒绝，需用户补偿挂钩）。
pub fn send_signal(signal: Signal, pid: Pid) -> Action {
    syscall(
        DataOp::SendSignal { signal, pid },
        vec![use_write_signal(), use_write_pid(pid)],
    )
}

// ── 其他 ──────────────────────────────────────────────────────────────

/// 复制 fd（共享同一句柄）。
pub fn dup(fd: Fd) -> Action {
    syscall(DataOp::Dup { fd }, vec![use_write_fd(fd)])
}

/// 读取墙上时钟毫秒（非确定性，虚拟时钟见 A2 feature）。
pub fn get_time() -> Action {
    syscall(DataOp::GetTime, vec![])
}

/// 等待一段时间（Action::Sleep 节点）。
pub fn sleep(duration: Duration) -> Action {
    Action::Sleep {
        duration,
        next: Box::new(Action::Pure),
    }
}
