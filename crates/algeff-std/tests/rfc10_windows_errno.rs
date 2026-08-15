//! RFC-10 回归：Windows 原生错误码（Win32/WSA）在 A5 执行器层归一化为 POSIX
//! 语义，保证跨平台错误语义一致（修复：`executor.rs` 的 `to_sys_err` /
//! `normalize_windows_errno`，冻结面 error.rs 未动）。
//!
//! 三个场景均为**跨平台**断言（Linux 上本就返回对应 POSIX errno，Windows
//! 修复前分别退化为 Other(80)/Other(10048)/Other(10061)）：
//!
//! 1. `create_new`（exclusive）撞已存在文件 → `AlreadyExists`（EEXIST）；
//! 2. UDP 端口占用 → `Other(98)`（EADDRINUSE，14 错误集无 AddrInUse 变体）；
//! 3. TCP 连接被拒 → `ConnectionRefused`（ECONNREFUSED）。
//!
//! 与 `tests/executor.rs` 同约定：直接调用 `TokioExecutor::execute` +
//! `ResourceRegistry`（不经 interpret），不 mock、真实 syscall。
//! Windows 端口预算：1 个 UDP 临时端口 + 1 个 TCP 临时端口（均为系统分配，
//! 无固定端口占用）。

use algeff_core::{DataOp, OpenFlags, ResourceRegistry, SysError, SyscallExecutor, Value};
use algeff_std::TokioExecutor;

/// 期望 execute 返回错误并取出（Ok 类型含非 Debug 的 UndoOp，不能 unwrap_err）。
async fn exec_err(ex: &mut TokioExecutor, reg: &mut ResourceRegistry, op: &DataOp) -> SysError {
    match ex.execute(op, reg).await {
        Err(e) => e,
        Ok(_) => panic!("期望错误，得到成功"),
    }
}

/// create_new（exclusive）撞已存在文件 → 两平台均 `AlreadyExists`
/// （修复前 Windows 经 `raw_os_error=80` → `Other(80)`，RFC-10 根因场景）。
#[tokio::test]
async fn create_new_existing_maps_to_already_exists() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("exists.txt");
    std::fs::write(&p, b"original").unwrap();

    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Open {
            path: p.clone(),
            flags: OpenFlags {
                write: true,
                create: true,
                exclusive: true,
                ..Default::default()
            },
        },
    )
    .await;
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "exclusive 撞已存在 → AlreadyExists（EEXIST，跨平台）"
    );
    assert_eq!(std::fs::read(&p).unwrap(), b"original", "失败不改动原文件");
    // 失败不分配 fd。
    assert!(reg.lookup(0).is_none(), "失败不分配句柄");
}

/// UDP 端口占用 → 两平台均 `Other(98)`（EADDRINUSE=98；修复前 Windows 为
/// Other(10048)）。UDP 面原登记「未测」，本测试补齐断言。
#[tokio::test]
async fn udp_bind_port_in_use_maps_to_eaddr_inuse() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    // 先绑 0 端口（系统分配），再绑同一地址 → 冲突（std/tokio bind 默认不设
    // SO_REUSEADDR，两平台均 EADDRINUSE）。
    let v = ex
        .execute(
            &DataOp::UdpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let fd = match v.0 {
        Value::Fd(f) => f,
        other => panic!("期望 Fd，得到 {other:?}"),
    };
    let addr = match reg.lookup(fd) {
        Some(algeff_core::ResourceHandle::UdpSocket(s)) => s.local_addr().unwrap(),
        _ => panic!("期望 UdpSocket 句柄"),
    };

    let e = exec_err(&mut ex, &mut reg, &DataOp::UdpBind { addr }).await;
    assert_eq!(
        e,
        SysError::Other(98),
        "端口占用 → EADDRINUSE(98)（14 错误集无 AddrInUse 变体，跨平台）"
    );
    // 清理：关闭首个 socket。
    ex.execute(&DataOp::Close { fd }, &mut reg).await.unwrap();
}

/// TCP 连接被拒 → 两平台均 `ConnectionRefused`（修复前 Windows 为 Other(10061)）。
#[tokio::test]
async fn tcp_connect_refused_maps_to_connection_refused() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    // 绑定临时端口后立即关闭 → 无监听 → 对端 RST → ECONNREFUSED（两平台一致）。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let e = exec_err(&mut ex, &mut reg, &DataOp::TcpConnect { addr }).await;
    assert_eq!(
        e,
        SysError::ConnectionRefused,
        "连接被拒 → ConnectionRefused（ECONNREFUSED=111，跨平台）"
    );
}
