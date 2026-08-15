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

/// 持锁 guard 的停车位：undo（recover 路径）与显式 `MutexUnlock` 均可取走释放，幂等。
type HeldLockSlot = Arc<tokio::sync::Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>>;

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
    /// tokio Mutex 保护跨任务访问（try_claim/release 为同步短临界区，无嵌套锁、
    /// 无循环等待）。
    arbiter: Arc<tokio::sync::Mutex<ResourceArbiter>>,
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
            arbiter: Arc::new(tokio::sync::Mutex::new(ResourceArbiter::new())),
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
        let file = o.open(path).await?;
        // registry 簿记 token：try_clone 共享同一 OS 描述（真实工作对象在 executor 侧）。
        let token = file.try_clone().await?;
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
            let n = g.read(&mut buf).await?;
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
            let n = n?;
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
            let pos = file.seek(std::io::SeekFrom::Current(0)).await?;
            let orig_len = file.metadata().await?.len();
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
                file.seek(std::io::SeekFrom::Start(pos)).await?;
                if readable {
                    orig.truncate(filled);
                    file.write_all(data).await?;
                    // 撤销：恢复原区域 + 截断回写前长度（D15：仅捕获物理数据）。
                    // 审计 R1（对抗测试 rev_undo_restores_file_cursor）：A6 双态
                    // w;w̄ = 1 要求**全部可观察状态**复原——游标（经 Seek(Current)
                    // 可观察）也必须回到写前位置。修复：恢复内容与长度后 seek 回
                    // `pos`（此前游标停留在 pos+orig.len()，破坏撤销双态）。
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
                    file.write_all(data).await?;
                    None // 写前读失败（如只写句柄）→ 降级 BestEffort。
                }
            } else {
                file.write_all(data).await?;
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
            r?;
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
            r?;
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
        let pos = g.seek(sf).await?;
        Ok((Value::U64(pos), None))
    }

    async fn op_stat(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        let meta = tokio::fs::metadata(path).await?;
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
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await?;
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
        // 同步系统调用（快速路径，无阻塞风险）。
        std::os::unix::fs::chown(path, Some(uid), Some(gid)).map_err(SysError::from)?;
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
        let meta = tokio::fs::metadata(path).await?;
        let undo = if meta.len() < FULL_UNDO_MAX_BYTES {
            let orig = tokio::fs::read(path).await?; // 写前读（Full 策略）。
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await?
                .set_len(len as u64)
                .await?;
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
                .await?
                .set_len(len as u64)
                .await?;
            None // BestEffort（pdr.md §11.2）：大文件（≥1MB）不撤销。
        };
        Ok((Value::Unit, undo))
    }

    async fn op_unlink(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        tokio::fs::remove_file(path).await?;
        // undo=None：恢复需缓存原内容+元数据（BestEffort/Skip）；补偿挂钩由用户提供（RFC-05）。
        Ok((Value::Unit, None))
    }

    async fn op_rename(
        &mut self,
        from: &Path,
        to: &Path,
    ) -> Result<(Value, Option<UndoOp>), SysError> {
        tokio::fs::rename(from, to).await?;
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
        tokio::fs::create_dir(path).await?;
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
        tokio::fs::remove_dir(path).await?;
        // undo=None：恢复目录内容不可行（BestEffort/Skip）；补偿挂钩由用户提供（RFC-05）。
        Ok((Value::Unit, None))
    }

    async fn op_read_dir(&mut self, path: &Path) -> Result<(Value, Option<UndoOp>), SysError> {
        let mut rd = tokio::fs::read_dir(path).await?;
        let mut names = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
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
        let listener = TcpListener::bind(addr).await?;
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
            ResourceHandle::TcpListener(l) => l.accept().await?,
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
        let stream = TcpStream::connect(addr).await?;
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
        let n = n?;
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
        r?;
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
            Err(e) => return Err(SysError::from(e)), // 底层句柄失效，无法恢复（深边缘）。
        };
        if let Err(e) = std_stream.set_nonblocking(true) {
            let err = SysError::from(e);
            return match tokio::net::TcpStream::from_std(std_stream) {
                Ok(s) => {
                    self.put_back(fd, ResourceHandle::TcpStream(Arc::new(s)), reg);
                    Err(err)
                }
                Err(_) => Err(err), // 恢复失败（深边缘）：std 句柄被 from_std 消费。
            };
        }
        if let Err(e) = std_stream.shutdown(*how) {
            let err = SysError::from(e);
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
            Err(e) => return Err(SysError::from(e)), // std 句柄被 from_std 消费，无法恢复。
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
        let sock = UdpSocket::bind(addr).await?;
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
        let (n, addr) = sock.recv_from(&mut buf).await?;
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
        let _ = sock.send_to(data, addr).await?;
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
        let child = tc.spawn()?;
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
                return Err(SysError::from(e));
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
                child.start_kill().map_err(SysError::from)
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
        let mut bytes = tokio::fs::read(path).await?;
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
            if arbiter.lock().await.try_claim(&claim_set) {
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
        // 占坑成功 → 本仲裁域内无竞争者持有物理锁（不变量：占坑 ⟺ 持锁，见
        // resource-notes §2），lock_owned 几乎立即获得；仍保留物理锁保证
        // 跨仲裁域（如独立执行器实例）互斥。
        let guard = m.lock_owned().await;
        // 停车位：undo（recover 路径）与显式 MutexUnlock 均可取走释放（幂等）。
        let slot = self
            .held_locks
            .entry(id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone();
        *slot.lock().await = Some(guard);
        let undo_slot = slot.clone();
        let undo: UndoOp = Box::pin(async move {
            // undo 顺序：先释放物理锁、再释放 arbiter 占坑。释放窗口内占坑仍持有，
            // 新占坑者会失败重试，不会出现「新锁已持有期间被旧 undo 释放占坑」的窗口
            // （保持 占坑 ⟺ 持锁 不变量）。
            if let Some(g) = undo_slot.lock().await.take() {
                drop(g);
            }
            // 幂等：release 对未占坑资源是 no-op（显式 MutexUnlock 已释放时安全）。
            arbiter.lock().await.release(&claim_set);
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
        self.arbiter.lock().await.release(&claim_set);
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
            g.seek(std::io::SeekFrom::Start(offset as u64)).await?;
            let mut buf = vec![0u8; len];
            let mut filled = 0usize;
            while filled < buf.len() {
                let n = g.read(&mut buf[filled..]).await?;
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
            g.write(&buf).await?
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
            n?
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
            n?
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
