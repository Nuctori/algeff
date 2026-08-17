//! R6 对抗审计（A1 块：RFC-10 修复正确性）—— algeff-std 执行器层 Windows
//! 错误码归一化对抗（真实 TokioExecutor + Runtime 全链路，不 mock）。
//!
//! 审计对象：fdd0cfe 引入的 `to_sys_err` / `normalize_windows_errno`
//! （executor.rs:63-117）及 40+ 接入点，对照冻结面 `SysError::from_errno`
//! 映射表（error.rs:41-54，14 种 POSIX 错误 + Other 兜底）。
//!
//! 可见性：`to_sys_err`/`normalize_windows_errno` 为私有 fn，集成测试不可
//! 直连（executor 内单测 executor.rs:1532-1608 已锁定 from_raw_os_error
//! 直连链）；本文件全部经**公开物理路径**触发真实 Win32/WSA 码，在 Windows
//! 实机上核验「Win32/WSA 码 → POSIX 码 → SysError 变体」三级链。既有
//! rfc10_windows_errno.rs 已覆盖 create_new/udp 占用/tcp 拒连三个场景，本
//! 文件只补**未覆盖面**：更多物理码链、失败路径毒化、Catch 不粘滞、映射表
//! 边界（未入 14 集的码 → Other(n)）、以及两条已证实缺陷的行为锁定。
//!
//! 发现与修复状态（本文件断言**修复后行为**）：
//! - F1（RFC-10 遗留）：Win32 `ERROR_NOT_SAME_DEVICE(17)` 未归一化 →
//!   跨卷 rename 误映射 `AlreadyExists`（Unix 同蓝图 → `CrossDevice`）；
//!   兜底 `from_errno(raw)` 还会把未映射 Win32 码按 POSIX errno 空间重解释
//!   （如 ERROR_INVALID_DATA(13)→PermissionDenied、
//!   ERROR_TOO_MANY_OPEN_FILES(4)→Interrupted、
//!   ERROR_SHARING_VIOLATION(32)→BrokenPipe）。修复：JD-2（609c393）kind
//!   臂补 `CrossesDevices → 18`；未映射码兑底 `Other(raw)`（MEDIUM-1，
//!   4d9a263）。
//! - F2（错误路径毒化）：`check_linear` 在 syscall **执行前**插入 Write 消费
//!   标记，interpret Syscall 臂 exec 失败直接上抛不回滚 → 失败后同路径再以
//!   Write 模式打开 → `InvalidInput`（r4b 只验证异路径重开，同路径盲区由
//!   本文件补齐）。修复：RFC-12（6ded2db）exec 失败回滚本批标记 + B2
//!   （2bfac05）批内部分失败前缀回滚——同路径重试语义恢复。
//!
//! 文件结构：§1 三级链核验、§2 F1/F2 修复后行为锁定（测试名
//! `xvol_rename_windows_maps_to_cross_device` /
//! `failed_write_syscall_rolls_back_linear_mark`）、§3 错误路径状态毒化、
//! §4 Catch 不粘滞。
//!
//! 疑似（无测试或仅行为锁定，见对应测试注释）：
//! - S1：Rmdir 非空 → Windows `Other(145)` vs Unix `Other(39)`（同蓝图跨平台
//!   Other(n) 不一致；EADDRINUSE 已归一化 98，ENOTEMPTY 类未归一化 → 策略
//!   不一致，normalize 表无 145→39）。
//! - S2：本沙箱实测 RST 后 read 报 WSAECONNABORTED(10053)→`Other(103)`，
//!   普通 Windows 栈可能报 10054→`ConnectionReset`（环境相关；10054 链已由
//!   executor 单测锁定）。
//!
//! Windows 端口预算：每测试 1-2 个系统分配临时端口（无固定端口），远低于
//! 500 上限。

use std::path::PathBuf;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceHandle, ResourceInner, ResourceRegistry,
    ResourceUsage, Runtime, SysError, SyscallExecutor, TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R4 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
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

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

fn read_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        ..Default::default()
    }
}

/// 期望 execute 返回错误并取出（Ok 类型含非 Debug 的 UndoOp，不能 unwrap_err）。
async fn exec_err(ex: &mut TokioExecutor, reg: &mut ResourceRegistry, op: &DataOp) -> SysError {
    match ex.execute(op, reg).await {
        Err(e) => e,
        Ok(_) => panic!("期望错误，得到成功"),
    }
}

/// 跨卷 rename 物理前提构造：源 = 进程临时目录（本机 X:\Temp），目标 =
/// 仓库所在卷（本机 D:，经 CARGO_MANIFEST_DIR 定位到 target/ 下，gitignore
/// 不污染工作树）。`tag` 保证并发测试间路径唯一（两处使用该前提的测试并行
/// 跑，共享路径会互删文件 → Windows delete-pending 竞态）。同卷环境
/// （std rename 成功）→ None（前提不成立，空跑）。
fn xvol_rename_paths(tag: &str) -> Option<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    let src_dir = std::env::temp_dir().join(format!("algeff_r6_xvol_src_{tag}"));
    let dst_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("algeff_r6_xvol_dst_{tag}"));
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::create_dir_all(&dst_dir).unwrap();
    let from = src_dir.join("cross.txt");
    std::fs::write(&from, b"payload").unwrap();
    let to = dst_dir.join("cross.txt");
    if std::fs::rename(&from, &to).is_ok() {
        // 同卷：跨卷前提不成立。清理后空跑（环境守卫，非断言弱化）。
        std::fs::remove_file(&to).ok();
        std::fs::remove_dir_all(&dst_dir).ok();
        std::fs::remove_dir_all(&src_dir).ok();
        eprintln!("跳过跨卷场景：temp_dir 与仓库同卷，前提不成立");
        None
    } else {
        Some((from, to, src_dir, dst_dir))
    }
}

fn xvol_cleanup(src_dir: &PathBuf, dst_dir: &PathBuf) {
    std::fs::remove_dir_all(dst_dir).ok();
    std::fs::remove_dir_all(src_dir).ok();
}

// ══════════════════════════════════════════════════════════════════════
// §1 RFC-10 三级链物理核验（Win32/WSA 码 → POSIX 码 → SysError 变体）
// ══════════════════════════════════════════════════════════════════════

/// Win32 ERROR_FILE_NOT_FOUND(2)/ERROR_PATH_NOT_FOUND(3) → ENOENT(2) →
/// NotFound。五个文件面接入点（Open/Stat/Truncate/Unlink/ReadDir）统一收敛。
#[tokio::test]
async fn chain_missing_path_ops_map_not_found() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let missing = std::env::temp_dir().join(format!("algeff_r6_missing_{}", std::process::id()));

    assert_eq!(
        exec_err(
            &mut ex,
            &mut reg,
            &DataOp::Open {
                path: missing.clone(),
                flags: read_flags(),
            },
        )
        .await,
        SysError::NotFound,
        "Open 缺失路径 → NotFound"
    );
    assert_eq!(
        exec_err(
            &mut ex,
            &mut reg,
            &DataOp::Stat {
                path: missing.clone()
            }
        )
        .await,
        SysError::NotFound,
        "Stat 缺失路径 → NotFound"
    );
    assert_eq!(
        exec_err(
            &mut ex,
            &mut reg,
            &DataOp::Truncate {
                path: missing.clone(),
                len: 4,
            },
        )
        .await,
        SysError::NotFound,
        "Truncate 缺失路径 → NotFound"
    );
    assert_eq!(
        exec_err(
            &mut ex,
            &mut reg,
            &DataOp::Unlink {
                path: missing.clone()
            }
        )
        .await,
        SysError::NotFound,
        "Unlink 缺失路径 → NotFound"
    );
    assert_eq!(
        exec_err(
            &mut ex,
            &mut reg,
            &DataOp::ReadDir {
                path: missing.clone()
            }
        )
        .await,
        SysError::NotFound,
        "ReadDir 缺失路径 → NotFound"
    );
    // 全失败路径零副作用：无 fd 分配。
    assert!(reg.lookup(0).is_none(), "失败链不分配句柄");
}

/// Win32 ERROR_ACCESS_DENIED(5) → EACCES(13) → PermissionDenied（Unix 原生
/// EACCES 收敛）。真实只读文件物理触发（executor 单测只覆盖 from_raw_os_error
/// 直连，本测试走真实 CreateFileW/EEXIST 面）。
#[tokio::test]
async fn chain_write_readonly_file_maps_permission_denied() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("ro.txt");
    std::fs::write(&p, b"secret").unwrap();
    #[cfg(windows)]
    {
        // 1.96：set_readonly 已是 Permissions 固有方法（Windows 无 from_mode）。
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(&p, perm).unwrap();
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o444)).unwrap();
        // root 守卫：root 无视权限位 → 前提不成立空跑。
        if std::fs::OpenOptions::new().write(true).open(&p).is_ok() {
            eprintln!("跳过：Unix root 下只读位不生效，无法构造 EACCES 前提");
            return;
        }
    }

    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Open {
            path: p.clone(),
            flags: OpenFlags {
                write: true,
                ..Default::default()
            },
        },
    )
    .await;
    assert_eq!(
        e,
        SysError::PermissionDenied,
        "只读文件写打开 → PermissionDenied（Win32 5→EACCES，Unix 原生）"
    );
    assert_eq!(std::fs::read(&p).unwrap(), b"secret", "失败不改动文件");
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");

    #[cfg(windows)]
    {
        // 恢复可写属性，否则 tempdir 清理时 DeleteFileW 报 ACCESS_DENIED。
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm).unwrap();
    }
}

/// 目录/文件类型链：ReadDir 普通文件 → Windows ERROR_DIRECTORY(267)（std
/// 解码 kind=NotADirectory）与 Unix ENOTDIR(20) 均收敛 NotADirectory。
#[tokio::test]
async fn chain_read_dir_on_file_maps_not_a_directory() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("plain.txt");
    std::fs::write(&f, b"x").unwrap();

    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(&mut ex, &mut reg, &DataOp::ReadDir { path: f }).await;
    assert_eq!(
        e,
        SysError::NotADirectory,
        "ReadDir 普通文件 → NotADirectory（跨平台收敛）"
    );
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");
}

/// Win32 ERROR_ALREADY_EXISTS(183) 与 Unix EEXIST(17) 收敛 AlreadyExists；
/// 失败后目录仍可写（不粘滞）、随后新建目录成功（executor 无残留）。
#[tokio::test]
async fn chain_mkdir_existing_maps_already_exists_dir_usable() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("sub");
    std::fs::create_dir(&d).unwrap();

    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Mkdir {
            path: d.clone(),
            mode: 0o755,
        },
    )
    .await;
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "Mkdir 已存在 → AlreadyExists（跨平台收敛）"
    );
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");

    // 不粘滞：目录仍可写文件；随后 mkdir 新目录成功。
    std::fs::write(d.join("f.txt"), b"y").unwrap();
    assert_eq!(std::fs::read(d.join("f.txt")).unwrap(), b"y");
    let d2 = dir.path().join("fresh");
    ex.execute(
        &DataOp::Mkdir {
            path: d2.clone(),
            mode: 0o755,
        },
        &mut reg,
    )
    .await
    .unwrap();
    assert!(d2.is_dir(), "失败后同 executor 新建目录成功");
}

/// 映射表边界（未入 14 错误集的码 → Other(n)）：Rmdir 非空 → Windows
/// ERROR_DIR_NOT_EMPTY(145) → Other(145)；Unix ENOTEMPTY(39) → Other(39)。
/// 行为锁定 + 疑似缺陷 S1 记录：同蓝图跨平台 Other(n) 不一致（EADDRINUSE 已
/// 归一化 98，ENOTEMPTY 类未归一化 → normalize 表策略不一致）。
/// 毒化检查：失败后目录内容仍可读；清空后同 executor Rmdir 成功。
#[tokio::test]
async fn chain_rmdir_nonempty_platform_other_locked() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("nonempty");
    std::fs::create_dir(&d).unwrap();
    std::fs::write(d.join("inner.txt"), b"x").unwrap();

    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(&mut ex, &mut reg, &DataOp::Rmdir { path: d.clone() }).await;
    #[cfg(windows)]
    assert_eq!(
        e,
        SysError::Other(145),
        "Windows ERROR_DIR_NOT_EMPTY(145) → Other(145)（S1 行为锁定）"
    );
    #[cfg(not(windows))]
    assert_eq!(
        e,
        SysError::Other(39),
        "Unix ENOTEMPTY(39) → Other(39)（S1 行为锁定）"
    );

    // 毒化：目录内容仍可读；清空后 Rmdir 成功（错误不粘滞）。
    assert_eq!(std::fs::read(d.join("inner.txt")).unwrap(), b"x");
    std::fs::remove_file(d.join("inner.txt")).unwrap();
    ex.execute(&DataOp::Rmdir { path: d.clone() }, &mut reg)
        .await
        .unwrap();
    assert!(!d.is_dir(), "清空后同 executor Rmdir 成功");
}

/// WSAEADDRINUSE(10048)/EADDRINUSE(98) → Other(98)（14 集无 AddrInUse 变体，
/// 归一化到 POSIX 98 保持跨平台一致）：UDP 与 TCP 两条 bind 接入点同链。
/// 毒化：冲突失败后首 socket 自环收发不受影响；Close 后同端口可重绑（无
/// 句柄泄漏占用）。
#[tokio::test]
async fn chain_bind_conflict_maps_other_98_udp_and_tcp() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

    // UDP：先绑 0（系统分配）→ 再绑同地址 → 冲突。
    let v = ex
        .execute(
            &DataOp::UdpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let fd = fd_of(&v.0);
    let addr = match reg.lookup(fd) {
        Some(ResourceHandle::UdpSocket(s)) => s.local_addr().unwrap(),
        _ => panic!("期望 UdpSocket 句柄"),
    };
    let e = exec_err(&mut ex, &mut reg, &DataOp::UdpBind { addr }).await;
    assert_eq!(
        e,
        SysError::Other(98),
        "UDP 端口占用 → Other(98)（EADDRINUSE 归一化，跨平台一致）"
    );

    // 首 socket 不受失败影响：自环 send → recv。
    ex.execute(
        &DataOp::UdpSendTo {
            fd,
            data: b"ping".to_vec(),
            addr,
        },
        &mut reg,
    )
    .await
    .unwrap();
    let v = ex
        .execute(&DataOp::UdpRecvFrom { fd, len: 8 }, &mut reg)
        .await
        .unwrap();
    match v.0 {
        Value::List(l) => assert_eq!(l[0], Value::Bytes(b"ping".to_vec()), "自环数据回读"),
        other => panic!("期望 List([Bytes, Addr])，得到 {other:?}"),
    }

    // Close 后同端口重绑成功（无句柄泄漏）。
    ex.execute(&DataOp::Close { fd }, &mut reg).await.unwrap();
    ex.execute(&DataOp::UdpBind { addr }, &mut reg)
        .await
        .unwrap();

    // TCP：std listener 占端口 → TcpBind 冲突 → 同归一化 Other(98)。
    let l1 = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taddr = l1.local_addr().unwrap();
    let e = exec_err(&mut ex, &mut reg, &DataOp::TcpBind { addr: taddr }).await;
    assert_eq!(
        e,
        SysError::Other(98),
        "TCP bind 占用 → Other(98)（同一 AddrInUse 链）"
    );
}

/// 映射表边界：未入 14 错误集的 WSAEADDRNOTAVAIL(10049) → EADDRNOTAVAIL(99)
/// → Other(99)。绑 TEST-NET 非本机地址物理触发（Windows/Unix 均 EADDRNOTAVAIL，
/// 跨平台一致；与 rfc10 的 Other(98) 相邻表项互补锁定）。
#[tokio::test]
async fn chain_udp_bind_non_local_maps_other_99() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::UdpBind {
            addr: "192.0.2.7:0".parse().unwrap(),
        },
    )
    .await;
    assert_eq!(
        e,
        SysError::Other(99),
        "绑非本机地址 → Other(99)（EADDRNOTAVAIL 归一化，未入 14 集码 → Other(n)）"
    );
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");
}

/// WSAECONNREFUSED(10061) → ECONNREFUSED(111) → ConnectionRefused；失败后
/// 无 fd 残留、同 executor 连真实 listener 成功（fd 0，错误不粘滞）。
#[tokio::test]
async fn chain_tcp_connect_refused_maps_connection_refused_not_sticky() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

    // 绑临时端口后立即关闭 → 无监听 → RST → ECONNREFUSED（两平台一致）。
    // 加固（R2 审计）：全量并行负载下刚释放的端口可能被其他测试进程
    // 立即重用（Windows 快速重用），connect 偶发成功——重试至多 5 个端口。
    let mut refused = false;
    for _ in 0..5 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        match ex.execute(&DataOp::TcpConnect { addr }, &mut reg).await {
            Err(SysError::ConnectionRefused) => {
                refused = true;
                break;
            }
            Err(e) => panic!("意外错误：{e:?}"),
            Ok((Value::Fd(fd), _)) => {
                // 端口被重用（并行负载竞态）→ 关闭该 fd 后换端口重试
                ex.execute(&DataOp::Close { fd }, &mut reg).await.unwrap();
            }
            Ok(_) => panic!("TcpConnect 意外返回值"),
        }
    }
    assert!(
        refused,
        "5 个临时端口均未得到 ConnectionRefused（端口重用竞态持续）"
    );
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");

    // 不粘滞：连真实 listener 成功，fd 从 0 起（无流表残留）。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr2 = listener.local_addr().unwrap();
    let v = ex
        .execute(&DataOp::TcpConnect { addr: addr2 }, &mut reg)
        .await
        .unwrap();
    let fd = fd_of(&v.0);
    assert_eq!(fd, 0, "失败未消耗 fd 编号");
    ex.execute(&DataOp::Close { fd }, &mut reg).await.unwrap();
}

/// ERROR_FILE_NOT_FOUND(2) → NotFound：spawn 缺失程序（Spawn 接入点
/// executor.rs:899，与文件面同链归一化）。
#[tokio::test]
async fn chain_spawn_missing_program_maps_not_found() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let cmd = std::process::Command::new("algeff_r6_definitely_missing_program_zzz");
    let e = exec_err(&mut ex, &mut reg, &DataOp::Spawn { cmd }).await;
    assert_eq!(
        e,
        SysError::NotFound,
        "spawn 缺失程序 → NotFound（Win32 ERROR_FILE_NOT_FOUND 归一化）"
    );
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");
}

// ══════════════════════════════════════════════════════════════════════
// §2 已证实缺陷锁定（F1：RFC-10 遗留，src 冻结只记录不修）
// ══════════════════════════════════════════════════════════════════════

/// F1 缺陷锁定：Win32 ERROR_NOT_SAME_DEVICE(17)（跨卷 rename 物理触发）未
/// 归一化 → 误映射 `AlreadyExists`；Unix 同蓝图 → EXDEV(18) → `CrossDevice`。
/// 跨平台错误语义一致性（RFC-10 目标）在该接入点仍被破坏。
/// 根因：normalize_windows_errno 表缺 `17→18`（executor.rs:98-114，表内
/// 80|183→17 只覆盖了 EEXIST 侧）；kind 优先路径无 `CrossesDevices` 臂
/// （executor.rs:64-92）；兜底 `from_errno(normalize(raw))` 把未映射 Win32
/// 码按 POSIX errno 空间重解释（error.rs:90-94 碰撞面）。
/// 修复方向（不在本审计内执行）：表加 `17→18` + kind 臂 `CrossesDevices→
/// 修复状态（JD-2，609c393）：kind 臂补 `CrossesDevices → Some(18)` 后，
/// Windows 跨卷 rename 现正确映射 `CrossDevice`——本测试断言修复后行为。
#[tokio::test]
async fn xvol_rename_windows_maps_to_cross_device() {
    let Some((from, to, src_dir, dst_dir)) = xvol_rename_paths("defect") else {
        return;
    };
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Rename {
            from: from.clone(),
            to: to.clone(),
        },
    )
    .await;
    #[cfg(windows)]
    assert_eq!(
        e,
        SysError::CrossDevice,
        "Windows 跨卷 rename → EXDEV(18) → CrossDevice（JD-2 修复后行为）"
    );
    #[cfg(not(windows))]
    assert_eq!(
        e,
        SysError::CrossDevice,
        "Unix 跨卷 rename → EXDEV → CrossDevice"
    );
    // 失败原子性：源文件完好。
    assert_eq!(
        std::fs::read(&from).unwrap(),
        b"payload",
        "失败不改动源文件"
    );
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");
    xvol_cleanup(&src_dir, &dst_dir);
}

// ══════════════════════════════════════════════════════════════════════
// §3 错误路径状态毒化（r4b 风格：错误后 undo 栈/registry/线性标记不残留）
// ══════════════════════════════════════════════════════════════════════

/// F2 缺陷锁定（已证实，跨平台核心逻辑）：`check_linear` 在 syscall **执行
/// 前**插入 Write 消费标记（resource.rs:337），interpret Syscall 臂 exec 失败
/// 直接 `?` 上抛（runtime.rs）不回滚 → 失败后同路径再以 Write 模式打开 →
/// InvalidInput（线性标记残留 = 状态毒化）。r4b 的
/// open_exclusive_existing_fails_no_state_poison 只验证了异路径重开（p2），
/// 同路径盲区由本测试补齐。
/// 修复状态（RFC-12，6ded2db）：exec 失败路径已回滚本批预插入的 Write/Own
/// 标记（与 A7 仲裁「失败回滚」同原则）——本测试现断言**修复后行为**（同路径
/// 重开成功）。
#[test]
fn failed_write_syscall_rolls_back_linear_mark() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("keep.txt");
    std::fs::write(&p, b"data").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "exclusive 撞已存在（RFC-10 归一化）"
    );
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");
    assert!(rt.registry().lookup(0).is_none(), "失败不分配 fd");

    // RFC-12 修复后：失败路径线性标记已回滚 → 同路径 Write 模式重开成功。
    let e2 = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    assert!(
        matches!(e2, Value::Fd(_)),
        "RFC-12 修复后行为：失败 Write 的线性标记已回滚，同路径重开成功，得到 {e2:?}"
    );

    // 对照：Read 模式不查 consumed → 同路径读打开不受影响（物理文件完好）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: read_flags(),
            },
            vec![rd_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd, len: 4 },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"data".to_vec()), "物理文件未被失败污染");
}

/// rename 失败路径毒化（F1 场景复用）：跨卷失败后源文件完好、无 fd 残留、
/// 同卷 rename 照常成功、源文件可经 executor 打开读取（错误不粘滞）。
#[tokio::test]
async fn poison_failed_rename_source_intact_not_sticky() {
    let Some((from, to, src_dir, dst_dir)) = xvol_rename_paths("poison") else {
        return;
    };
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Rename {
            from: from.clone(),
            to: to.clone(),
        },
    )
    .await;
    // 映射本身为 F1 缺陷（Windows AlreadyExists / Unix CrossDevice），此处聚焦毒化。
    let _ = e;

    assert_eq!(std::fs::read(&from).unwrap(), b"payload", "源文件完好");
    assert!(reg.lookup(0).is_none(), "rename 失败不分配 fd");

    // 不粘滞：同卷 rename 成功 + 反向 rename 成功（undo 语义面无残留）。
    let f2 = src_dir.join("b.txt");
    let t2 = src_dir.join("c.txt");
    std::fs::write(&f2, b"y").unwrap();
    ex.execute(
        &DataOp::Rename {
            from: f2.clone(),
            to: t2.clone(),
        },
        &mut reg,
    )
    .await
    .unwrap();
    assert!(!f2.exists() && t2.exists(), "失败后同卷 rename 成功");
    ex.execute(
        &DataOp::Rename {
            from: t2.clone(),
            to: f2.clone(),
        },
        &mut reg,
    )
    .await
    .unwrap();
    assert!(!t2.exists() && f2.exists(), "反向 rename 同样可用");

    // 源文件仍可经 executor 打开读取（文件面无残留状态）。
    let v = ex
        .execute(
            &DataOp::Open {
                path: from.clone(),
                flags: read_flags(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let fd = fd_of(&v.0);
    let v = ex
        .execute(&DataOp::Read { fd, len: 7 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v.0, Value::Bytes(b"payload".to_vec()), "失败后仍可读源文件");
    ex.execute(&DataOp::Close { fd }, &mut reg).await.unwrap();

    xvol_cleanup(&src_dir, &dst_dir);
}

// ══════════════════════════════════════════════════════════════════════
// §4 Catch 捕获且不粘滞（r3a 风格：executor IO 错误可被 Catch 捕获）
// ══════════════════════════════════════════════════════════════════════

/// Catch 捕获 RFC-10 归一化 IO 错误（exclusive 撞已存在 → AlreadyExists）：
/// handler 收到正确变体；失败无 undo/fd 残留；随后同一 Runtime 读打开并读取
/// 同一文件成功（错误不粘滞）。注：后续用 Read 模式重开——Write 面残留为 F2
/// 缺陷，见 failed_write_syscall_rolls_back_linear_mark。
#[test]
fn catch_rfc10_io_error_is_caught_and_not_sticky() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("keep.txt");
    std::fs::write(&p, b"keep-data").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(syscall(
                DataOp::Open {
                    path: p.clone(),
                    flags: OpenFlags {
                        write: true,
                        create: true,
                        exclusive: true,
                        ..Default::default()
                    },
                },
                vec![wr_path(p.clone())],
                Action::Pure,
            )),
            handler: Box::new(|e| {
                assert_eq!(
                    e,
                    SysError::AlreadyExists,
                    "Catch 收到 RFC-10 归一化错误（Win32 80/183 → EEXIST）"
                );
                Action::Pure(Value::U64(42))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(42), "handler 返回值");
    assert!(rt.undo_stack().is_empty(), "失败 Open 不产生 undo");
    assert!(rt.registry().lookup(0).is_none(), "失败 Open 不分配 fd");

    // 不粘滞：同一 Runtime 随后读打开并读取同一文件成功。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: read_flags(),
            },
            vec![rd_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd, len: 9 },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"keep-data".to_vec()), "Catch 后同文件可读");
    rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
        .unwrap();
    assert!(rt.registry().lookup(fd).is_none(), "Close 释放句柄");
    // read 产生 1 个游标逆（A6 游标可观察）；Catch 失败路径无 undo。
    assert_eq!(
        rt.undo_stack().len(),
        1,
        "read 游标逆（Catch 失败路径无 undo）"
    );
}

/// Catch 捕获网络错误（TcpConnect 被拒 → ConnectionRefused）且不粘滞：
/// 捕获后同一 Runtime 连真实 listener 成功、fd 从 0 起（无流表/注册表残留）。
#[test]
fn catch_network_error_is_caught_and_not_sticky() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // 无监听 → RST → ConnectionRefused。

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(syscall(DataOp::TcpConnect { addr }, vec![], Action::Pure)),
            handler: Box::new(|e| {
                assert_eq!(
                    e,
                    SysError::ConnectionRefused,
                    "Catch 收到 ConnectionRefused（WSAECONNREFUSED 归一化）"
                );
                Action::Pure(Value::U64(7))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(7), "handler 返回值");
    assert!(rt.undo_stack().is_empty(), "失败连接不产生 undo");

    // 不粘滞：随后连真实 listener 成功。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr2 = listener.local_addr().unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::TcpConnect { addr: addr2 },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);
    assert_eq!(fd, 0, "失败未消耗 fd 编号");
    rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
        .unwrap();
    assert!(rt.registry().lookup(fd).is_none(), "Close 释放句柄");
}
