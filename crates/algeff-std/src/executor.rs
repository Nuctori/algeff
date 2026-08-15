//! Tokio 物理执行器 —— A5 交付（pdr.md §2.2 / §10 / §11 / §12.2）。
//!
//! 实现 `SyscallExecutor`：每个 DataOp 在 tokio 上执行，返回
//! `(Value, Option<UndoOp>)`。可逆操作（撤销策略 Full，pdr.md §11.2）返回逆操作；
//! 不可逆操作（UdpSendTo/Kill/SendSignal 等）返回 `None`（补偿挂钩由用户提供）。
//!
//! ## 撤销闭包约束（CTO 裁决 D15）
//! undo 闭包只捕获物理资源数据（`Arc` 句柄、原内容 `Bytes`、路径、位置），
//! **禁止捕获 registry 引用**（`execute` 只拿到 `&mut registry`，闭包是 `'static`）。
//!
//! ## 句柄存储设计（RFC-05 关联）
//! 冻结的 `ResourceHandle` 全部为 `Arc<T>`，而 tokio 1.52 的
//! `AsyncRead/AsyncWrite/AsyncSeek` 只对 `&mut T` 实现（无 `&File`/`&TcpStream`）。
//! 因此按类型的可克隆性分三档：
//! - **可克隆**（`tokio::fs::File::try_clone`）：executor 侧持有
//!   `Arc<tokio::sync::Mutex<File>>` 工作对象（共享 `&mut` 访问；Dup 真正共享
//!   同一文件描述与游标）；registry 侧放 `try_clone` 出的簿记 token（同一 OS 描述）。
//! - **不可克隆**（`TcpStream`、管道半端、`Child`）：registry 持有真实 `Arc<T>`；
//!   IO 走 `take → Arc::get_mut → 操作 → 重新 allocate`（决策 D1 单调分配使注册表
//!   fd 轮换），executor 以「逻辑 fd → 当前注册表 fd」映射对外隐藏轮换。
//!   Wait/Kill 以 pid 为键，天然隐藏轮换。
//! - **`&self` 型操作**（`TcpListener::accept`、`UdpSocket::recv_from/send_to`、
//!   `TcpStream::shutdown` 之外）：直接经 registry 查找。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use algeff_core::{
    AccessMode, BoxFuture, DataOp, MmapProt, OpenFlags, PipeFlags, Resource, ResourceArbiter,
    ResourceHandle, ResourceRegistry, ResourceUsage, SysError, SyscallExecutor, UndoOp, Value,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::process::Child;

/// Full 撤销策略上限（pdr.md §11.2）：文件小于该值做写前读完整回滚。
const FULL_UNDO_MAX_BYTES: u64 = 1024 * 1024;
/// 跨平台内存管道缓冲区（契约决策 D5）。
const PIPE_BUF_SIZE: usize = 64 * 1024;
/// SIGKILL（tokio `Child::start_kill` 仅支持该信号，跨平台常量）。
const SIGKILL: i32 = 9;
/// 动态仲裁占坑有限重试上限（A7：失败回滚 + 有限重试，绝不阻塞等待）。
const ARBITER_RETRY_LIMIT: usize = 8;
/// 动态仲裁占坑重试退避间隔（固定 1ms，竞争窗口合计约 7ms）。
const ARBITER_RETRY_BACKOFF: Duration = Duration::from_millis(1);

/// io::Error → SysError 转换（RFC-10 / JD-1 / JD-2 修复，A5 域）。
///
/// 缺口 1（RFC-10，Windows）：冻结面 `SysError::from_errno`（error.rs）只识别
/// POSIX errno，而 Windows 上 `io::Error::raw_os_error()` 返回 Win32/WSA 原生码
/// （ERROR_FILE_EXISTS=80、WSAEADDRINUSE=10048 等）——直接透传会退化为
/// `Other(n)`，同一蓝图在 Windows 返回 Other(80)、在 Unix 返回
/// `AlreadyExists`，破坏跨平台错误语义一致性。
///
/// 缺口 2（JD-1，macOS/BSD）：`raw_os_error()` 的 Darwin 码与 Linux 不同
/// （EAGAIN=35/ETIMEDOUT=60/ECONNRESET=54/ECONNREFUSED=61/EADDRINUSE=48/
/// EADDRNOTAVAIL=49；Linux 为 11/110/104/111/98/99），纯透传使 WouldBlock/
/// TimedOut/ConnectionReset/ConnectionRefused 在 macOS 上退化为 Other(n)。
///
/// 修复：统一为 **kind-first**——std 的 `decode_error_kind` 在各平台用平台
/// 自身 errno 常量解码 `ErrorKind`（Linux EAGAIN=11、macOS EAGAIN=35、
/// Windows WSAEWOULDBLOCK=10035 均解码为 WouldBlock），kind → POSIX errno
/// 映射天然跨平台正确且不受码值漂移影响。kind 无法归类时 fallback 到
/// `raw_os_error`：Windows 上 std 已解码的常见 Win32/WSA 码均经 kind 臂命中
/// （含 WSAECONNABORTED=10053 → ConnectionAborted），未命中码兜底为
/// **Other(raw)**（避免撞码错映射，F1 文档化方向）；Unix 上 raw 即 POSIX
/// errno，透传语义与冻结面
/// `From<io::Error>` 一致。原手写 `normalize_windows_errno` 码表删除
/// （JD-3：其全部条目已被 kind 臂覆盖）。
/// （JD-3：其全部条目已被 kind 臂覆盖）。
fn to_sys_err(e: std::io::Error) -> SysError {
    // ErrorKind 优先：语义映射不受平台码值漂移影响（比手写平台码表更稳健）。
    let kind_errno = match e.kind() {
        std::io::ErrorKind::NotFound => Some(2),
        std::io::ErrorKind::PermissionDenied => Some(13),
        std::io::ErrorKind::WouldBlock => Some(11),
        std::io::ErrorKind::Interrupted => Some(4),
        std::io::ErrorKind::TimedOut => Some(110),
        std::io::ErrorKind::ConnectionReset => Some(104),
        // JD-2：Windows WSAECONNABORTED=10053 的 kind=ConnectionAborted
        // （JD-3 核实：唯一真实到达原码表的条目）→ ECONNABORTED。
        std::io::ErrorKind::ConnectionAborted => Some(103),
        std::io::ErrorKind::ConnectionRefused => Some(111),
        std::io::ErrorKind::BrokenPipe => Some(32),
        std::io::ErrorKind::StorageFull => Some(28),
        std::io::ErrorKind::InvalidInput => Some(22),
        std::io::ErrorKind::AlreadyExists => Some(17),
        // JD-2：Windows ERROR_NOT_SAME_DEVICE=17 的 kind=CrossesDevices；
        // 缺臂会落 raw 路径 → from_errno(17)=AlreadyExists（撞码错映射）。
        std::io::ErrorKind::CrossesDevices => Some(18),
        std::io::ErrorKind::NotADirectory => Some(20),
        std::io::ErrorKind::IsADirectory => Some(21),
        // EADDRINUSE/EADDRNOTAVAIL 不在 14 错误集（pdr.md §10.1）→ 映射到
        // POSIX 码后经 from_errno 落为 Other(98)/Other(99)，与 Unix 上
        // bind 冲突的真实 errno 一致（跨平台可移植性目标）。
        std::io::ErrorKind::AddrInUse => Some(98),
        std::io::ErrorKind::AddrNotAvailable => Some(99),
        _ => None,
    };
    if let Some(errno) = kind_errno {
        return SysError::from_errno(errno);
    }
    match e.raw_os_error() {
        // kind 未命中兜底（平台分支）：
        // - Windows：std 未解码的 Win32/WSA 码**不代表 POSIX errno**，直接
        //   Other(raw) 兜底（F1 文档化方向）——避免撞码错映射：
        //   ERROR_SHARING_VIOLATION=32 若经 from_errno 会被误标 BrokenPipe（32=EPIPE）、
        //   ERROR_INVALID_DATA=13 → PermissionDenied、ERROR_TOO_MANY_OPEN_FILES=4 → Interrupted。
        // - Unix：raw 即 POSIX errno，透传与冻结面 From<io::Error> 一致。
        #[cfg(windows)]
        Some(raw) => SysError::Other(raw),
        #[cfg(not(windows))]
        Some(raw) => SysError::from_errno(raw),
        None => SysError::Other(0),
    }
}

/// 持锁 guard 的停车位：undo（recover 路径）与显式 `MutexUnlock` 均可取走释放，幂等。
type HeldLockSlot = Arc<tokio::sync::Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>>;

/// MutexLock 仲裁占坑的 RAII 守卫（R-1 MEDIUM 批 8：claim 取消泄漏修复）。
///
/// 泄漏窗口：`try_claim` 成功（占坑已落入仲裁表）后、undo 建立前存在 await 点
/// （`lock_owned`、slot 停车位锁）。若 future 在这些 await 点被丢弃（如
/// `Action::Timeout` Elapsed 时 runtime.rs 经 `tokio::time::timeout` 直接丢弃
/// 内层 future、不做任何清理），占坑会随 future 的局部状态一并消失但**不**
/// release —— 该 MutexLock id 在本执行器内永久 WouldBlock（状态毒化，非死锁）。
///
/// 守卫在 `drop` 时自动 release 占坑（`release` 幂等：对未占坑资源是 no-op，
/// 双保险安全）；undo/锁持有成功建立后由 `disarm()` 停用，释放职责移交 undo
/// 路径（防双释放）。持有型守卫（clone Arc，非借用）：可跨 await 存活，且与
/// undo 闭包（`'static` + Send，捕获同型 Arc 与 claim_set）并存。
/// A5 批 9：arbiter 改 std Mutex 后，drop 内同步 `lock()` 安全（见 `Drop` 注释）。
struct ArbiterClaimGuard {
    arbiter: Arc<std::sync::Mutex<ResourceArbiter>>,
    claim_set: Vec<ResourceUsage>,
    armed: bool,
}

impl ArbiterClaimGuard {
    fn new(arbiter: Arc<std::sync::Mutex<ResourceArbiter>>, claim_set: Vec<ResourceUsage>) -> Self {
        Self {
            arbiter,
            claim_set,
            armed: true,
        }
    }

    /// 释放职责移交给 undo 路径：此后 drop 不再 release（防双释放）。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ArbiterClaimGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // A5 批 9（Drop 内 async panic 修复）：arbiter 已改 std Mutex，临界区
        // （`try_claim`/`release`）无 await、微秒级 —— `lock()` 只会短暂阻塞等待
        // 持锁者完成临界区。修复前 tokio Mutex 的 `blocking_lock` 兜底在异步
        // worker 线程 poll 帧内调用会 panic（tokio 明确禁止），而取消路径
        // （Timeout 丢弃内层 future）的 drop 恰在该上下文执行 → Drop 内 panic
        // 有 double-panic abort 风险；std Mutex 无此问题。`expect`：临界区内
        // 无 panic 源（纯表操作），锁中毒不可达。
        // release 幂等：对未占坑资源是 no-op（已 disarm / 已显式释放时双保险）。
        self.arbiter
            .lock()
            .expect("arbiter 锁中毒不可达：临界区无 panic 源")
            .release(&self.claim_set);
    }
}

/// 默认物理执行器（pdr.md §12.2）。
#[derive(Debug)]
pub struct TokioExecutor {
    /// 子进程 pid → 注册表 fd（Wait/Kill 经此定位；注册表 fd 随 take/allocate 轮换）。
    children: HashMap<u32, u64>,
    /// 互斥锁 id → 原语（MutexLock/MutexUnlock，executor 内部映射）。
    mutexes: HashMap<u64, Arc<tokio::sync::Mutex<()>>>,
    /// 持锁 guard 停车位（undo 与显式 MutexUnlock 均可取走释放，幂等）。
    held_locks: HashMap<u64, HeldLockSlot>,
    /// 动态资源仲裁（D16 / R-1 落地）：MutexLock id → `Resource::Fd(id)` 占坑表。
    /// Arc 共享：undo 闭包（'static + Send）与显式 MutexUnlock 都需释放占坑；
    /// std Mutex 保护跨任务访问（A5 批 9：try_claim/release 为同步短临界区，
    /// 无 await、无嵌套锁、无循环等待；Drop 内同步 lock 安全）。
    arbiter: Arc<std::sync::Mutex<ResourceArbiter>>,
    /// 文件工作对象（registry 侧为 `try_clone` 簿记 token，共享同一 OS 描述）。
    files: HashMap<u64, Arc<tokio::sync::Mutex<tokio::fs::File>>>,
    /// TCP 流逻辑 fd → 当前注册表 fd。
    stream_fds: HashMap<u64, u64>,
    /// 管道读端逻辑 fd → 当前注册表 fd。
    pipe_reader_fds: HashMap<u64, u64>,
    /// 管道写端逻辑 fd → 当前注册表 fd。
    pipe_writer_fds: HashMap<u64, u64>,
}

impl Default for TokioExecutor {
    fn default() -> Self {
        Self {
            children: HashMap::new(),
            mutexes: HashMap::new(),
            held_locks: HashMap::new(),
            arbiter: Arc::new(std::sync::Mutex::new(ResourceArbiter::new())),
            files: HashMap::new(),
            stream_fds: HashMap::new(),
            pipe_reader_fds: HashMap::new(),
            pipe_writer_fds: HashMap::new(),
        }
    }
}

impl TokioExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 逻辑 fd → 当前注册表 fd（仅对 take/get_mut 轮换型句柄）。
    fn translated_fd(&self, fd: u64) -> Option<u64> {
        self.stream_fds
            .get(&fd)
            .copied()
            .or_else(|| self.pipe_reader_fds.get(&fd).copied())
            .or_else(|| self.pipe_writer_fds.get(&fd).copied())
    }

    /// 轮换型句柄操作后放回：注册表分配新 fd（D1 单调），更新逻辑映射。
    fn put_back(&mut self, fd: u64, handle: ResourceHandle, reg: &mut ResourceRegistry) {
        let new_fd = reg.allocate(handle);
        if let Some(v) = self.stream_fds.get_mut(&fd) {
            *v = new_fd;
        } else if let Some(v) = self.pipe_reader_fds.get_mut(&fd) {
            *v = new_fd;
        } else if let Some(v) = self.pipe_writer_fds.get_mut(&fd) {
            *v = new_fd;
        }
    }

    /// 把 Child 句柄放回注册表（轮换后更新 pid 映射）。
    fn put_child_back(&mut self, pid: u32, arc: Arc<Child>, reg: &mut ResourceRegistry) {
        let fd = reg.allocate(ResourceHandle::Child(arc));
        self.children.insert(pid, fd);
    }

    /// 取走管道读端句柄并做类型转换；类型不符时恢复注册表条目与内部映射
    /// （blocker-3：take 后任何错误路径都不得丢句柄）。
    fn take_pipe_reader(
        &mut self,
        fd: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<Arc<tokio::io::ReadHalf<tokio::io::DuplexStream>>, SysError> {
        let cur = self
            .pipe_reader_fds
            .get(&fd)
            .copied()
            .ok_or(SysError::NotFound)?;
        match reg.take(cur).ok_or(SysError::NotFound)? {
            ResourceHandle::PipeReader(a) => Ok(a),
            h => {
                self.put_back(fd, h, reg);
                Err(SysError::InvalidInput)
            }
        }
    }

    /// 取走管道写端句柄并做类型转换；类型不符时恢复注册表条目与内部映射（blocker-3）。
    fn take_pipe_writer(
        &mut self,
        fd: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<Arc<tokio::io::WriteHalf<tokio::io::DuplexStream>>, SysError> {
        let cur = self
            .pipe_writer_fds
            .get(&fd)
            .copied()
            .ok_or(SysError::NotFound)?;
        match reg.take(cur).ok_or(SysError::NotFound)? {
            ResourceHandle::PipeWriter(a) => Ok(a),
            h => {
                self.put_back(fd, h, reg);
                Err(SysError::InvalidInput)
            }
        }
    }

    /// 取走 TCP 流句柄并做类型转换；类型不符时恢复注册表条目与内部映射（blocker-3）。
    fn take_tcp_stream(
        &mut self,
        fd: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<Arc<TcpStream>, SysError> {
        let cur = self
            .stream_fds
            .get(&fd)
            .copied()
            .ok_or(SysError::NotFound)?;
        match reg.take(cur).ok_or(SysError::NotFound)? {
            ResourceHandle::TcpStream(a) => Ok(a),
            h => {
                self.put_back(fd, h, reg);
                Err(SysError::InvalidInput)
            }
        }
    }

    // ── 文件 ───────────────────────────────────────────────────────────

    async fn op_open(
        &mut self,
        path: &Path,
        flags: &OpenFlags,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let mut o = tokio::fs::OpenOptions::new();
        o.read(flags.read)
            .write(flags.write)
            .append(flags.append)
            .create(flags.create)
            .truncate(flags.truncate)
            .create_new(flags.exclusive);
        let file = o.open(path).await.map_err(to_sys_err)?;
        // registry 簿记 token：try_clone 共享同一 OS 描述（真实工作对象在 executor 侧）。
        let token = file.try_clone().await.map_err(to_sys_err)?;
        let fd = reg.allocate(ResourceHandle::File(Arc::new(token)));
        self.files
            .insert(fd, Arc::new(tokio::sync::Mutex::new(file)));
        // undo=None：物理关闭由 Arc Drop 保证；Fd 表残留清理列入 RFC-05。
        Ok((Value::Fd(fd), None))
    }

    async fn op_read(
        &mut self,
        fd: u64,
        len: usize,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        if let Some(m) = self.files.get(&fd) {
            let mut g = m.lock().await;
            let mut buf = vec![0u8; len];
            let n = g.read(&mut buf).await.map_err(to_sys_err)?;
            buf.truncate(n);
            return Ok((Value::Bytes(buf), None));
        }
        if self.pipe_reader_fds.contains_key(&fd) {
            let mut arc = self.take_pipe_reader(fd, reg)?;
            let mut buf = vec![0u8; len];
            let n = {
                // 被 Dup 共享时无法 &mut → InvalidInput（注释）。
                let rh = match Arc::get_mut(&mut arc) {
                    Some(rh) => rh,
                    None => {
                        // 错误路径：恢复注册表条目与内部映射后再返回（blocker-3）。
                        self.put_back(fd, ResourceHandle::PipeReader(arc), reg);
                        return Err(SysError::InvalidInput);
                    }
                };
                rh.read(&mut buf).await
            };
            // 成功与 I/O 错误均先恢复句柄再传播（blocker-3）。
            self.put_back(fd, ResourceHandle::PipeReader(arc), reg);
            let n = n.map_err(to_sys_err)?;
            buf.truncate(n);
            return Ok((Value::Bytes(buf), None));
        }
        Err(SysError::NotFound)
    }

    async fn op_write(
        &mut self,
        fd: u64,
        data: &[u8],
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // 文件：Full 撤销（<1MB 写前读原内容）或 BestEffort（大文件，undo=None）。
        if let Some(m) = self.files.get(&fd) {
            let mut g = m.lock().await;
            let file = &mut *g;
            let pos = file
                .seek(std::io::SeekFrom::Current(0))
                .await
                .map_err(to_sys_err)?;
            let orig_len = file.metadata().await.map_err(to_sys_err)?.len();
            let undo = if orig_len < FULL_UNDO_MAX_BYTES {
                // 写前读：读取将被覆盖区域（pos..pos+len），完整回滚（Full 策略）。
                // 只写句柄（Windows 上读会 ACCESS_DENIED）→ 降级 BestEffort（undo=None）。
                let mut orig = vec![0u8; data.len()];
                let mut filled = 0usize;
                let mut readable = true;
                while filled < orig.len() {
                    match file.read(&mut orig[filled..]).await {
                        Ok(0) => break,
                        Ok(n) => filled += n,
                        Err(_) => {
                            readable = false;
                            break;
                        }
                    }
                }
                file.seek(std::io::SeekFrom::Start(pos))
                    .await
                    .map_err(to_sys_err)?;
                if readable {
                    orig.truncate(filled);
                    file.write_all(data).await.map_err(to_sys_err)?;
                    // 写后必须 flush（R1 flaky 根因）：tokio::fs::File 的 write_all
                    // 是**异步落盘**——poll_write 把数据拷入内部缓冲后立即返回
                    // Ready(Ok(n))，OS 写经 spawn_mandatory_blocking 在后台完成。
                    // 若 op 返回时 OS 写仍在飞，调用方随后经 std::fs::read 等同步
                    // 观察会读到写前旧内容。对抗套件两处因此 flaky：
                    // rev_undo_restores_file_cursor（Write 后写可见性断言）与
                    // lin_stale_fd_write_after_replace_succeeds（旧 fd 写落盘断言），
                    // 并行负载下 blocking pool 饱和拉宽在飞窗口 → 复现率 6~17%
                    // （实测）；新回归测试 rev_write_effect_immediately_observable_via_sync_read
                    // 放大后 30 跑 13 跑触发。
                    // flush 使 Write 完成 ⇔ OS 已落盘（A4/A6 可观察性契约）。
                    file.flush().await.map_err(to_sys_err)?;
                    // 撤销：恢复原区域 + 截断回写前长度（D15：仅捕获物理数据）。
                    // 审计 R1（对抗测试 rev_undo_restores_file_cursor）：A6 双态
                    // w;w̄ = 1 要求**全部可观察状态**复原——游标（经 Seek(Current)
                    // 可观察）也必须回到写前位置。修复：恢复内容与长度后 seek 回
                    // `pos`（此前游标停留在 pos+orig.len()，破坏撤销双态）。
                    // 注：undo 闭包首步 seek 前无需再排空——tokio AsyncSeekExt 的
                    // seek future 会先 poll_complete 排空在飞操作；此处 flush 已保证
                    // 文件进入 undo 时处于 Idle。
                    let undo_file = m.clone();
                    let undo: UndoOp = Box::pin(async move {
                        let mut g = undo_file.lock().await;
                        if g.seek(std::io::SeekFrom::Start(pos)).await.is_err() {
                            return;
                        }
                        let _ = g.write_all(&orig).await;
                        let _ = g.set_len(orig_len).await;
                        let _ = g.seek(std::io::SeekFrom::Start(pos)).await;
                    });
                    Some(undo)
                } else {
                    file.write_all(data).await.map_err(to_sys_err)?;
                    file.flush().await.map_err(to_sys_err)?;
                    None // 写前读失败（如只写句柄）→ 降级 BestEffort。
                }
            } else {
                file.write_all(data).await.map_err(to_sys_err)?;
                file.flush().await.map_err(to_sys_err)?;
                None // BestEffort（pdr.md §11.2）：大文件（≥1MB）不撤销。
            };
            return Ok((Value::Unit, undo));
        }
        // 管道写端（轮换型；错误路径恢复句柄，blocker-3）。
        if self.pipe_writer_fds.contains_key(&fd) {
            let mut arc = self.take_pipe_writer(fd, reg)?;
            let r = {
                let wh = match Arc::get_mut(&mut arc) {
                    Some(w) => w,
                    None => {
                        self.put_back(fd, ResourceHandle::PipeWriter(arc), reg);
                        return Err(SysError::InvalidInput);
                    }
                };
                wh.write_all(data).await
            };
            self.put_back(fd, ResourceHandle::PipeWriter(arc), reg);
            r.map_err(to_sys_err)?;
            return Ok((Value::Unit, None));
        }
        // TCP 流（Write 对流的复用，轮换型；错误路径恢复句柄，blocker-3）。
        if self.stream_fds.contains_key(&fd) {
            let mut arc = self.take_tcp_stream(fd, reg)?;
            let r = {
                let s = match Arc::get_mut(&mut arc) {
                    Some(s) => s,
                    None => {
                        self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
                        return Err(SysError::InvalidInput);
                    }
                };
                s.write_all(data).await
            };
            self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
            r.map_err(to_sys_err)?;
            return Ok((Value::Unit, None));
        }
        Err(SysError::NotFound)
    }

    async fn op_seek(
        &mut self,
        fd: u64,
        offset: i64,
        whence: &std::io::SeekFrom,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let m = self.files.get(&fd).ok_or(SysError::NotFound)?;
        let mut g = m.lock().await;
        // DataOp 冗余双字段：whence 决定基准，offset 为位移（以 offset 为准）。
        let sf = match whence {
            std::io::SeekFrom::Start(_) => std::io::SeekFrom::Start(offset as u64),
            std::io::SeekFrom::Current(_) => std::io::SeekFrom::Current(offset),
            std::io::SeekFrom::End(_) => std::io::SeekFrom::End(offset),
        };
        let pos = g.seek(sf).await.map_err(to_sys_err)?;
        Ok((Value::U64(pos), None))
    }

    async fn op_stat(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        let meta = tokio::fs::metadata(path).await.map_err(to_sys_err)?;
        // 自设计格式：List([len, is_dir, is_file])。
        Ok((
            Value::List(vec![
                Value::U64(meta.len()),
                Value::Bool(meta.is_dir()),
                Value::Bool(meta.is_file()),
            ]),
            None,
        ))
    }

    #[cfg(unix)]
    async fn op_chmod(
        &mut self,
        path: &Path,
        mode: u32,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .await
            .map_err(to_sys_err)?;
        // undo=None：恢复需原权限快照；补偿挂钩由用户提供（RFC-05）。
        Ok((Value::Unit, None))
    }

    #[cfg(not(unix))]
    async fn op_chmod(
        &mut self,
        _path: &Path,
        _mode: u32,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // 非 Unix 平台无 chmod 语义 → ENOSYS。
        Err(SysError::Other(38))
    }

    #[cfg(unix)]
    async fn op_chown(
        &mut self,
        path: &Path,
        uid: u32,
        gid: u32,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // 同步系统调用（快速路径，无阻塞风险）。JD-5：补接统一入口 `to_sys_err`
        // （Linux 行为不变：raw 即 POSIX errno；macOS Darwin 码经 kind-first 归一）。
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(to_sys_err)?;
        // undo=None：恢复需原 uid/gid 快照；补偿挂钩由用户提供（RFC-05）。
        Ok((Value::Unit, None))
    }

    #[cfg(not(unix))]
    async fn op_chown(
        &mut self,
        _path: &Path,
        _uid: u32,
        _gid: u32,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // 非 Unix 平台无 chown 语义 → ENOSYS。
        Err(SysError::Other(38))
    }

    async fn op_truncate(
        &mut self,
        path: &Path,
        len: usize,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let meta = tokio::fs::metadata(path).await.map_err(to_sys_err)?;
        let undo = if meta.len() < FULL_UNDO_MAX_BYTES {
            let orig = tokio::fs::read(path).await.map_err(to_sys_err)?; // 写前读（Full 策略）。
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await
                .map_err(to_sys_err)?
                .set_len(len as u64)
                .await
                .map_err(to_sys_err)?;
            let p = path.to_path_buf();
            let undo: UndoOp = Box::pin(async move {
                // 恢复原内容与原长度（路径级撤销，仅捕获物理数据）。
                if let Ok(mut f) = tokio::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&p)
                    .await
                {
                    let _ = f.write_all(&orig).await;
                    let _ = f.set_len(orig.len() as u64).await;
                }
            });
            Some(undo)
        } else {
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await
                .map_err(to_sys_err)?
                .set_len(len as u64)
                .await
                .map_err(to_sys_err)?;
            None // BestEffort（pdr.md §11.2）：大文件（≥1MB）不撤销。
        };
        Ok((Value::Unit, undo))
    }

    async fn op_unlink(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        tokio::fs::remove_file(path).await.map_err(to_sys_err)?;
        // undo=None：恢复需缓存原内容+元数据（BestEffort/Skip）；补偿挂钩由用户提供（RFC-05）。
        Ok((Value::Unit, None))
    }

    async fn op_rename(
        &mut self,
        from: &Path,
        to: &Path,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        tokio::fs::rename(from, to).await.map_err(to_sys_err)?;
        let (f, t) = (from.to_path_buf(), to.to_path_buf());
        // 逆操作：反向 Rename（Full 策略，路径级撤销）。
        let undo: UndoOp = Box::pin(async move {
            let _ = tokio::fs::rename(&t, &f).await;
        });
        Ok((Value::Unit, Some(undo)))
    }

    async fn op_mkdir(
        &mut self,
        path: &Path,
        mode: u32,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        tokio::fs::create_dir(path).await.map_err(to_sys_err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await;
        }
        #[cfg(not(unix))]
        let _ = mode;
        let p = path.to_path_buf();
        // 尽力撤销：仅空目录可删（非空 → remove_dir 失败静默，BestEffort）。
        let undo: UndoOp = Box::pin(async move {
            let _ = tokio::fs::remove_dir(&p).await;
        });
        Ok((Value::Unit, Some(undo)))
    }

    async fn op_rmdir(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        tokio::fs::remove_dir(path).await.map_err(to_sys_err)?;
        // undo=None：恢复目录内容不可行（BestEffort/Skip）；补偿挂钩由用户提供（RFC-05）。
        Ok((Value::Unit, None))
    }

    async fn op_read_dir(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        let mut rd = tokio::fs::read_dir(path).await.map_err(to_sys_err)?;
        let mut names = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(to_sys_err)? {
            names.push(Value::Str(entry.file_name().to_string_lossy().into_owned()));
        }
        Ok((Value::List(names), None))
    }

    // ── 网络 TCP ───────────────────────────────────────────────────────

    async fn op_tcp_bind(
        &mut self,
        addr: &std::net::SocketAddr,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let listener = TcpListener::bind(addr).await.map_err(to_sys_err)?;
        let fd = reg.allocate(ResourceHandle::TcpListener(Arc::new(listener)));
        // undo=None：关闭由 Arc Drop 保证；Fd 表残留清理列入 RFC-05。
        Ok((Value::Fd(fd), None))
    }

    async fn op_tcp_accept(
        &mut self,
        listener: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let handle = reg.lookup(listener).ok_or(SysError::NotFound)?;
        let (stream, peer) = match handle {
            ResourceHandle::TcpListener(l) => l.accept().await.map_err(to_sys_err)?,
            _ => return Err(SysError::InvalidInput),
        };
        let fd = reg.allocate(ResourceHandle::TcpStream(Arc::new(stream)));
        self.stream_fds.insert(fd, fd);
        Ok((Value::List(vec![Value::Fd(fd), Value::Addr(peer)]), None))
    }

    async fn op_tcp_connect(
        &mut self,
        addr: &std::net::SocketAddr,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let stream = TcpStream::connect(addr).await.map_err(to_sys_err)?;
        let fd = reg.allocate(ResourceHandle::TcpStream(Arc::new(stream)));
        self.stream_fds.insert(fd, fd);
        Ok((Value::Fd(fd), None))
    }

    async fn op_tcp_read(
        &mut self,
        fd: u64,
        len: usize,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let mut arc = self.take_tcp_stream(fd, reg)?;
        let mut buf = vec![0u8; len];
        let n = {
            // 被 Dup 共享时无法 &mut → InvalidInput；错误路径恢复句柄（blocker-3）。
            let s = match Arc::get_mut(&mut arc) {
                Some(s) => s,
                None => {
                    self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
                    return Err(SysError::InvalidInput);
                }
            };
            s.read(&mut buf).await
        };
        self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
        let n = n.map_err(to_sys_err)?;
        buf.truncate(n);
        Ok((Value::Bytes(buf), None))
    }

    async fn op_tcp_write(
        &mut self,
        fd: u64,
        data: &[u8],
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let mut arc = self.take_tcp_stream(fd, reg)?;
        let r = {
            // 被 Dup 共享时无法 &mut → InvalidInput；错误路径恢复句柄（blocker-3）。
            let s = match Arc::get_mut(&mut arc) {
                Some(s) => s,
                None => {
                    self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
                    return Err(SysError::InvalidInput);
                }
            };
            s.write_all(data).await
        };
        self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
        r.map_err(to_sys_err)?;
        Ok((Value::Unit, None))
    }

    async fn op_tcp_shutdown(
        &mut self,
        fd: u64,
        how: &std::net::Shutdown,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let arc = self.take_tcp_stream(fd, reg)?;
        // tokio 未公开 std::net::Shutdown 的 Read/Both 语义（shutdown_std 为
        // pub(super)），经 std 层往返实现完整 how 语义（被 Dup 共享时无法 → InvalidInput）。
        let stream = match Arc::try_unwrap(arc) {
            Ok(s) => s,
            Err(arc) => {
                // 被 Dup 共享（仍有强引用）：恢复注册表条目与内部映射（blocker-3）。
                self.put_back(fd, ResourceHandle::TcpStream(arc), reg);
                return Err(SysError::InvalidInput);
            }
        };
        // std 往返的每一步失败都尽量恢复句柄（blocker-3）：set_nonblocking/shutdown
        // 失败时 std 句柄仍在手，可重新包装回 tokio 放回注册表。
        let std_stream = match stream.into_std() {
            Ok(s) => s,
            Err(e) => return Err(to_sys_err(e)), // 底层句柄失效，无法恢复（深边缘）。
        };
        if let Err(e) = std_stream.set_nonblocking(true) {
            let err = to_sys_err(e);
            return match tokio::net::TcpStream::from_std(std_stream) {
                Ok(s) => {
                    self.put_back(fd, ResourceHandle::TcpStream(Arc::new(s)), reg);
                    Err(err)
                }
                Err(_) => Err(err), // 恢复失败（深边缘）：std 句柄被 from_std 消费。
            };
        }
        if let Err(e) = std_stream.shutdown(*how) {
            let err = to_sys_err(e);
            return match tokio::net::TcpStream::from_std(std_stream) {
                Ok(s) => {
                    self.put_back(fd, ResourceHandle::TcpStream(Arc::new(s)), reg);
                    Err(err)
                }
                Err(_) => Err(err),
            };
        }
        let stream = match tokio::net::TcpStream::from_std(std_stream) {
            Ok(s) => s,
            Err(e) => return Err(to_sys_err(e)), // std 句柄被 from_std 消费，无法恢复。
        };
        self.put_back(fd, ResourceHandle::TcpStream(Arc::new(stream)), reg);
        Ok((Value::Unit, None))
    }

    // ── 网络 UDP ───────────────────────────────────────────────────────

    async fn op_udp_bind(
        &mut self,
        addr: &std::net::SocketAddr,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let sock = UdpSocket::bind(addr).await.map_err(to_sys_err)?;
        let fd = reg.allocate(ResourceHandle::UdpSocket(Arc::new(sock)));
        Ok((Value::Fd(fd), None))
    }

    async fn op_udp_recv_from(
        &mut self,
        fd: u64,
        len: usize,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let sock = match reg.lookup(fd).ok_or(SysError::NotFound)? {
            ResourceHandle::UdpSocket(s) => s,
            _ => return Err(SysError::InvalidInput),
        };
        let mut buf = vec![0u8; len];
        let (n, addr) = sock.recv_from(&mut buf).await.map_err(to_sys_err)?;
        buf.truncate(n);
        Ok((
            Value::List(vec![Value::Bytes(buf), Value::Addr(addr)]),
            None,
        ))
    }

    async fn op_udp_send_to(
        &mut self,
        fd: u64,
        data: &[u8],
        addr: &std::net::SocketAddr,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let sock = match reg.lookup(fd).ok_or(SysError::NotFound)? {
            ResourceHandle::UdpSocket(s) => s,
            _ => return Err(SysError::InvalidInput),
        };
        let _ = sock.send_to(data, addr).await.map_err(to_sys_err)?;
        // undo=None：UDP 发送不可逆（pdr.md §11.1）；补偿挂钩由用户提供。
        Ok((Value::Unit, None))
    }

    // ── 管道（决策 D5：tokio duplex 跨平台内存管道）───────────────────

    async fn op_pipe_open(
        &mut self,
        flags: &PipeFlags,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let _ = flags; // tokio duplex 天然非阻塞；nonblocking 标志忽略。
                       // duplex(n) 返回相连的一对：A.read 与 B.write 共享同一缓冲区。
                       // 读端取 A 的 ReadHalf，写端取 B 的 WriteHalf → 相连管道。
        let (a, b) = tokio::io::duplex(PIPE_BUF_SIZE);
        let (ra, _wa) = tokio::io::split(a);
        let (_rb, wb) = tokio::io::split(b);
        let rfd = reg.allocate(ResourceHandle::PipeReader(Arc::new(ra)));
        let wfd = reg.allocate(ResourceHandle::PipeWriter(Arc::new(wb)));
        self.pipe_reader_fds.insert(rfd, rfd);
        self.pipe_writer_fds.insert(wfd, wfd);
        Ok((Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)]), None))
    }

    // ── 进程 ───────────────────────────────────────────────────────────

    async fn op_spawn(
        &mut self,
        cmd: &std::process::Command,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // std::process::Command 不可 Clone（&DataOp 只读借用），经 getter 重建
        // tokio Command（program/args/cwd/envs/stdio 保留；uid/gid 等不迁移）。
        let mut tc = tokio::process::Command::new(cmd.get_program());
        tc.args(cmd.get_args());
        if let Some(dir) = cmd.get_current_dir() {
            tc.current_dir(dir);
        }
        for (k, v) in cmd.get_envs() {
            match v {
                Some(val) => {
                    tc.env(k, val);
                }
                None => {
                    tc.env_remove(k);
                }
            }
        }
        // std::process::Command 的 stdio 配置 getter 尚未稳定，未迁移（注释）。
        let child = tc.spawn().map_err(to_sys_err)?;
        let pid = child.id().ok_or(SysError::InvalidInput)?;
        let fd = reg.allocate(ResourceHandle::Child(Arc::new(child)));
        self.children.insert(pid, fd);
        Ok((Value::Pid(pid), None))
    }

    async fn op_wait(
        &mut self,
        pid: u32,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let fd = self.children.get(&pid).copied().ok_or(SysError::NotFound)?;
        let mut arc = match reg.take(fd).ok_or(SysError::NotFound)? {
            ResourceHandle::Child(a) => a,
            h => {
                // 类型不符：恢复注册表条目与 pid 映射（blocker-3）。
                let new_fd = reg.allocate(h);
                self.children.insert(pid, new_fd);
                return Err(SysError::InvalidInput);
            }
        };
        let status = {
            // 被 Dup 共享时无法 &mut → InvalidInput（注释）；错误路径恢复句柄（blocker-3）。
            let child = match Arc::get_mut(&mut arc) {
                Some(c) => c,
                None => {
                    self.put_child_back(pid, arc, reg);
                    return Err(SysError::InvalidInput);
                }
            };
            child.wait().await
        };
        // wait 失败（如 ECHILD）：句柄恢复，后续 Close/Kill 仍可寻址（blocker-3）。
        let status = match status {
            Ok(s) => s,
            Err(e) => {
                self.put_child_back(pid, arc, reg);
                return Err(to_sys_err(e));
            }
        };
        self.children.remove(&pid);
        // 信号终止时无退出码 → 1（Unix 惯例 128+signal 留作注释）。
        let code = status.code().unwrap_or(1) as u64;
        Ok((Value::U64(code), None))
    }

    async fn op_kill(
        &mut self,
        pid: u32,
        signal: i32,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let fd = self.children.get(&pid).copied().ok_or(SysError::NotFound)?;
        let mut arc = match reg.take(fd).ok_or(SysError::NotFound)? {
            ResourceHandle::Child(a) => a,
            h => {
                // 类型不符：恢复注册表条目与 pid 映射（blocker-3，与 op_wait 一致）。
                let new_fd = reg.allocate(h);
                self.children.insert(pid, new_fd);
                return Err(SysError::InvalidInput);
            }
        };
        let res = {
            let child = match Arc::get_mut(&mut arc) {
                Some(c) => c,
                None => {
                    self.put_child_back(pid, arc, reg);
                    return Err(SysError::InvalidInput);
                }
            };
            if signal != SIGKILL {
                // tokio `Child::start_kill` 仅支持 SIGKILL（跨平台）；其他信号 → ENOSYS。
                Err(SysError::Other(38))
            } else {
                child.start_kill().map_err(to_sys_err)
            }
        };
        self.put_child_back(pid, arc, reg);
        res?;
        // undo=None：kill 不可逆（pdr.md §11.1）；补偿挂钩由用户提供。
        Ok((Value::Unit, None))
    }

    async fn op_send_signal(
        &mut self,
        signal: i32,
        pid: u32,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        if signal == SIGKILL && self.children.contains_key(&pid) {
            // 自有子进程：走 start_kill（等价 SIGKILL）。
            return self.op_kill(pid, signal, reg).await;
        }
        // 非 SIGKILL 或外部 pid：无 libc/nix 依赖无法向任意 pid 发信号（跨平台）；
        // Err(Other(38)) + 补偿挂钩由用户提供（注释）。
        Err(SysError::Other(38))
    }

    // ── 内存映射 ───────────────────────────────────────────────────────

    async fn op_mmap(
        &mut self,
        path: &Path,
        len: usize,
        prot: &MmapProt,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let _ = prot; // prot 忽略：读入内存即完成映射语义（用户态 COW）。
        let mut bytes = tokio::fs::read(path).await.map_err(to_sys_err)?;
        // 按 len 截断（medium-7）：映射长度语义；文件短于 len 时返回实际长度。
        if bytes.len() > len {
            bytes.truncate(len);
        }
        // undo=None：内存 COW 语义，撤销由上层（A2 用户态 COW 层）负责。
        Ok((Value::Bytes(bytes), None))
    }

    async fn op_munmap(
        &mut self,
        addr: usize,
        len: usize,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        let _ = (addr, len); // 无真实映射（Mmap 返回 Bytes）；no-op。
        Ok((Value::Unit, None))
    }

    // ── 时间 ───────────────────────────────────────────────────────────

    async fn op_get_time(&mut self) -> Result<(Value, Option<UndoOp>), SysError> {
        // 非确定性：墙上时钟毫秒（SystemTime）。确定性方案（VirtualClock）
        // 由 A2 virtual-clock feature 提供（RFC）。
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Ok((Value::U64(ms), None))
    }

    // ── 同步 ───────────────────────────────────────────────────────────

    async fn op_mutex_lock(&mut self, id: u64) -> Result<(Value, Option<UndoOp>), SysError> {
        let m = self
            .mutexes
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        // 动态仲裁（D16 / R-1 落地）：MutexLock id 映射为 `Resource::Fd(id)` 独占占坑。
        // 模式选 Write（互斥语义；Write 与 Own 在仲裁器同为独占，Write 表达
        // 「共享互斥锁」而非「终结所有权」，见 spec/resource-notes.md §8）。
        // 失败 → 有限重试（A7：失败回滚 + 有限重试，绝不阻塞等待，无循环等待链）
        // → 超限返回 WouldBlock（调用方整体回滚）。
        let claim_set = vec![ResourceUsage {
            resource: Resource::Fd(id),
            mode: AccessMode::Write,
        }];
        let arbiter = self.arbiter.clone();
        let mut claimed = false;
        for attempt in 0..ARBITER_RETRY_LIMIT {
            // std Mutex 同步 lock（A5 批 9）：临界区（try_claim）无 await、微秒级，
            // 短持有不会长时间阻塞其他任务；`unwrap`：临界区无 panic 源，中毒不可达。
            if arbiter.lock().unwrap().try_claim(&claim_set) {
                claimed = true;
                break;
            }
            if attempt + 1 < ARBITER_RETRY_LIMIT {
                tokio::time::sleep(ARBITER_RETRY_BACKOFF).await;
            }
        }
        if !claimed {
            return Err(SysError::WouldBlock);
        }
        // 占坑成功 → RAII 守卫接管释放职责（R-1 MEDIUM 批 8）：undo 建立前
        // （`lock_owned`、slot 停车位锁两个 await 点）若 future 被丢弃
        // （Timeout Elapsed 取消路径），守卫 drop 自动 release 占坑，杜绝
        // 「claim 取消泄漏 → 永久 WouldBlock」状态毒化。
        let mut claim_guard = ArbiterClaimGuard::new(arbiter.clone(), claim_set.clone());
        // 本仲裁域内无竞争者持有物理锁（不变量：占坑 ⟺ 持锁，见 resource-notes
        // §2），lock_owned 几乎立即获得；仍保留物理锁保证跨仲裁域（如独立
        // 执行器实例）互斥。
        let guard = m.lock_owned().await;
        // 停车位：undo（recover 路径）与显式 MutexUnlock 均可取走释放（幂等）。
        let slot = self
            .held_locks
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone();
        *slot.lock().await = Some(guard);
        // 锁持有已落位（停车位，park 为最后一个 await 点）→ 释放职责移交 undo
        // 路径（undo/显式 MutexUnlock 经 slot 取走释放）；disarm 防双释放。
        claim_guard.disarm();
        let undo_slot = slot.clone();
        let undo: UndoOp = Box::pin(async move {
            // undo 顺序：先释放物理锁、再释放 arbiter 占坑。释放窗口内占坑仍持有，
            // 新占坑者会失败重试，不会出现「新锁已持有期间被旧 undo 释放占坑」的窗口
            // （保持 占坑 ⟺ 持锁 不变量）。
            if let Some(g) = undo_slot.lock().await.take() {
                drop(g);
            }
            // 幂等：release 对未占坑资源是 no-op（显式 MutexUnlock 已释放时安全）。
            // std Mutex 同步 lock（临界区无 await，A5 批 9）。
            arbiter.lock().unwrap().release(&claim_set);
        });
        Ok((Value::Unit, Some(undo)))
    }

    async fn op_mutex_unlock(&mut self, id: u64) -> Result<(Value, Option<UndoOp>), SysError> {
        // 显式解锁：取走停车位中的 guard 并释放（若 undo 已释放则为 no-op，幂等）。
        if let Some(slot) = self.held_locks.get(&id) {
            if let Some(g) = slot.lock().await.take() {
                drop(g);
            }
        }
        // 同步释放 arbiter 占坑（顺序与 undo 一致：先物理锁、后占坑）。
        // 幂等：`release` 对未占坑资源是 no-op——第二次 unlock（或 undo 已释放）
        // 不会重复释放，无需额外标志位（注释：核心契约 `release` 幂等性保证）。
        let claim_set = vec![ResourceUsage {
            resource: Resource::Fd(id),
            mode: AccessMode::Write,
        }];
        self.arbiter.lock().unwrap().release(&claim_set);
        Ok((Value::Unit, None))
    }

    // ── 其他 ───────────────────────────────────────────────────────────

    async fn op_send_file(
        &mut self,
        out: u64,
        input: u64,
        offset: usize,
        len: usize,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        if out == input {
            // 同 fd 读写 → 自拷贝（无意义）；拒绝（InvalidInput）。
            return Err(SysError::InvalidInput);
        }
        // 输入侧：文件（seek + read）。
        let in_file = self.files.get(&input).ok_or(SysError::NotFound)?;
        let buf = {
            let mut g = in_file.lock().await;
            g.seek(std::io::SeekFrom::Start(offset as u64))
                .await
                .map_err(to_sys_err)?;
            let mut buf = vec![0u8; len];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = g.read(&mut buf[filled..]).await.map_err(to_sys_err)?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            buf.truncate(filled);
            buf
        };
        // 输出侧：文件 / TCP 流 / 管道写端。
        let written = if let Some(m) = self.files.get(&out) {
            let mut g = m.lock().await;
            let n = g.write(&buf).await.map_err(to_sys_err)?;
            // 写后必须 flush（D-039 对齐；R3c MEDIUM-1）：op_send_file 输出到
            // 文件与 op_write 同属**异步落盘**面——tokio::fs::File 的 write
            // 返回时 OS 写可能仍在飞（blocking 池后台完成），op 返回后立即
            // 同步读会观察到旧内容（adversarial_r3c.rs
            // r2_sendfile_file_target_visibility 修复前 64 轮实测
            // stale_immediate>0，与 R1 flaky 同根因）。flush 使 SendFile
            // 完成 ⇔ OS 已落盘（A4/A6 可观察性契约，同 D-039）。
            // 管道/TCP 输出无需 flush：其 write 是对端缓冲/socket 缓冲的
            // 投递语义，无“落盘”可观察面（同 op_write 管道路径不 flush
            // 的约定）。
            g.flush().await.map_err(to_sys_err)?;
            n
        } else if self.stream_fds.contains_key(&out) {
            let mut arc = self.take_tcp_stream(out, reg)?;
            let n = {
                // 错误路径恢复句柄（blocker-3）。
                let s = match Arc::get_mut(&mut arc) {
                    Some(s) => s,
                    None => {
                        self.put_back(out, ResourceHandle::TcpStream(arc), reg);
                        return Err(SysError::InvalidInput);
                    }
                };
                s.write(&buf).await
            };
            self.put_back(out, ResourceHandle::TcpStream(arc), reg);
            n.map_err(to_sys_err)?
        } else if self.pipe_writer_fds.contains_key(&out) {
            let mut arc = self.take_pipe_writer(out, reg)?;
            let n = {
                let w = match Arc::get_mut(&mut arc) {
                    Some(w) => w,
                    None => {
                        self.put_back(out, ResourceHandle::PipeWriter(arc), reg);
                        return Err(SysError::InvalidInput);
                    }
                };
                w.write(&buf).await
            };
            self.put_back(out, ResourceHandle::PipeWriter(arc), reg);
            n.map_err(to_sys_err)?
        } else {
            return Err(SysError::NotFound);
        };
        // undo=None：输出侧写入的撤销由调用方按需补偿（BestEffort，注释）。
        Ok((Value::U64(written as u64), None))
    }

    async fn op_dup(
        &mut self,
        fd: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // 轮换型句柄先取当前注册表 fd（直存句柄则原样）。
        let cur = self.translated_fd(fd).unwrap_or(fd);
        let handle = reg.lookup(cur).ok_or(SysError::NotFound)?.clone();
        let new_fd = reg.allocate(handle);
        // 共享工作对象：File 共享同一 OS 描述与游标；TcpStream/管道共享同一 Arc
        // （共享后 &mut 访问受限 → InvalidInput，注释见 RFC-05）。
        if let Some(m) = self.files.get(&fd) {
            self.files.insert(new_fd, m.clone());
        }
        if self.stream_fds.contains_key(&fd) {
            self.stream_fds.insert(new_fd, new_fd);
        }
        if self.pipe_reader_fds.contains_key(&fd) {
            self.pipe_reader_fds.insert(new_fd, new_fd);
        }
        if self.pipe_writer_fds.contains_key(&fd) {
            self.pipe_writer_fds.insert(new_fd, new_fd);
        }
        Ok((Value::Fd(new_fd), None))
    }

    async fn op_dup2(
        &mut self,
        old_fd: u64,
        new_fd: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        // 先释放 new_fd 占用（轮换型经逻辑映射移除，直存型直接 take）。
        if let Some(cur) = self.stream_fds.remove(&new_fd) {
            reg.remove(cur);
        } else if let Some(cur) = self.pipe_reader_fds.remove(&new_fd) {
            reg.remove(cur);
        } else if let Some(cur) = self.pipe_writer_fds.remove(&new_fd) {
            reg.remove(cur);
        } else {
            let _ = reg.take(new_fd);
        }
        self.files.remove(&new_fd);
        // 决策 D1（fd 全局单调、永不复用）使 Dup2 无法精确落到 new_fd：
        // 语义退化为「先关 new_fd，再复制 old_fd 到新 fd」（注释）。
        let (v, undo) = self.op_dup(old_fd, reg).await?;
        Ok((v, undo))
    }

    async fn op_close(
        &mut self,
        fd: u64,
        reg: &mut ResourceRegistry,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        if let Some(cur) = self.stream_fds.remove(&fd) {
            reg.remove(cur);
        } else if let Some(cur) = self.pipe_reader_fds.remove(&fd) {
            reg.remove(cur);
        } else if let Some(cur) = self.pipe_writer_fds.remove(&fd) {
            reg.remove(cur);
        } else if self.files.remove(&fd).is_some() {
            reg.remove(fd);
        } else if reg.take(fd).is_some() {
            // 直存句柄（TcpListener/UdpSocket/Child token）。
        } else {
            return Err(SysError::NotFound);
        }
        // undo=None：关闭不可逆；Fd 表残留清理列入 RFC-05。
        Ok((Value::Unit, None))
    }
}

impl SyscallExecutor for TokioExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async move {
            match op {
                DataOp::Open { path, flags } => self.op_open(path, flags, registry).await,
                DataOp::Read { fd, len } => self.op_read(*fd, *len, registry).await,
                DataOp::Write { fd, data } => self.op_write(*fd, data, registry).await,
                DataOp::Close { fd } => self.op_close(*fd, registry).await,
                DataOp::Seek { fd, offset, whence } => self.op_seek(*fd, *offset, whence).await,
                DataOp::Stat { path } => self.op_stat(path).await,
                DataOp::Chmod { path, mode } => self.op_chmod(path, *mode).await,
                DataOp::Chown { path, uid, gid } => self.op_chown(path, *uid, *gid).await,
                DataOp::Truncate { path, len } => self.op_truncate(path, *len).await,
                DataOp::Unlink { path } => self.op_unlink(path).await,
                DataOp::Rename { from, to } => self.op_rename(from, to).await,
                DataOp::Mkdir { path, mode } => self.op_mkdir(path, *mode).await,
                DataOp::Rmdir { path } => self.op_rmdir(path).await,
                DataOp::ReadDir { path } => self.op_read_dir(path).await,
                DataOp::TcpBind { addr } => self.op_tcp_bind(addr, registry).await,
                DataOp::TcpAccept { listener } => self.op_tcp_accept(*listener, registry).await,
                DataOp::TcpConnect { addr } => self.op_tcp_connect(addr, registry).await,
                DataOp::TcpRead { fd, len } => self.op_tcp_read(*fd, *len, registry).await,
                DataOp::TcpWrite { fd, data } => self.op_tcp_write(*fd, data, registry).await,
                DataOp::TcpShutdown { fd, how } => self.op_tcp_shutdown(*fd, how, registry).await,
                DataOp::UdpBind { addr } => self.op_udp_bind(addr, registry).await,
                DataOp::UdpRecvFrom { fd, len } => self.op_udp_recv_from(*fd, *len, registry).await,
                DataOp::UdpSendTo { fd, data, addr } => {
                    self.op_udp_send_to(*fd, data, addr, registry).await
                }
                DataOp::PipeOpen { flags } => self.op_pipe_open(flags, registry).await,
                DataOp::Spawn { cmd } => self.op_spawn(cmd, registry).await,
                DataOp::Kill { pid, signal } => self.op_kill(*pid, *signal, registry).await,
                DataOp::Wait { pid } => self.op_wait(*pid, registry).await,
                DataOp::SendSignal { signal, pid } => {
                    self.op_send_signal(*signal, *pid, registry).await
                }
                DataOp::Mmap { path, len, prot } => self.op_mmap(path, *len, prot).await,
                DataOp::Munmap { addr, len } => self.op_munmap(*addr, *len).await,
                DataOp::GetTime => self.op_get_time().await,
                DataOp::MutexLock { id } => self.op_mutex_lock(*id).await,
                DataOp::MutexUnlock { id } => self.op_mutex_unlock(*id).await,
                DataOp::SendFile {
                    out,
                    input,
                    offset,
                    len,
                } => {
                    self.op_send_file(*out, *input, *offset, *len, registry)
                        .await
                }
                DataOp::Dup { fd } => self.op_dup(*fd, registry).await,
                DataOp::Dup2 { old_fd, new_fd } => self.op_dup2(*old_fd, *new_fd, registry).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R-1 MEDIUM 批 8 的确定性复现：`try_claim` 成功后、undo 建立前的 await 点
    /// （`lock_owned`）future 被丢弃（等同 `Action::Timeout` Elapsed 丢弃内层
    /// future）→ `ArbiterClaimGuard` drop 必须自动 release 占坑，否则该 id 永久
    /// WouldBlock（状态毒化）。
    ///
    /// 构造：先占物理锁但不占仲裁坑（直接经内部 `mutexes` 表锁住，模拟极端
    /// 取消时序下「try_claim 成功 → lock_owned 阻塞」的窗口），启动
    /// `op_mutex_lock`，等占坑落入仲裁表后 abort 任务（未来在 `lock_owned`
    /// await 处被丢弃）。修复前：占坑泄漏（仲裁表残留）；修复后：守卫 drop
    /// 自动 release。
    #[tokio::test]
    async fn mutex_claim_guard_releases_on_cancel() {
        let mut ex = TokioExecutor::new();
        let id = 999u64;
        // 物理锁被占用且仲裁表空闲：try_claim 成功（无占坑冲突）后 `lock_owned`
        // 必然阻塞，形成 MEDIUM 描述的泄漏窗口。
        let phys = ex
            .mutexes
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _phys_held = phys.lock_owned().await;
        // 观察句柄：`op_mutex_lock` 阻塞期间持有执行器锁，但仲裁锁在 try_claim
        // 成功后即释放 —— 经独立 Arc 观察占坑状态，避免与执行器锁互斥死锁。
        let arb_obs = ex.arbiter.clone();
        let ex = Arc::new(tokio::sync::Mutex::new(ex));
        let task_ex = ex.clone();
        let handle = tokio::spawn(async move {
            let mut g = task_ex.lock().await;
            g.op_mutex_lock(id).await
        });
        // 等 try_claim 生效（占坑落入仲裁表）→ 此刻 future 必阻塞在 lock_owned。
        let resource = Resource::Fd(id);
        let wait_claim = async {
            for _ in 0..10_000 {
                match arb_obs.lock() {
                    Ok(arb) if arb.held(&resource) => return,
                    _ => {}
                }
                tokio::task::yield_now().await;
            }
            panic!("try_claim 未在超时内生效（占坑未出现在仲裁表）");
        };
        tokio::time::timeout(Duration::from_secs(2), wait_claim)
            .await
            .expect("等待占坑生效超时");
        // 取消路径（等同 Timeout Elapsed 丢弃内层 future）：abort 在下一 await
        // （lock_owned）丢弃 future → 守卫 drop → 自动 release 占坑。
        handle.abort();
        assert!(
            handle.await.is_err(),
            "op_mutex_lock 应在物理锁占用期间被取消，而非完成"
        );
        // 修复后：占坑已由守卫释放（release 幂等），仲裁表恢复干净。
        {
            let arb = arb_obs.lock().unwrap();
            assert!(
                !arb.held(&resource),
                "取消后占坑必须已释放（guard drop → release）"
            );
            assert!(arb.is_clean(), "取消后仲裁表应干净");
        }
        drop(_phys_held);
        // 再次 MutexLock 同 id：占坑已释放 → 成功（非永久 WouldBlock）。
        let mut ex2 = ex.lock().await;
        let (_v, undo) = ex2
            .op_mutex_lock(id)
            .await
            .expect("取消后应能重新获取锁（claim 未泄漏）");
        assert!(undo.is_some(), "重新获取的锁应带 undo");
    }

    /// A5 批 9 审计 blocker 的确定性复现：取消路径的守卫 drop 与「另一任务正
    /// 处于仲裁临界区」（arbiter 锁被持有）重叠。
    ///
    /// 修复前（tokio Mutex + `blocking_lock` 兜底）：Drop 恰在 worker 线程 poll
    /// 帧内执行，`try_lock` 失败 → `blocking_lock` → tokio 明确 panic（"Cannot
    /// block the current thread from within a runtime"）→ Drop 内 panic 有
    /// double-panic abort 风险。修复后（std Mutex）：Drop 内 `lock()` 同步阻塞
    /// 等待持锁者完成微秒级临界区，无 panic、无 abort。
    ///
    /// 构造：物理锁被占用 → `op_mutex_lock` 在 try_claim 成功后必然阻塞在
    /// `lock_owned`；此时外部 std 线程持 arbiter 锁（模拟另一任务正处于临界区），
    /// abort 取消任务 → 守卫 drop 的 `lock()` 与外部持锁者重叠（阻塞至其释放）。
    #[tokio::test]
    async fn mutex_claim_guard_drop_contends_with_arbiter_lock() {
        let mut ex = TokioExecutor::new();
        let id = 4242u64;
        // 物理锁被占用且仲裁表空闲：try_claim 成功（无占坑冲突）后 `lock_owned`
        // 必然阻塞，形成守卫 drop 的确定性窗口。
        let phys = ex
            .mutexes
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _phys_held = phys.lock_owned().await;
        let arb_obs = ex.arbiter.clone();
        let ex = Arc::new(tokio::sync::Mutex::new(ex));
        let task_ex = ex.clone();
        let handle = tokio::spawn(async move {
            let mut g = task_ex.lock().await;
            g.op_mutex_lock(id).await
        });
        // 等 try_claim 生效（占坑落表）→ 此刻 future 必阻塞在 lock_owned。
        let resource = Resource::Fd(id);
        let wait_claim = async {
            for _ in 0..10_000 {
                match arb_obs.lock() {
                    Ok(arb) if arb.held(&resource) => return,
                    _ => {}
                }
                tokio::task::yield_now().await;
            }
            panic!("try_claim 未在超时内生效（占坑未出现在仲裁表）");
        };
        tokio::time::timeout(Duration::from_secs(2), wait_claim)
            .await
            .expect("等待占坑生效超时");
        // 外部 std 线程持 arbiter 锁（模拟另一任务正处于仲裁临界区的瞬间）：
        // 经 channel 确认已持锁后再触发取消，保证 drop 与临界区确定性重叠。
        let (tx, rx) = std::sync::mpsc::channel();
        let holder = {
            let arb = arb_obs.clone();
            std::thread::spawn(move || {
                let _g = arb.lock().expect("外部持锁者取锁");
                tx.send(()).unwrap();
                std::thread::sleep(Duration::from_millis(30));
            })
        };
        rx.recv().expect("外部持锁者未就绪");
        // 取消路径（等同 Timeout Elapsed 丢弃内层 future）：abort 在 lock_owned
        // 处丢弃 future → 守卫 drop → lock() 与外部持锁者重叠（安全阻塞等待）。
        handle.abort();
        let join_err = match handle.await {
            Err(e) => e,
            Ok(_) => panic!("op_mutex_lock 应在物理锁占用期间被取消，而非完成"),
        };
        assert!(
            join_err.is_cancelled(),
            "任务应因 abort 取消而非 panic（Drop 内 lock 不得 panic）"
        );
        holder.join().expect("外部持锁线程正常退出");
        // 修复后（std Mutex）：无 panic、无 abort；仲裁表最终干净。
        {
            let arb = arb_obs.lock().unwrap();
            assert!(
                !arb.held(&resource),
                "取消后占坑必须已释放（guard drop → release）"
            );
            assert!(arb.is_clean(), "取消后仲裁表应干净");
        }
        drop(_phys_held);
        // 再次 MutexLock 同 id：占坑已释放 → 成功（非永久 WouldBlock）。
        let mut ex2 = ex.lock().await;
        let (_v, undo) = ex2
            .op_mutex_lock(id)
            .await
            .expect("取消后应能重新获取锁（claim 未泄漏）");
        assert!(undo.is_some(), "重新获取的锁应带 undo");
    }

    /// A5 批 9 确定性单测：`disarm` 后 drop 不触碰 arbiter 锁 —— 手动
    /// `arbiter.lock().unwrap()` 持锁期间 drop 一个已 disarm 的 guard，无 panic、
    /// 无死锁（std Mutex 非重入：armed 守卫在持锁线程内 drop 会自死锁，因此
    /// 释放职责必须经 `disarm` 移交 undo 路径——本测试验证该语义成立）。
    #[test]
    fn claim_guard_disarmed_drop_under_held_arbiter_lock_no_panic() {
        let arbiter = Arc::new(std::sync::Mutex::new(ResourceArbiter::new()));
        let claim_set = vec![ResourceUsage {
            resource: Resource::Fd(7),
            mode: AccessMode::Write,
        }];
        // 模拟 op_mutex_lock 的占坑成功路径：try_claim 已提交（守卫持有占坑职责）。
        assert!(arbiter.lock().unwrap().try_claim(&claim_set));
        // 手动持锁（模拟另一路径正处于 arbiter 临界区，与 drop 重叠）。
        let _outer = arbiter.lock().unwrap();
        let mut guard = ArbiterClaimGuard::new(arbiter.clone(), claim_set.clone());
        guard.disarm(); // 释放职责已移交 undo 路径 → drop 不触碰锁。
        drop(guard); // 持锁期间 drop 已 disarm guard：无 panic、无死锁。
        drop(_outer);
        // disarm 后占坑保留（职责在 undo 路径，防双释放）。
        let arb = arbiter.lock().unwrap();
        assert!(
            arb.held(&Resource::Fd(7)),
            "disarm 后占坑应保留（防双释放）"
        );
    }

    /// RFC-10：Windows 原生错误码 → POSIX 语义归一化（`to_sys_err` 单测）。
    /// std 的 decode_error_kind 已把 Win32/WSA 码解码为语义 kind（实测
    /// raw=80→AlreadyExists、10048→AddrInUse、10061→ConnectionRefused），
    /// 故 `from_raw_os_error` 构造的错误经 kind 优先路径命中；未映射码透传
    /// （与修复前行为一致）。
    #[cfg(windows)]
    #[test]
    fn to_sys_err_maps_windows_codes_to_posix_semantics() {
        use std::io::Error;
        // Win32：文件面
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(80)),
            SysError::AlreadyExists,
            "ERROR_FILE_EXISTS(80) → EEXIST(17)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(183)),
            SysError::AlreadyExists,
            "ERROR_ALREADY_EXISTS(183) → EEXIST(17)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(5)),
            SysError::PermissionDenied,
            "ERROR_ACCESS_DENIED(5) → EACCES(13)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(2)),
            SysError::NotFound,
            "ERROR_FILE_NOT_FOUND(2) → ENOENT(2)"
        );
        // WSA：网络面
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(10048)),
            SysError::Other(98),
            "WSAEADDRINUSE(10048) → EADDRINUSE(98)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(10061)),
            SysError::ConnectionRefused,
            "WSAECONNREFUSED(10061) → ECONNREFUSED(111)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(10054)),
            SysError::ConnectionReset,
            "WSAECONNRESET(10054) → ECONNRESET(104)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(10060)),
            SysError::TimedOut,
            "WSAETIMEDOUT(10060) → ETIMEDOUT(110)"
        );
        // 未映射码：透传（不改变既有行为）。
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(1234)),
            SysError::Other(1234),
            "未映射码透传"
        );
        // 撞码防护（审查 MEDIUM-1）：kind 未命中的 Win32 码不得再经
        // from_errno 重解释——32/13/4 与 POSIX EPIPE/EACCES/EINTR 码值重合，
        // 修复前会误标 BrokenPipe/PermissionDenied/Interrupted。
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(32)),
            SysError::Other(32),
            "ERROR_SHARING_VIOLATION(32) → Other(32)（不得误标 BrokenPipe）"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(13)),
            SysError::Other(13),
            "ERROR_INVALID_DATA(13) → Other(13)（不得误标 PermissionDenied）"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(4)),
            SysError::Other(4),
            "ERROR_TOO_MANY_OPEN_FILES(4) → Other(4)（不得误标 Interrupted）"
        );
    }

    /// JD-1：macOS/BSD Darwin errno → POSIX 语义（kind-first 修复回归）。
    /// `from_raw_os_error` 构造 Darwin 码，std 用 Darwin 常量解码为语义 kind
    /// （EAGAIN=35→WouldBlock、ETIMEDOUT=60→TimedOut 等），kind-first 映射回
    /// POSIX errno 得具名变体；修复前 not(windows) 分支纯透传 Darwin 码 →
    /// WouldBlock/TimedOut/ConnectionReset/ConnectionRefused 全部退化为
    /// Other(n)。本机 Windows 无法执行，但编译必须过（cfg(target_os = "macos")）。
    #[cfg(target_os = "macos")]
    #[test]
    fn to_sys_err_maps_darwin_codes_to_posix_semantics() {
        use std::io::Error;
        // Darwin errno（sys/errno.h）与 Linux 数值不同（Linux 为 11/110/104/111/98/99）。
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(35)),
            SysError::WouldBlock,
            "Darwin EAGAIN(35) → WouldBlock"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(60)),
            SysError::TimedOut,
            "Darwin ETIMEDOUT(60) → TimedOut"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(54)),
            SysError::ConnectionReset,
            "Darwin ECONNRESET(54) → ConnectionReset"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(61)),
            SysError::ConnectionRefused,
            "Darwin ECONNREFUSED(61) → ConnectionRefused"
        );
        // 14 错误集无 AddrInUse/AddrNotAvailable 变体 → Other(98)/Other(99)
        // （与 Linux 上 bind 冲突的真实 errno 一致，跨平台可移植性目标）。
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(48)),
            SysError::Other(98),
            "Darwin EADDRINUSE(48) → Other(98)"
        );
        assert_eq!(
            to_sys_err(Error::from_raw_os_error(49)),
            SysError::Other(99),
            "Darwin EADDRNOTAVAIL(49) → Other(99)"
        );
    }

    /// JD-2：CrossesDevices 臂回归——Windows ERROR_NOT_SAME_DEVICE=17 与
    /// Unix EXDEV=18 的 kind 均为 CrossesDevices，必须经 kind 臂映射到
    /// EXDEV(18) → `SysError::CrossDevice`。修复前（缺臂）：Windows 落 raw
    /// 路径 → from_errno(17)=AlreadyExists（撞码错映射）；Unix 透传 18 恰好
    /// 正确（未被发现）。注意：`from_errno(18)` 在 14 错误集内（CrossDevice
    /// 变体，error.rs 冻结面），非 Other(18)。
    #[test]
    fn to_sys_err_maps_crosses_devices() {
        use std::io::Error;
        #[cfg(windows)]
        let e = Error::from_raw_os_error(17); // ERROR_NOT_SAME_DEVICE
        #[cfg(not(windows))]
        let e = Error::from_raw_os_error(18); // EXDEV
        assert_eq!(
            to_sys_err(e),
            SysError::CrossDevice,
            "kind=CrossesDevices → EXDEV(18) → CrossDevice（非 AlreadyExists）"
        );
    }
}
