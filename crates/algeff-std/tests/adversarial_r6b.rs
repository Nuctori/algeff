//! R6b 对抗审计（第 6 轮 B 块：RFC-10 执行器全物理 IO 错误面回归）——
//! 40+ 转换点接入 `to_sys_err` 后，错误语义与错误路径行为不得退化。
//!
//! 审计对象：`fix/rfc10-windows-errno`（fdd0cfe）对 `executor.rs` 的改造——
//! Windows 原生错误码（Win32/WSA）在 A5 执行器层归一化为 POSIX 语义
//! （`to_sys_err` / `normalize_windows_errno`），冻结面 error.rs 未动。
//! 本文件按 op 类逐面攻击**错误路径**：
//!
//! 1. **文件面**：Open/Stat/Truncate/Unlink/Rename/Mkdir/Rmdir/ReadDir 不存在的
//!    路径（NotFound）、写只读句柄（Windows PermissionDenied / Unix Other(9)
//!    EBADF——平台固有差异，非 RFC-10 回归）；
//! 2. **管道面**：读端关闭后写（Windows BrokenPipe / Unix Other(0)——后者为
//!    RFC-10 未覆盖的纯 kind 错误（无 raw errno）语义丢失点，见文件内注释）、
//!    SendFile 到无读端管道 + SendFile 自拷贝（InvalidInput）；
//! 3. **网络面**：UDP 绑占用端口（Other(98) EADDRINUSE，与 rfc10 场景同源，
//!    本文件补状态毒化断言：首 socket 不受影响、Close 后端口可复用）；
//! 4. **进程面**：Spawn 不存在命令（NotFound，两平台一致）；
//! 5. **映射面**：Mmap 不存在文件（NotFound）；
//! 6. **豁免审计**：`#[cfg(unix)]` op_chmod/op_chown 保留裸 `?`/`SysError::from`
//!    是否合理——Unix 上与 `to_sys_err` 逐字节等价（raw_os_error 即 POSIX
//!    errno）、Windows 编译期排除 → 合理；Windows 侧断言其返回 Other(38) ENOSYS。
//!
//! 每个错误路径均断言：① 期望 SysError 变体（本机 Windows 实测值优先；
//! 与 POSIX 语义有差时断言 Other(n) 并注明）；② 无状态毒化（undo 栈不增长、
//! registry 不残留句柄、轮换型句柄 put_back 恢复——错误后句柄仍可寻址、
//! 重复错误同变体而非 NotFound）；③ 线性标记语义（失败的 Syscall 按设计
//! 消费 Write 标记，但 Own 终结集独立——错误后 Close(Own) 仍合法，r2/r4b
//! 风格断言）。
//!
//! 驱动方式：状态毒化/Catch 断言走 `Runtime::run_blocking`（D9：普通
//! `#[test]`，tokio 上下文之外）；`cfg(windows)` 豁免测试走直接
//! `TokioExecutor::execute`（rfc10_windows_errno.rs 同约定）。
//! Windows 端口预算：1 个 UDP 临时端口（系统分配，无固定端口占用）。

use std::path::PathBuf;

use algeff_core::{
    Action, DataOp, MmapProt, OpenFlags, Owned, PipeFlags, ReadOnly, ResourceInner,
    ResourceRegistry, ResourceUsage, Runtime, SysError, SyscallExecutor, TypedResource, Value,
    WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R4b 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}
fn rd_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Path(path)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

fn syscall(
    op: DataOp,
    resources: Vec<ResourceUsage>,
    next: impl FnOnce(Value) -> Action + Send + 'static,
) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(next),
    }
}

fn read_only_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        ..Default::default()
    }
}

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

/// Open(path, rw+create) → Pure(Fd)。
fn open_fd(rt: &mut Runtime, path: PathBuf) -> u64 {
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path)],
            Action::Pure,
        ))
        .unwrap();
    fd_of(&v)
}

/// 期望 execute 返回错误并取出（Ok 类型含非 Debug 的 UndoOp，不能 unwrap_err）。
async fn exec_err(ex: &mut TokioExecutor, reg: &mut ResourceRegistry, op: &DataOp) -> SysError {
    match ex.execute(op, reg).await {
        Err(e) => e,
        Ok(_) => panic!("期望错误，得到成功"),
    }
}

// ══════════════════════════════════════════════════════════════════════
// 文件面：不存在的路径（Open/Stat/Truncate/Unlink/Rename/Mkdir/Rmdir/ReadDir）
// ══════════════════════════════════════════════════════════════════════

/// Open 不存在的文件（无 create）→ NotFound；不分配 fd、无 undo；随后文件被
/// 外部创建后同 Runtime 再 Open → 成功（失败不粘滞）。
#[test]
fn r6b_open_missing_not_found_no_poison() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("missing-open.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: read_only_flags(),
            },
            vec![rd_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Open 缺失 → NotFound（ENOENT，跨平台）");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");
    assert!(rt.registry().lookup(0).is_none(), "失败不分配 fd");

    // 不粘滞：文件出现后同 Runtime 可正常打开。
    std::fs::write(&p, b"now exists").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: read_only_flags(),
            },
            vec![rd_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(fd_of(&v), 0, "恢复后首个 fd 分配");
}

/// Stat 不存在的路径 → NotFound；无 undo、无句柄；随后 Stat 存在的文件 → Ok。
#[test]
fn r6b_stat_missing_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("missing-stat.txt");
    let exist = dir.path().join("exist.txt");
    std::fs::write(&exist, b"data").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Stat { path: p.clone() },
            vec![rd_path(p)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Stat 缺失 → NotFound（跨平台）");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    let v = rt
        .run_blocking(syscall(
            DataOp::Stat {
                path: exist.clone(),
            },
            vec![rd_path(exist)],
            Action::Pure,
        ))
        .unwrap();
    match v {
        Value::List(l) => {
            assert_eq!(l.len(), 3, "Stat 返回 [len, is_dir, is_file]");
            assert_eq!(l[0], Value::U64(4), "len=4");
        }
        other => panic!("期望 List，得到 {other:?}"),
    }
}

/// Truncate 不存在的路径 → NotFound（metadata 先失败）；不创建文件、无 undo。
#[test]
fn r6b_truncate_missing_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("missing-trunc.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Truncate { path: p.clone(), len: 3 },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Truncate 缺失 → NotFound（跨平台）");
    assert!(!p.exists(), "失败路径不创建文件");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");
}

/// Unlink 不存在的路径 → NotFound；无 undo；随后创建后 Unlink → Ok。
#[test]
fn r6b_unlink_missing_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("missing-unlink.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Unlink { path: p.clone() },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Unlink 缺失 → NotFound（跨平台）");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // 重试前 Replace：失败的 Syscall 已消费 wr_path 的 Write 线性标记（A4
    // check_linear 先行，按设计），Reset 清空标记后同资源可再声明（r1
    // rev_mkdir 同约定）——错误不粘滞、恢复链完整。
    std::fs::write(&p, b"x").unwrap();
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Unlink { path: p.clone() },
        vec![wr_path(p.clone())],
        Action::Pure,
    ))
    .unwrap();
    assert!(!p.exists(), "恢复后 Unlink 成功");
}

/// Rename 不存在的源 → NotFound；目标不被创建；随后源出现后 Rename → Ok。
#[test]
fn r6b_rename_missing_from_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("missing-rename.txt");
    let to = dir.path().join("target.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Rename {
                from: from.clone(),
                to: to.clone(),
            },
            vec![wr_path(from.clone()), wr_path(to.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Rename 缺失源 → NotFound（跨平台）");
    assert!(!to.exists(), "失败不创建目标");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // 重试前 Replace：清空失败的 Syscall 已消费的 Write 线性标记（同
    // r6b_unlink 注；r1 rev_mkdir 同约定）。
    std::fs::write(&from, b"x").unwrap();
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Rename {
            from: from.clone(),
            to: to.clone(),
        },
        vec![wr_path(from.clone()), wr_path(to.clone())],
        Action::Pure,
    ))
    .unwrap();
    assert!(to.exists() && !from.exists(), "恢复后 Rename 成功");
    // 成功路径产生 undo（反向 Rename）。
    assert_eq!(rt.undo_stack().len(), 1);
}

/// Mkdir 双错误路径：(a) 父目录缺失 → NotFound；(b) 路径被文件占用 →
/// AlreadyExists（两平台一致）。均无 undo。
#[test]
fn r6b_mkdir_parent_missing_and_file_collision() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("no-parent").join("sub");
    let file_path = dir.path().join("occupied.txt");
    std::fs::write(&file_path, b"i am a file").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Mkdir {
                path: nested.clone(),
                mode: 0o755,
            },
            vec![wr_path(nested.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Mkdir 父目录缺失 → NotFound（跨平台）");
    assert!(!nested.exists(), "失败不创建目录");

    let e = rt
        .run_blocking(syscall(
            DataOp::Mkdir {
                path: file_path.clone(),
                mode: 0o755,
            },
            vec![wr_path(file_path.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "Mkdir 撞已存在文件 → AlreadyExists（EEXIST，跨平台）"
    );
    assert!(rt.undo_stack().is_empty(), "两次失败均不产生 undo");
}

/// Rmdir 不存在的路径 → NotFound；无 undo；随后创建目录后 Rmdir → Ok。
#[test]
fn r6b_rmdir_missing_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("missing-rmdir");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Rmdir { path: d.clone() },
            vec![wr_path(d.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Rmdir 缺失 → NotFound（跨平台）");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // 重试前 Replace：清空失败的 Syscall 已消费的 Write 线性标记（同
    // r6b_unlink 注）。
    std::fs::create_dir(&d).unwrap();
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Rmdir { path: d.clone() },
        vec![wr_path(d.clone())],
        Action::Pure,
    ))
    .unwrap();
    assert!(!d.exists(), "恢复后 Rmdir 成功");
}

/// ReadDir 不存在的路径 → NotFound；无 undo；随后目录出现后 ReadDir → Ok。
#[test]
fn r6b_read_dir_missing_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("missing-readdir");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::ReadDir { path: d.clone() },
            vec![rd_path(d.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "ReadDir 缺失 → NotFound（跨平台）");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("f.txt"), b"x").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::ReadDir { path: d.clone() },
            vec![rd_path(d)],
            Action::Pure,
        ))
        .unwrap();
    match v {
        Value::List(l) => assert_eq!(l.len(), 1, "恢复后 ReadDir 列出 1 个条目"),
        other => panic!("期望 List，得到 {other:?}"),
    }
}

/// 写只读句柄：Windows ERROR_ACCESS_DENIED(5) → PermissionDenied（RFC-10
/// 归一化；修复前为 Other(5)）；Unix EBADF(9) → Other(9)（平台固有差异，
/// 非回归——本机 Windows 实测值优先，Unix 分支断言 Other(n) 并注明）。
///
/// 状态毒化面：文件内容不变、无 undo、fd 仍在注册表可 Close(Own)（失败
/// Syscall 按设计消费 Write 线性标记，但 Own 终结集独立——错误后 Close 仍
/// 合法，pdr §14）；另一 rw 句柄随后完整 Write/Read/Replace 链成功。
#[test]
fn r6b_write_readonly_fd_error_no_poison_linearity() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("ro.txt");
    std::fs::write(&p, b"0123456789").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // fd0：只读句柄。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: read_only_flags(),
            },
            vec![rd_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let ro_fd = fd_of(&v);
    // fd1：rw 句柄（同一文件，独立 OS 句柄/游标）。
    let rw_fd = open_fd(&mut rt, p.clone());

    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: ro_fd,
                data: b"X".to_vec(),
            },
            vec![wr(ro_fd)],
            Action::Pure,
        ))
        .unwrap_err();
    #[cfg(windows)]
    assert_eq!(
        e,
        SysError::PermissionDenied,
        "Windows 写只读句柄 → ERROR_ACCESS_DENIED(5) → PermissionDenied（RFC-10 归一化）"
    );
    #[cfg(unix)]
    assert_eq!(
        e,
        SysError::Other(9),
        "Unix 写只读句柄 → EBADF(9) → Other(9)（14 错误集无 EBADF；平台固有差异）"
    );

    // 无状态毒化①：文件内容未变、无 undo、失败未丢句柄。
    assert_eq!(std::fs::read(&p).unwrap(), b"0123456789", "失败不改动文件");
    assert!(rt.undo_stack().is_empty(), "失败的 Write 不产生 undo");
    assert!(rt.registry().lookup(ro_fd).is_some(), "失败后只读 fd 仍可寻址");

    // 线性标记：失败 Syscall 已消费 Write 标记（check_linear 先行，按设计），
    // 但 Own 终结集独立 → 错误后 Close(Own) 仍合法（r2/r4b 风格断言）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: ro_fd },
        vec![ow(ro_fd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(
        rt.registry().lookup(ro_fd).is_none(),
        "Close 后只读 fd 释放"
    );

    // 无状态毒化②：rw 句柄不受错误影响——Write + Read 全链路成功，undo 正常。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: rw_fd,
            data: b"X".to_vec(),
        },
        vec![wr(rw_fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"X123456789", "rw 句柄写生效");
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rw_fd, len: 1 },
            vec![rd(rw_fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"1".to_vec()), "写后游标处读回下一字节");
    assert_eq!(rt.undo_stack().len(), 1, "成功 Write 的 undo 正常入栈");

    // 恢复链完整：Replace 撤销写、清空注册表。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"0123456789", "undo 恢复原内容");
    assert!(rt.undo_stack().is_empty(), "Replace 清空 undo");
}

// ══════════════════════════════════════════════════════════════════════
// 管道面：无读端写 / SendFile 错误 / SendFile 自拷贝
// ══════════════════════════════════════════════════════════════════════

/// 管道读端关闭后写（Runtime 层）：tokio duplex 返回纯 kind 错误
/// （`BrokenPipe.into()`，无 raw errno，tokio io/util/mem.rs:279）。RFC-10
/// 两平台行为：
/// - Windows：kind 优先路径 BrokenPipe → errno 32 → `SysError::BrokenPipe`；
/// - Unix：`to_sys_err` = 冻结 `From<io::Error>`（raw_os_error 优先），纯
///   kind 错误 raw=None → **`Other(0)`，kind 语义丢失** —— RFC-10 未覆盖的
///   跨平台不一致点（error.rs 冻结 + src 禁止修改，记录不修；Windows 实测值
///   优先断言，Unix 分支断言 Other(0) 并注明）。
///
/// 状态毒化面：无 undo；Close 写端后 Write → NotFound（映射干净释放）。
/// 句柄恢复（put_back，blocker-3）的重复错误断言见
/// `r6b_pipe_write_no_reader_error_handle_restored`（direct executor——
/// Runtime 层第二次同 wr(fd) 声明会被 A4 线性检查先行拦截，无法到执行器）。
#[test]
fn r6b_pipe_write_no_reader_error() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let (rfd, wfd) = match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List，得到 {other:?}"),
    };

    // 关闭读端 → 管道无读端。
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ))
    .unwrap();

    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: wfd,
                data: b"data".to_vec(),
            },
            vec![wr(wfd)],
            Action::Pure,
        ))
        .unwrap_err();
    #[cfg(windows)]
    assert_eq!(
        e,
        SysError::BrokenPipe,
        "Windows：duplex BrokenPipe（纯 kind）→ kind 路径 → BrokenPipe"
    );
    #[cfg(unix)]
    assert_eq!(
        e,
        SysError::Other(0),
        "Unix：duplex BrokenPipe（纯 kind，raw=None）→ 冻结 From → Other(0)——\
         RFC-10 未修复的 kind 语义丢失点（Unix 分支；Windows 为 BrokenPipe）"
    );
    assert!(rt.undo_stack().is_empty(), "失败的 Write 不产生 undo");

    // 读端已 Close → Read 被 A4 Own 终结语义拦截（check_linear 先行：Own
    // 之后该资源任何 usage 均拒绝，pdr §14）→ InvalidInput；executor 层
    // 的 NotFound 语义在 r6b_pipe_write_no_reader_error_handle_restored
    // （direct executor，无线性面）中断言。
    let e3 = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd, len: 4 },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e3,
        SysError::InvalidInput,
        "Close(Own) 后 Read 同资源 → A4 Own 终结拒绝（设计语义）"
    );

    // 关闭写端 → 随后 Write 同 NotFound（双双干净释放；Own 集独立，错误后
    // Close 合法）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: wfd },
        vec![ow(wfd)],
        Action::Pure,
    ))
    .unwrap();
    let e4 = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: wfd,
                data: b"data".to_vec(),
            },
            vec![wr(wfd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e4,
        SysError::InvalidInput,
        "Close(Own) 后 Write 同资源 → A4 Own 终结拒绝（设计语义；executor 层 \
         NotFound 见 direct executor 测试）"
    );
    // 读端无轮换（Close 前无读操作）→ 注册表直查可观测。
    assert!(rt.registry().lookup(rfd).is_none(), "Close 后读端从注册表释放");
}

/// 管道错误路径的 put_back 句柄恢复（blocker-3，direct executor 层）：
/// 第一次写失败后写端句柄必须已恢复——第二次写同一错误（非 NotFound，即
/// 句柄与映射未被错误路径丢弃）；Close 后 Write → NotFound（干净释放）。
/// 注：轮换型句柄（D1）put_back 后注册表 fd 轮换（逻辑 fd 不变，executor
/// 内部映射隐藏），不做 registry 直查断言。
#[tokio::test]
async fn r6b_pipe_write_no_reader_error_handle_restored() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

    let v = ex
        .execute(
            &DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let (rfd, wfd) = match v.0 {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List，得到 {other:?}"),
    };
    ex.execute(&DataOp::Close { fd: rfd }, &mut reg)
        .await
        .unwrap();

    let e1 = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Write {
            fd: wfd,
            data: b"data".to_vec(),
        },
    )
    .await;
    #[cfg(windows)]
    assert_eq!(e1, SysError::BrokenPipe, "Windows：无读端写 → BrokenPipe");
    #[cfg(unix)]
    assert_eq!(
        e1,
        SysError::Other(0),
        "Unix：无读端写 → Other(0)（纯 kind 错误，见 r6b_pipe_write_no_reader_error 注）"
    );

    // 第二次写：同一错误（非 NotFound）——put_back 已恢复句柄与映射。
    let e2 = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Write {
            fd: wfd,
            data: b"data".to_vec(),
        },
    )
    .await;
    assert_eq!(e2, e1, "重复错误同变体（写端句柄未丢，blocker-3）");

    // Close 后 Write → NotFound（干净释放，无悬空映射）。
    ex.execute(&DataOp::Close { fd: wfd }, &mut reg)
        .await
        .unwrap();
    let e3 = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Write {
            fd: wfd,
            data: b"data".to_vec(),
        },
    )
    .await;
    assert_eq!(e3, SysError::NotFound, "Close 后写端不可寻址");
}

/// SendFile 错误面（Runtime 层）：
/// (a) 输出侧为无读端管道写端 → 与 op_write 管道路径同源错误（Windows
///     BrokenPipe / Unix Other(0)，同上注明）；输入侧文件 fd 不受影响
///     （Seek+Read 仍可用，错误路径不丢输入句柄）；
/// (b) out == input 自拷贝 → InvalidInput（无 io、无状态变化）。
/// 句柄恢复（put_back）的重复错误断言见
/// `r6b_send_file_error_handle_restored`（direct executor——Runtime 层
/// 第二次同 wr(wfd) 声明会被 A4 线性检查先行拦截）。
#[test]
fn r6b_send_file_closed_pipe_error_and_self_copy_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("sendfile-src.txt");
    std::fs::write(&src, b"SRC").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let sfd = open_fd(&mut rt, src.clone());
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let (rfd, wfd) = match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List，得到 {other:?}"),
    };
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ))
    .unwrap();

    // (a) SendFile 输出到无读端管道 → io 错误。
    let e = rt
        .run_blocking(syscall(
            DataOp::SendFile {
                out: wfd,
                input: sfd,
                offset: 0,
                len: 3,
            },
            vec![rd(sfd), wr(wfd)],
            Action::Pure,
        ))
        .unwrap_err();
    #[cfg(windows)]
    assert_eq!(
        e,
        SysError::BrokenPipe,
        "Windows：SendFile 到无读端管道 → BrokenPipe（kind 路径）"
    );
    #[cfg(unix)]
    assert_eq!(
        e,
        SysError::Other(0),
        "Unix：SendFile 到无读端管道 → Other(0)（纯 kind 错误 raw=None；同上注明）"
    );
    assert!(rt.undo_stack().is_empty(), "失败的 SendFile 不产生 undo");

    // 输入侧句柄未被错误路径损坏：Seek+Read 回读源内容。
    rt.run_blocking(syscall(
        DataOp::Seek {
            fd: sfd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        vec![rd(sfd)],
        Action::Pure,
    ))
    .unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: sfd, len: 3 },
            vec![rd(sfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"SRC".to_vec()), "输入文件 fd 仍可用");

    // 输出句柄恢复验证（direct executor 层，见文件头注）：第二次 SendFile
    // 同一错误（非 NotFound）→ Close 成功 → 输入 fd 正常关闭。
    //（Runtime 层此处不可重复同 wr(wfd) 声明——A4 线性检查先行拦截。）
    // (b) 自拷贝 → InvalidInput（拒绝，无 io）。
    let e3 = rt
        .run_blocking(syscall(
            DataOp::SendFile {
                out: sfd,
                input: sfd,
                offset: 0,
                len: 3,
            },
            vec![wr(sfd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e3, SysError::InvalidInput, "SendFile 自拷贝 → InvalidInput");
    assert!(
        rt.registry().lookup(sfd).is_some(),
        "自拷贝拒绝不丢句柄"
    );

    // 清理：全部 fd 可正常关闭。
    for (fd, usage) in [(sfd, ow(sfd)), (wfd, ow(wfd))] {
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![usage], Action::Pure))
            .unwrap();
    }
    assert!(rt.undo_stack().is_empty(), "全程错误路径无 undo 残留");
}

/// SendFile 错误路径的 put_back 句柄恢复（blocker-3，direct executor 层）：
/// 输出侧为无读端管道时第一次 SendFile 失败后，输出句柄与映射必须已恢复——
/// 第二次 SendFile 同一错误（非 NotFound）；随后 Close 输出端成功、输入 fd
/// 仍可读。
#[tokio::test]
async fn r6b_send_file_error_handle_restored() {
    use algeff_core::ResourceHandle;
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("sendfile-src.txt");
    std::fs::write(&src, b"SRC").unwrap();

    // 输入：文件（rw+create）。
    let v = ex
        .execute(
            &DataOp::Open {
                path: src.clone(),
                flags: rw_flags(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let sfd = fd_of(&v.0);
    // 输出：管道写端；先关读端。
    let v = ex
        .execute(
            &DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let (rfd, wfd) = match v.0 {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List，得到 {other:?}"),
    };
    ex.execute(&DataOp::Close { fd: rfd }, &mut reg)
        .await
        .unwrap();

    let op = DataOp::SendFile {
        out: wfd,
        input: sfd,
        offset: 0,
        len: 3,
    };
    let e1 = exec_err(&mut ex, &mut reg, &op).await;
    #[cfg(windows)]
    assert_eq!(e1, SysError::BrokenPipe, "Windows：SendFile 到无读端管道");
    #[cfg(unix)]
    assert_eq!(
        e1,
        SysError::Other(0),
        "Unix：SendFile 到无读端管道 → Other(0)（纯 kind 错误，同上注明）"
    );

    // 第二次 SendFile：同一错误（非 NotFound）——输出句柄已恢复（blocker-3）。
    let e2 = exec_err(&mut ex, &mut reg, &op).await;
    assert_eq!(e2, e1, "重复 SendFile 错误同变体（输出句柄未丢）");

    // 输入 fd 不受影响：Seek+Read 回读源内容。
    ex.execute(
        &DataOp::Seek {
            fd: sfd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        &mut reg,
    )
    .await
    .unwrap();
    let v = ex
        .execute(&DataOp::Read { fd: sfd, len: 3 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v.0, Value::Bytes(b"SRC".to_vec()), "输入文件 fd 仍可用");

    // Close 输出端成功；随后 Write → NotFound（干净释放）。
    ex.execute(&DataOp::Close { fd: wfd }, &mut reg)
        .await
        .unwrap();
    let e3 = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Write {
            fd: wfd,
            data: b"x".to_vec(),
        },
    )
    .await;
    assert_eq!(e3, SysError::NotFound, "Close 后输出端不可寻址");

    // 类型完整性：输入 fd 仍是文件句柄（轮换面外，直查可观测）。
    assert!(
        matches!(reg.lookup(sfd), Some(ResourceHandle::File(_))),
        "输入 fd 仍为文件句柄"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 映射面：Mmap 不存在路径
// ══════════════════════════════════════════════════════════════════════

/// Mmap 不存在的文件 → NotFound；无 undo；随后文件出现后同 Runtime Mmap → Ok
/// （失败不粘滞）。
#[test]
fn r6b_mmap_missing_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("missing-mmap.bin");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Mmap {
                path: p.clone(),
                len: 16,
                prot: MmapProt::default(),
            },
            vec![rd_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Mmap 缺失 → NotFound（跨平台）");
    assert!(rt.undo_stack().is_empty(), "失败的 Mmap 不产生 undo");

    std::fs::write(&p, b"0123456789").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Mmap {
                path: p,
                len: 4,
                prot: MmapProt::default(),
            },
            vec![rd_path(dir.path().join("missing-mmap.bin"))],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"0123".to_vec()), "恢复后 Mmap 正常（len 截断）");
}

// ══════════════════════════════════════════════════════════════════════
// 进程面：Spawn 不存在的命令
// ══════════════════════════════════════════════════════════════════════

/// Spawn 不存在的命令：Windows ERROR_FILE_NOT_FOUND(2) / Unix ENOENT(2) →
/// NotFound（两平台一致）；随后 Spawn 真实命令 + Wait → 正常（children 映射
/// 未被失败污染）。
#[test]
fn r6b_spawn_missing_command_not_found_then_success() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut cmd = std::process::Command::new("definitely-missing-r6b-cmd-xyz");
    cmd.arg("--help");

    let e = rt
        .run_blocking(syscall(
            DataOp::Spawn { cmd },
            vec![],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::NotFound,
        "Spawn 缺失命令 → NotFound（ENOENT，跨平台）"
    );
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // 不粘滞：随后真实命令 Spawn + Wait 成功（children 映射干净）。
    #[cfg(windows)]
    let mut real = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "exit", "3"]);
        c
    };
    #[cfg(not(windows))]
    let mut real = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "exit 3"]);
        c
    };
    let v = rt
        .run_blocking(syscall(
            DataOp::Spawn { cmd: real },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let pid = match v {
        Value::Pid(p) => p,
        other => panic!("期望 Pid，得到 {other:?}"),
    };
    let v = rt
        .run_blocking(syscall(
            DataOp::Wait { pid },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::U64(3), "Wait 取回退出码 3");
}

// ══════════════════════════════════════════════════════════════════════
// 网络面：UDP 绑占用端口（状态毒化）
// ══════════════════════════════════════════════════════════════════════

/// UDP 绑占用端口 → Other(98)（EADDRINUSE；与 rfc10_windows_errno 场景同源，
/// 本测试补状态毒化面）：首 socket 不受失败影响（send_to 仍可用）、失败不分配
/// fd、Close 首 socket 后同地址可再绑（端口复用，无句柄泄漏占用）。
#[test]
fn r6b_udp_bind_occupied_other_98_no_poison() {
    use algeff_core::ResourceHandle;
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::UdpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);
    let addr = match rt.registry().lookup(fd) {
        Some(ResourceHandle::UdpSocket(s)) => s.local_addr().unwrap(),
        other => panic!("期望 UdpSocket 句柄，得到 {other:?}"),
    };

    let e = rt
        .run_blocking(syscall(
            DataOp::UdpBind { addr },
            vec![],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::Other(98),
        "UDP 端口占用 → EADDRINUSE(98)（14 错误集无 AddrInUse 变体，跨平台）"
    );
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");
    // 失败不分配新 fd：首 fd 之后无新句柄（D1 单调，失败的 bind 不 allocate）。
    assert!(
        rt.registry().lookup(fd + 1).is_none(),
        "失败的 bind 不分配 fd"
    );

    // 首 socket 不受失败影响：send_to 仍可用（UDP 无连接，目标无监听也成功投递）。
    rt.run_blocking(syscall(
        DataOp::UdpSendTo {
            fd,
            data: b"ping".to_vec(),
            addr: "127.0.0.1:1".parse().unwrap(),
        },
        vec![rd(fd)],
        Action::Pure,
    ))
    .unwrap();

    // Close 首 socket → 端口释放 → 同地址可再绑（无跨操作泄漏占用端口）。
    rt.run_blocking(syscall(
        DataOp::Close { fd },
        vec![ow(fd)],
        Action::Pure,
    ))
    .unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::UdpBind { addr },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let fd2 = fd_of(&v);
    assert!(
        fd2 != fd,
        "重绑分配新 fd（D1 单调；原 fd 已释放，端口复用）"
    );
    rt.run_blocking(syscall(
        DataOp::Close { fd: fd2 },
        vec![ow(fd2)],
        Action::Pure,
    ))
    .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// Catch 组合：错误可被 Catch 捕获，捕获后可继续执行新操作（不粘滞）
// ══════════════════════════════════════════════════════════════════════

/// Catch 捕获 Stat 缺失错误 → handler 断言变体并返回；随后同 Runtime 新操作
/// 全链路成功（Open+Write 落盘、undo 入栈、Replace 恢复）——错误路径不粘滞、
/// 恢复链完整。
#[test]
fn r6b_catch_error_then_new_ops_work() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.txt");
    let target = dir.path().join("created.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(syscall(
                DataOp::Stat { path: missing.clone() },
                vec![rd_path(missing)],
                Action::Pure,
            )),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "Catch 收到 Stat 缺失 NotFound");
                Action::Pure(Value::Unit)
            }),
        })
        .unwrap();
    assert_eq!(v, Value::Unit, "Catch 捕获并处理错误");
    assert!(rt.undo_stack().is_empty(), "失败 op 不产生 undo");
    assert!(rt.registry().lookup(0).is_none(), "失败 op 不分配 fd");

    // Catch 后继续执行新操作（不粘滞）：Open + Write + Read 全链路。
    let fd = open_fd(&mut rt, target.clone());
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"hello".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"hello", "Write 落盘");
    assert_eq!(rt.undo_stack().len(), 1, "Write undo 正常入栈");

    // Replace：写撤销（文件回到创建时状态=空）、注册表清空、undo 清空。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"", "Write undo 恢复");
    assert!(rt.undo_stack().is_empty(), "Replace 清空 undo");
    assert!(rt.registry().lookup(fd).is_none(), "Replace 释放句柄");
}

// ══════════════════════════════════════════════════════════════════════
// 豁免审计：cfg(unix) op_chmod/op_chown 裸 `?`/`SysError::from` 是否合理
// ══════════════════════════════════════════════════════════════════════

/// 豁免合理性验证（Windows 侧）：op_chmod/op_chown 在非 Unix 平台编译为
/// `Err(Other(38))`（ENOSYS），不经任何 io 转换——RFC-10 转换点盘点中
/// 两处 `#[cfg(unix)]` 豁免不构成 Windows 漏接（Windows 分支无 io 面）。
/// Unix 分支与 `to_sys_err` 逐字节等价（raw_os_error 即 POSIX errno，
/// `SysError::from` 透传），豁免无行为差异（仅 cfg(windows) 断言——
/// Unix 上 chown 权限随 CI 身份变化，不可稳定断言）。
#[cfg(windows)]
#[tokio::test]
async fn r6b_chmod_chown_windows_enosys() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("perm.txt");
    std::fs::write(&p, b"x").unwrap();

    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Chmod {
            path: p.clone(),
            mode: 0o644,
        },
    )
    .await;
    assert_eq!(e, SysError::Other(38), "Windows chmod → ENOSYS(38)");

    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Chown {
            path: p,
            uid: 0,
            gid: 0,
        },
    )
    .await;
    assert_eq!(e, SysError::Other(38), "Windows chown → ENOSYS(38)");

    assert!(reg.lookup(0).is_none(), "ENOSYS 失败不分配句柄");
}
