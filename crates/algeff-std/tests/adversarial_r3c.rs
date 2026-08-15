//! R3c 对抗审计（第 3 轮 C 块：网络深度 + R2 已修回归）。
//!
//! 攻击方法论：与 R1/R2 相同——不 mock、全部经真实 `Runtime` +
//! `TokioExecutor` 全链路（`run_blocking` → `interpret` → 共享执行器通道），
//! 驱动方式全部为普通 `#[test]`（非 `#[tokio::test]`，D9：`Runtime::new` 与
//! `run_blocking` 须在 tokio 上下文之外调用）。R1/R2 已覆盖的面（可逆深链/
//! 游标、线性、并发 Fork、错误路径 put_back、值流、确定性、MutexLock 基本
//! 语义、Timeout 内 Fork、Mmap 边界等）**不重复**，本块只攻击：
//!
//! ## 攻击面 1：网络深度（真实 TCP/UDP）
//! 1a. **TcpAccept 循环**：同一 listener 顺序 accept 3 个连接，每个连接
//!     各自收发（echo）——accept 不得消费/毒化 listener fd；
//! 1b. **TcpShutdown 半关闭**：shutdown(Write) 后读半端仍可用（客户端可
//!     继续上行、服务端可读），写半端必须真实关闭（再写必须失败）——
//!     std 往返（into_std → shutdown → from_std）不得破坏半关闭语义；
//! 1c. **UdpBind + 无数据超时**：RecvFrom 无数据时被 `Action::Timeout`
//!     打断走 on_timeout；超时取消不毒化 socket（随后可正常收/发）；
//! 1d. **ConnectionReset 后同蓝图重连**：服务端立即关闭连接（客户端观察
//!     到 EOF/错误）→ 同一客户端蓝图对同一 listener 重连成功（错误不毒化）。
//!
//! ## 攻击面 2：R2 已修回归
//! 2a. **flaky flush 修复回归**：Write 后立即同步读 64 轮，与 R1 回归测试
//!     同思路但用**不同组合**——并行 Fork 双文件双写（压 blocking pool，
//!     原 flaky 根因场景），每轮结束后 Replace 撤销复位；
//! 2b. **嵌套 Fork 深层**：深度 6 非平衡形状，叶 fd 两两不相交 + 执行器
//!     映射读回正确；跨轮（两轮 16 fd）同样不相交；
//! 2c. **arbiter 争用混合超时压力**：8 并发分支 × 30 轮随机 Timeout 风暴
//!     （无声明争用 → 真并行 → 动态仲裁有限重试），取消路径（claim 守卫
//!     drop）与孤儿锁（recover 路径）都不造成**永久毒化**——风暴后
//!     recover + 同 id 重入成功；
//! 2d. **SendFile 文件目标写可见性**（R3c MEDIUM-1，D-039 对齐修复）：
//!     op_send_file 输出到文件曾无显式 flush（与 R1 flaky 同根因的异步
//!     落盘窗口）；executor 修复后 64 轮立即同步读必须观察到新内容。
//!
//! Windows 端口预算：全部测试合计约 7 个 TCP 连接 + 1 个 UDP 端口，远低于
//! 500 上限。

use std::net::{Shutdown, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceHandle, ResourceInner, ResourceUsage,
    Runtime, SysError, TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

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

/// List([Fd, Fd]) → (fd0, fd1)。
fn pair_of(v: &Value) -> (u64, u64) {
    match v {
        Value::List(l) if l.len() == 2 => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
    }
}

/// List([Fd, Addr])（TcpAccept 结果）→ (fd, peer_addr)。
fn accept_of(v: &Value) -> (u64, SocketAddr) {
    match v {
        Value::List(l) if l.len() == 2 => {
            let fd = fd_of(&l[0]);
            let addr = match &l[1] {
                Value::Addr(a) => *a,
                other => panic!("期望 Addr，得到 {other:?}"),
            };
            (fd, addr)
        }
        other => panic!("期望 List([Fd, Addr])，得到 {other:?}"),
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

/// Open(path, read-only) → Pure(Fd)。
fn open_pure(path: PathBuf) -> Action {
    syscall(
        DataOp::Open {
            path: path.clone(),
            flags: read_only_flags(),
        },
        vec![rd_path(path)],
        Action::Pure,
    )
}

/// Seek(0) → Read(len) → Pure(Bytes)。
fn seek_read_all(fd: u64, len: usize) -> Action {
    syscall(
        DataOp::Seek {
            fd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        vec![rd(fd)],
        move |_| syscall(DataOp::Read { fd, len }, vec![rd(fd)], Action::Pure),
    )
}

/// 经 Runtime 读回 fd 内容（executor 文件映射）。
fn read_back(rt: &mut Runtime, fd: u64, len: usize) -> Vec<u8> {
    match rt.run_blocking(seek_read_all(fd, len)) {
        Ok(Value::Bytes(b)) => b,
        other => panic!("fd {fd} 读回失败: {other:?}"),
    }
}

/// 断言 fd 列表两两不同且全部可 lookup。
fn assert_disjoint_lookupable(fds: &[u64], reg: &mut algeff_core::ResourceRegistry) {
    let mut uniq = std::collections::HashSet::new();
    for fd in fds {
        assert!(uniq.insert(*fd), "fd 重复: {fd}，全部: {fds:?}");
        assert!(reg.lookup(*fd).is_some(), "fd {fd} 句柄不可 lookup");
    }
    assert_eq!(uniq.len(), fds.len());
}

/// 循环 `TcpRead` 直至收满 expected 字节（TCP 流可能分片到达）。
fn tcp_read_exact(fd: u64, expected: usize, acc: Vec<u8>) -> Action {
    syscall(
        DataOp::TcpRead { fd, len: expected },
        vec![rd(fd)],
        move |v| match v {
            Value::Bytes(b) if b.is_empty() => {
                panic!("TcpRead 提前 EOF（仍缺 {expected} 字节）")
            }
            Value::Bytes(b) if b.len() == expected => {
                let mut all = acc;
                all.extend_from_slice(&b);
                Action::Pure(Value::Bytes(all))
            }
            Value::Bytes(b) => {
                let mut all = acc;
                all.extend_from_slice(&b);
                tcp_read_exact(fd, expected - b.len(), all)
            }
            other => panic!("期望 Bytes，得到 {other:?}"),
        },
    )
}

/// 通用 TCP 客户端线程（自带 tokio runtime，e2e.rs 模式）：
/// connect → write payload → read 恰好 expect 字节 → Ok(bytes)。
fn spawn_client(
    addr: SocketAddr,
    payload: Vec<u8>,
    expect: usize,
) -> std::thread::JoinHandle<Result<Vec<u8>, String>> {
    std::thread::spawn(move || {
        let crt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        crt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut s = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| e.to_string())?;
                s.write_all(&payload).await.map_err(|e| e.to_string())?;
                let mut buf = vec![0u8; expect];
                let mut got = 0usize;
                while got < expect {
                    let n = s.read(&mut buf[got..]).await.map_err(|e| e.to_string())?;
                    if n == 0 {
                        return Err(format!("EOF（读到 {got}/{expect}）"));
                    }
                    got += n;
                }
                Ok(buf)
            })
            .await
            .map_err(|_| "客户端 10s 超时".to_string())?
        })
    })
}

/// 连接终止探测客户端：write payload 后读一次，Ok(true) = 观察到终止
/// （EOF 或读错误），Ok(false) = 意外收到数据。
fn spawn_client_terminate_probe(
    addr: SocketAddr,
    payload: Vec<u8>,
) -> std::thread::JoinHandle<Result<bool, String>> {
    std::thread::spawn(move || {
        let crt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        crt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut s = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| e.to_string())?;
                s.write_all(&payload).await.map_err(|e| e.to_string())?;
                let mut buf = [0u8; 4];
                match s.read(&mut buf).await {
                    Ok(0) => Ok(true),  // EOF（服务端关闭）
                    Err(_) => Ok(true), // ConnectionReset/其他读错误
                    Ok(_) => Ok(false), // 意外收到数据
                }
            })
            .await
            .map_err(|_| "客户端 10s 超时".to_string())?
        })
    })
}

/// 半关闭探测客户端：读 4 字节（期望 "PING"）→ 再读一次（期望 EOF）→
/// 上行 "pong"。返回 (收到的前 4 字节, 是否观察到 EOF)。
fn spawn_client_halfclose(
    addr: SocketAddr,
) -> std::thread::JoinHandle<Result<(Vec<u8>, bool), String>> {
    std::thread::spawn(move || {
        let crt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
        crt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut s = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut ping = [0u8; 4];
                s.read_exact(&mut ping).await.map_err(|e| e.to_string())?;
                let mut probe = [0u8; 4];
                let eof = match s.read(&mut probe).await {
                    Ok(0) => true,
                    Err(_) => true,
                    Ok(_) => false,
                };
                s.write_all(b"pong").await.map_err(|e| e.to_string())?;
                Ok((ping.to_vec(), eof))
            })
            .await
            .map_err(|_| "客户端 10s 超时".to_string())?
        })
    })
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1a：TcpAccept 循环 —— 同一 listener 顺序 accept 3 个连接，每个
// 连接各自收发（echo）。accept 是 &self 型操作（reg.lookup 直存句柄），
// 不得消费 listener；三次循环证明 listener fd 全程可用、无状态毒化。
// ══════════════════════════════════════════════════════════════════════

/// 服务端链：对同一 listener 连续 accept `remaining` 次，每次读 4 字节、
/// 原样 echo、shutdown(Both)、关闭连接，最后返回 U64(3)。
fn accept_echo_loop(listener: u64, remaining: usize) -> Action {
    if remaining == 0 {
        return Action::Pure(Value::U64(3));
    }
    syscall(
        DataOp::TcpAccept { listener },
        vec![rd(listener)],
        move |v| {
            let (sfd, _peer) = accept_of(&v);
            Action::Sequential {
                current: Box::new(tcp_read_exact(sfd, 4, Vec::new())),
                next: Box::new(move |v| {
                    let msg = match v {
                        Value::Bytes(b) => b,
                        other => panic!("期望 Bytes，得到 {other:?}"),
                    };
                    syscall(
                        DataOp::TcpWrite {
                            fd: sfd,
                            data: msg.clone(),
                        },
                        vec![wr(sfd)],
                        move |_| {
                            syscall(
                                DataOp::TcpShutdown {
                                    fd: sfd,
                                    how: Shutdown::Both,
                                },
                                vec![rd(sfd)],
                                move |_| {
                                    syscall(DataOp::Close { fd: sfd }, vec![ow(sfd)], move |_| {
                                        accept_echo_loop(listener, remaining - 1)
                                    })
                                },
                            )
                        },
                    )
                }),
            }
        },
    )
}

#[test]
fn net_tcp_accept_loop_3_connections_each_echo() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let lfd = fd_of(&v);
    let addr: SocketAddr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener，得到 {other:?}"),
    };

    // 3 个客户端并发连接（backlog 排队），各自发送 iiii、期望收到原样 echo。
    let msgs: Vec<Vec<u8>> = (0..3u8).map(|i| vec![b'0' + i; 4]).collect();
    let clients: Vec<_> = msgs
        .iter()
        .map(|m| spawn_client(addr, m.clone(), 4))
        .collect();

    // 服务端：同一 listener 顺序 accept 3 次，每次完整收发后关闭连接。
    let v = rt.run_blocking(accept_echo_loop(lfd, 3)).unwrap();
    assert_eq!(v, Value::U64(3), "三次 accept 全部完成");

    for (c, m) in clients.into_iter().zip(msgs.iter()) {
        let echo = c.join().unwrap().unwrap();
        assert_eq!(&echo, m, "每个连接收到自己的 echo（accept 循环不串线）");
    }

    // listener 未被 accept 消费：仍可 lookup，随后正常 Close。
    assert!(
        rt.registry().lookup(lfd).is_some(),
        "三次 accept 后 listener fd 仍可寻址"
    );
    rt.run_blocking(syscall(
        DataOp::Close { fd: lfd },
        vec![ow(lfd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "网络 ops 不产生 undo");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1b：TcpShutdown 半关闭 —— shutdown(Write) 后读半端仍可用、写半端
// 真实关闭（再写失败）。op_tcp_shutdown 走 std 往返
// （into_std → set_nonblocking → shutdown → from_std），该往返不得破坏
// 半关闭语义、不得吞句柄（put_back 轮换后逻辑 fd 仍可用）。
// ══════════════════════════════════════════════════════════════════════

/// 服务端链：accept → 写 "PING" → shutdown(Write) → 读 4 字节（期望客户端
/// 在 EOF 后上行 "pong"）→ 返回 List([sfd, Bytes("pong")])。
fn half_close_server_chain(listener: u64) -> Action {
    syscall(
        DataOp::TcpAccept { listener },
        vec![rd(listener)],
        move |v| {
            let (sfd, _peer) = accept_of(&v);
            syscall(
                DataOp::TcpWrite {
                    fd: sfd,
                    data: b"PING".to_vec(),
                },
                vec![wr(sfd)],
                move |_| {
                    syscall(
                        DataOp::TcpShutdown {
                            fd: sfd,
                            how: Shutdown::Write,
                        },
                        vec![rd(sfd)],
                        move |_| {
                            syscall(
                                DataOp::TcpRead { fd: sfd, len: 4 },
                                vec![rd(sfd)],
                                move |v| {
                                    let got = match v {
                                        Value::Bytes(b) => b,
                                        other => panic!("期望 Bytes，得到 {other:?}"),
                                    };
                                    Action::Pure(Value::List(vec![
                                        Value::Fd(sfd),
                                        Value::Bytes(got),
                                    ]))
                                },
                            )
                        },
                    )
                },
            )
        },
    )
}

#[test]
fn net_tcp_shutdown_half_close_read_alive_write_fails() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let lfd = fd_of(&v);
    let addr: SocketAddr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener，得到 {other:?}"),
    };

    let client = spawn_client_halfclose(addr);
    let v = rt.run_blocking(half_close_server_chain(lfd)).unwrap();
    let (sfd, pong) = match &v {
        Value::List(l) if l.len() == 2 => (
            fd_of(&l[0]),
            match &l[1] {
                Value::Bytes(b) => b.clone(),
                other => panic!("期望 Bytes，得到 {other:?}"),
            },
        ),
        other => panic!("期望 List([Fd, Bytes])，得到 {other:?}"),
    };
    assert_eq!(
        pong, b"pong",
        "shutdown(Write) 后读半端仍可用（收到客户端上行）"
    );
    let (ping, eof) = client.join().unwrap().unwrap();
    assert_eq!(ping, b"PING", "客户端收到 shutdown(Write) 前的数据");
    assert!(eof, "客户端在 PING 之后观察到 EOF（FIN 已发送）");

    // 写半端必须真实关闭：shutdown(Write) 后再写必须失败。资源声明留空
    // （pdr §18 用户责任边界）——绕过 A4 线性拦截，把错误来源隔离到 socket 层
    // （链内首个 TcpWrite 已消费 wr(sfd)，再声明 Write 会被 A4 拒绝）。
    let e = rt
        .run_blocking(syscall(
            DataOp::TcpWrite {
                fd: sfd,
                data: b"PONG".to_vec(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap_err();
    eprintln!("R3C 半关闭：shutdown(Write) 后写错误 = {e:?}");

    // put_back 轮换未吞句柄：全部 fd 正常 Close。
    rt.run_blocking(syscall(
        DataOp::Close { fd: sfd },
        vec![ow(sfd)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Close { fd: lfd },
        vec![ow(lfd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1c：UdpBind + RecvFrom 无数据超时 —— `Action::Timeout` 包裹
// `UdpRecvFrom`，无数据时走 on_timeout；超时取消（tokio timeout 丢弃内层
// future）不得毒化 socket——随后 RecvFrom 与 UdpSendTo 均正常。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn net_udp_recv_timeout_no_data_then_socket_alive() {
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
    let ufd = fd_of(&v);
    let uaddr: SocketAddr = match rt.registry().lookup(ufd).unwrap() {
        ResourceHandle::UdpSocket(s) => s.local_addr().unwrap(),
        other => panic!("期望 UdpSocket，得到 {other:?}"),
    };

    // 1) 无数据 → Timeout 打断 → on_timeout(42)。
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(syscall(
                DataOp::UdpRecvFrom { fd: ufd, len: 64 },
                vec![rd(ufd)],
                Action::Pure,
            )),
            duration: Duration::from_millis(200),
            on_timeout: Box::new(Action::Pure(Value::U64(42))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(42), "无数据时 RecvFrom 应被 Timeout 打断");

    // 2) 外部持续发送，轮询 RecvFrom：超时取消（含潜在的非取消安全窗口）
    //    不得毒化 socket——至少收到一个数据报。
    let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut got = false;
    for _ in 0..30 {
        let _ = probe.send_to(b"hello-udp", uaddr);
        let v = rt
            .run_blocking(Action::Timeout {
                action: Box::new(syscall(
                    DataOp::UdpRecvFrom { fd: ufd, len: 64 },
                    vec![rd(ufd)],
                    Action::Pure,
                )),
                duration: Duration::from_millis(100),
                on_timeout: Box::new(Action::Pure(Value::Unit)),
            })
            .unwrap();
        if let Value::List(l) = v {
            if l[0] == Value::Bytes(b"hello-udp".to_vec()) {
                got = true;
                break;
            }
        }
    }
    assert!(got, "超时取消后 socket 仍可 RecvFrom（数据报被接收）");

    // 3) 发送方向不受影响：UdpSendTo 到外部 socket 可收到。
    let sink = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let sink_addr = sink.local_addr().unwrap();
    rt.run_blocking(syscall(
        DataOp::UdpSendTo {
            fd: ufd,
            data: b"ping".to_vec(),
            addr: sink_addr,
        },
        vec![wr(ufd)],
        Action::Pure,
    ))
    .unwrap();
    let mut buf = [0u8; 8];
    let (n, _) = sink.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"ping", "UdpSendTo 在超时取消后仍正常");

    rt.run_blocking(syscall(
        DataOp::Close { fd: ufd },
        vec![ow(ufd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1d：ConnectionReset（连接被服务端立即关闭）后同蓝图重连成功 ——
// 错误不毒化执行器/registry：undo 空、listener 仍可寻址、第二次连接走
// 同一条客户端蓝图（connect → write → read）正常拿到回包。
// ══════════════════════════════════════════════════════════════════════

/// 第一次连接（错误路径）：accept → 读 4 字节 → shutdown(Both) + close
/// （不回包）→ 客户端观察到连接终止。
fn reset_server_first(listener: u64) -> Action {
    syscall(
        DataOp::TcpAccept { listener },
        vec![rd(listener)],
        move |v| {
            let (sfd, _peer) = accept_of(&v);
            Action::Sequential {
                current: Box::new(tcp_read_exact(sfd, 4, Vec::new())),
                next: Box::new(move |_| {
                    syscall(
                        DataOp::TcpShutdown {
                            fd: sfd,
                            how: Shutdown::Both,
                        },
                        vec![rd(sfd)],
                        move |_| syscall(DataOp::Close { fd: sfd }, vec![ow(sfd)], Action::Pure),
                    )
                }),
            }
        },
    )
}

/// 第二次连接（正常路径）：accept → 读 4 字节 → echo "REP2" → 关闭。
fn reset_server_second(listener: u64) -> Action {
    syscall(
        DataOp::TcpAccept { listener },
        vec![rd(listener)],
        move |v| {
            let (sfd, _peer) = accept_of(&v);
            Action::Sequential {
                current: Box::new(tcp_read_exact(sfd, 4, Vec::new())),
                next: Box::new(move |_| {
                    syscall(
                        DataOp::TcpWrite {
                            fd: sfd,
                            data: b"REP2".to_vec(),
                        },
                        vec![wr(sfd)],
                        move |_| {
                            syscall(
                                DataOp::TcpShutdown {
                                    fd: sfd,
                                    how: Shutdown::Both,
                                },
                                vec![rd(sfd)],
                                move |_| {
                                    syscall(DataOp::Close { fd: sfd }, vec![ow(sfd)], Action::Pure)
                                },
                            )
                        },
                    )
                }),
            }
        },
    )
}

#[test]
fn net_conn_reset_reconnect_same_blueprint_succeeds() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let lfd = fd_of(&v);
    let addr: SocketAddr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener，得到 {other:?}"),
    };

    // 连接 1：服务端立即关闭 → 客户端观察到终止（EOF/错误）。
    let c1 = spawn_client_terminate_probe(addr, b"req1".to_vec());
    rt.run_blocking(reset_server_first(lfd)).unwrap();
    let terminated = c1.join().unwrap().unwrap();
    assert!(terminated, "连接被关闭后客户端必须观察到终止信号");
    assert!(rt.undo_stack().is_empty(), "reset 路径不产生残留 undo");
    assert!(
        rt.registry().lookup(lfd).is_some(),
        "错误路径后 listener 仍可寻址"
    );

    // 连接 2：同一客户端蓝图（connect → write → read echo）重连成功。
    let c2 = spawn_client(addr, b"req2".to_vec(), 4);
    rt.run_blocking(reset_server_second(lfd)).unwrap();
    let echo = c2.join().unwrap().unwrap();
    assert_eq!(
        echo,
        b"REP2".to_vec(),
        "错误不毒化：同蓝图重连成功且收到回包"
    );

    rt.run_blocking(syscall(
        DataOp::Close { fd: lfd },
        vec![ow(lfd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2a：R2 flaky flush 修复回归 —— 并行 Fork 双文件双写 64 轮 + 每轮
// 立即同步读。与 R1 回归（单文件顺序 64 轮）不同组合：双路写并发压
// blocking pool（原 flaky 根因场景：write_all 异步落盘，flush 前窗口读旧
// 内容），修复后 Write op 返回 ⇔ OS 已落盘（A4/A6 可观察性契约）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn r2_flush_regression_parallel_double_write_64_rounds() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r3c-fl-a.txt");
    let pb = dir.path().join("r3c-fl-b.txt");
    std::fs::write(&pa, b"0000000000").unwrap();
    std::fs::write(&pb, b"0000000000").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    for i in 0..64u8 {
        let pay_a = [b'A' + (i % 26); 4];
        let pay_b = [b'B' + (((i as u32 * 7) % 26) as u8); 4];
        // RFC-05：Replace 使旧 fd 失效（registry 活性唯一真相），每轮重开
        // 双文件新 fd（顺带覆盖 D10「Replace 后同路径重开正常」）。
        let pb_in = pb.clone();
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(pa.clone())],
                move |v| {
                    let fda = fd_of(&v);
                    syscall(
                        DataOp::Open {
                            path: pb_in.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(pb_in.clone())],
                        move |v| Action::Pure(Value::List(vec![Value::Fd(fda), v])),
                    )
                },
            ))
            .unwrap();
        let (fda, fdb) = pair_of(&v);
        // 并行 Fork：左右分支各自 Seek(0)+Write 不同文件（资源不相交 → 真
        // 并行，两路 OS 写并发压 blocking pool —— R1 单写顺序组合的加强版）。
        rt.run_blocking(Action::Fork {
            left: Box::new(syscall(
                DataOp::Seek {
                    fd: fda,
                    offset: 0,
                    whence: std::io::SeekFrom::Start(0),
                },
                vec![rd(fda)],
                move |_| {
                    syscall(
                        DataOp::Write {
                            fd: fda,
                            data: pay_a.to_vec(),
                        },
                        vec![wr(fda)],
                        Action::Pure,
                    )
                },
            )),
            right: Box::new(syscall(
                DataOp::Seek {
                    fd: fdb,
                    offset: 0,
                    whence: std::io::SeekFrom::Start(0),
                },
                vec![rd(fdb)],
                move |_| {
                    syscall(
                        DataOp::Write {
                            fd: fdb,
                            data: pay_b.to_vec(),
                        },
                        vec![wr(fdb)],
                        Action::Pure,
                    )
                },
            )),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap();

        // Write op 返回后立即同步读两个文件——修复前任一侧可能读到旧内容。
        let got_a = std::fs::read(&pa).unwrap();
        let got_b = std::fs::read(&pb).unwrap();
        assert_eq!(
            &got_a[0..4],
            &pay_a[..],
            "第 {i} 轮：a 文件 Write 效果必须立即可观察（并行路径）"
        );
        assert_eq!(
            &got_b[0..4],
            &pay_b[..],
            "第 {i} 轮：b 文件 Write 效果必须立即可观察（并行路径）"
        );

        // Replace：撤销两分支 Write（内容+游标恢复）+ 清 A4 标记；旧 fd 随之
        // 失效（RFC-05），下一轮重开新 fd 复用同一文件。
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap();
        assert_eq!(
            std::fs::read(&pa).unwrap(),
            b"0000000000",
            "第 {i} 轮：a 撤销恢复"
        );
        assert_eq!(
            std::fs::read(&pb).unwrap(),
            b"0000000000",
            "第 {i} 轮：b 撤销恢复"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2b：嵌套 Fork 深层（深度 6，非平衡）—— 叶 fd 两两不相交 + 执行器
// 映射读回正确；同一 Runtime 两轮（16 个 fd）跨轮同样不相交（D1 单调 +
// 嵌套区间不污染父游标）。R2 覆盖深度 5 平衡/不规则 11 叶，本测试用深度 6
// 非平衡 8 叶 + 跨轮断言，攻击更深路径。
// ══════════════════════════════════════════════════════════════════════

enum Shape {
    Leaf,
    Fork(Box<Shape>, Box<Shape>),
}

/// 非平衡形状：右侧深链 + 局部双子。深度 6、8 叶、7 个 Fork 节点：
/// F( l0, F( l1, F( F(l2,l3), F( l4, F( F(l5,l6), l7 ) ) ) ) )
fn deep_unbalanced_shape() -> Shape {
    Shape::Fork(
        Box::new(Shape::Leaf),
        Box::new(Shape::Fork(
            Box::new(Shape::Leaf),
            Box::new(Shape::Fork(
                Box::new(Shape::Fork(Box::new(Shape::Leaf), Box::new(Shape::Leaf))),
                Box::new(Shape::Fork(
                    Box::new(Shape::Leaf),
                    Box::new(Shape::Fork(
                        Box::new(Shape::Fork(Box::new(Shape::Leaf), Box::new(Shape::Leaf))),
                        Box::new(Shape::Leaf),
                    )),
                )),
            )),
        )),
    )
}

/// 形状 → Action：DFS 分配叶路径（全树叶路径互异 → 所有 Fork 均并行），
/// combine 逐层 List 拼接（保持 DFS 序）。
fn shape_to_action(shape: &Shape, files: &[PathBuf], next: &mut usize) -> Action {
    match shape {
        Shape::Leaf => {
            let p = files[*next].clone();
            *next += 1;
            open_pure(p)
        }
        Shape::Fork(l, r) => Action::Fork {
            left: Box::new(shape_to_action(l, files, next)),
            right: Box::new(shape_to_action(r, files, next)),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        },
    }
}

/// DFS 展平 combine 结果，收集叶 Fd（保持叶序）。
fn flatten_fds(v: &Value, out: &mut Vec<u64>) {
    match v {
        Value::Fd(f) => out.push(*f),
        Value::List(l) => {
            for x in l {
                flatten_fds(x, out);
            }
        }
        other => panic!("期望 Fd/List，得到 {other:?}"),
    }
}

#[test]
fn r2_fork_depth6_unbalanced_fds_disjoint_and_readback() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut contents = Vec::new();
    for i in 0..8u8 {
        let p = dir.path().join(format!("d6-{i}.txt"));
        let c = format!("depth6-{i:02}").into_bytes();
        std::fs::write(&p, &c).unwrap();
        files.push(p);
        contents.push(c);
    }
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut all_fds = Vec::new();

    for round in 0..2 {
        let mut next = 0usize;
        let action = shape_to_action(&deep_unbalanced_shape(), &files, &mut next);
        assert_eq!(next, 8, "8 个叶");
        let v = rt.run_blocking(action).unwrap();
        let mut fds = Vec::new();
        flatten_fds(&v, &mut fds);
        assert_eq!(fds.len(), 8, "全部叶 fd 到达 combine");
        assert_disjoint_lookupable(&fds, rt.registry());
        for (fd, content) in fds.iter().zip(contents.iter()) {
            let got = read_back(&mut rt, *fd, content.len());
            assert_eq!(
                &got, content,
                "第 {round} 轮 fd {fd} 应指向对应文件内容（深度 6 嵌套下映射未被并发覆盖）"
            );
        }
        all_fds.extend(fds);
    }
    // 跨轮：16 个 fd 全部两两不相交（嵌套区间不污染父游标、D1 单调不复用）。
    assert_disjoint_lookupable(&all_fds, rt.registry());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2c：arbiter 争用混合超时压力 —— 8 并发分支 × 30 轮随机 Timeout
// 风暴。分支**不声明**锁资源（pdr §18 用户责任边界）→ 静态层放行 → 真
// 并行 → 动态仲裁（8×1ms 有限重试 + WouldBlock 快速失败）。随机 Timeout
// 覆盖两类取消窗口：
//   (a) 锁获取中取消 → ArbiterClaimGuard drop 自动 release（R2 批 8/批 9
//       修复面）——不得泄漏占坑；
//   (b) 持锁睡眠中取消 → 孤儿锁（undo 已入分支栈，fork 完成时合并回父，
//       recover 路径可释放，R2 孤儿副作用同类可恢复子类）——不得永久毒化。
// 关键回归断言：风暴后 Replace（recover）→ 同 id 重入成功（无永久
// WouldBlock）；全部轮次有明确结果（无悬挂/丢失/意外错误）。
// ══════════════════════════════════════════════════════════════════════

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

/// 一轮：Timeout(1..=3ms) 包裹 MutexLock → Sleep(1..=5ms) → MutexUnlock。
/// 结果 U64：0=完整完成, 1=WouldBlock, 2=timeout, 3=其他错误。
fn arb_round(id: u64, dur_ms: u64, sleep_ms: u64) -> Action {
    Action::Catch {
        action: Box::new(Action::Timeout {
            action: Box::new(Action::Sequential {
                current: Box::new(syscall(DataOp::MutexLock { id }, vec![], Action::Pure)),
                next: Box::new(move |_| Action::Sequential {
                    current: Box::new(Action::Sleep {
                        duration: Duration::from_millis(sleep_ms),
                        next: Box::new(Action::Pure),
                    }),
                    next: Box::new(move |_| {
                        // 完成路径显式产出 U64(0)（op_mutex_unlock 返回 Unit，
                        // go 延续按 U64 匹配——修复预存 flaky：完整完成轮 panic 期望 U64）
                        syscall(DataOp::MutexUnlock { id }, vec![], |_| {
                            Action::Pure(Value::U64(0))
                        })
                    }),
                }),
            }),
            duration: Duration::from_millis(dur_ms),
            on_timeout: Box::new(Action::Pure(Value::U64(2))),
        }),
        handler: Box::new(|e| {
            Action::Pure(Value::U64(if e == SysError::WouldBlock { 1 } else { 3 }))
        }),
    }
}

/// 分支：rounds 轮累计 [ok, blocked, timed_out, err] 计数。
fn arb_branch(id: u64, rounds: usize, seed: u64) -> Action {
    fn go(id: u64, remaining: usize, acc: [u64; 4], seed: u64) -> Action {
        if remaining == 0 {
            return Action::Pure(Value::List(acc.iter().map(|&x| Value::U64(x)).collect()));
        }
        let s1 = xorshift(seed);
        let dur_ms = 1 + s1 % 3; // 1..=3
        let s2 = xorshift(s1);
        let sleep_ms = 1 + s2 % 5; // 1..=5
        Action::Sequential {
            current: Box::new(arb_round(id, dur_ms, sleep_ms)),
            next: Box::new(move |v| {
                let idx = match v {
                    Value::U64(x) => x as usize,
                    other => panic!("期望 U64，得到 {other:?}"),
                };
                let mut a = acc;
                a[idx] += 1;
                go(id, remaining - 1, a, s2)
            }),
        }
    }
    go(id, rounds, [0, 0, 0, 0], seed)
}

/// 两个 4 元计数 List 逐元素相加。
fn add_count_lists(l: Value, r: Value) -> Value {
    let (a, b) = match (l, r) {
        (Value::List(a), Value::List(b)) => {
            let mut av = Vec::new();
            let mut bv = Vec::new();
            for x in a {
                match x {
                    Value::U64(v) => av.push(v),
                    other => panic!("期望 U64，得到 {other:?}"),
                }
            }
            for x in b {
                match x {
                    Value::U64(v) => bv.push(v),
                    other => panic!("期望 U64，得到 {other:?}"),
                }
            }
            (av, bv)
        }
        other => panic!("期望 List，得到 {other:?}"),
    };
    assert_eq!(a.len(), 4);
    assert_eq!(b.len(), 4);
    Value::List(
        a.iter()
            .zip(b.iter())
            .map(|(x, y)| Value::U64(x + y))
            .collect(),
    )
}

/// N 路并行 Fork（平衡合并，combine 相加计数）。分支资源全空 → 无冲突 →
/// 真并行。
fn fork_n(branches: Vec<Action>) -> Action {
    fn rec(mut actions: Vec<Action>) -> Action {
        if actions.len() == 1 {
            return actions.pop().unwrap();
        }
        // Action 非 Clone：经 split_off 所有权切分（不可索引切片克隆）。
        let mid = actions.len() / 2;
        let right = actions.split_off(mid);
        let l = rec(actions);
        let r = rec(right);
        Action::Fork {
            left: Box::new(l),
            right: Box::new(r),
            combine: Box::new(|lv, rv| Action::Pure(add_count_lists(lv, rv))),
        }
    }
    rec(branches)
}

#[test]
fn r2_arbiter_8x30_mixed_timeout_storm_no_poison() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut branches = Vec::new();
    for b in 0..8 {
        branches.push(arb_branch(42, 30, 0x9E37_79B9_7F4A_7C15 ^ (b as u64 + 1)));
    }
    let v = rt.run_blocking(fork_n(branches)).unwrap();
    let counts: Vec<u64> = match v {
        Value::List(l) => l
            .into_iter()
            .map(|x| match x {
                Value::U64(u) => u,
                other => panic!("期望 U64，得到 {other:?}"),
            })
            .collect(),
        other => panic!("期望 List，得到 {other:?}"),
    };
    assert_eq!(counts.len(), 4);
    let (ok, blocked, timed_out, err) = (counts[0], counts[1], counts[2], counts[3]);
    eprintln!(
        "R3C arbiter 风暴：ok={ok} blocked={blocked} timed_out={timed_out} err={err} undo_len={}",
        rt.undo_stack().len()
    );
    assert_eq!(
        ok + blocked + timed_out + err,
        8 * 30,
        "全部轮次有明确结果（无悬挂/丢失）"
    );
    assert_eq!(err, 0, "不允许出现 WouldBlock 之外的错误");
    assert!(timed_out >= 1, "超时压力必须实际触发（随机 Timeout 生效）");
    assert!(
        rt.undo_stack().len() <= 8 * 30,
        "undo 栈无爆涨（≤ 成功获取锁的轮次数）"
    );

    // 关键回归：风暴（含取消路径）后 recover 可完全释放——锁 id 无永久毒化。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "recover 后 undo 栈空");
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 42 },
        vec![rd(42)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::MutexUnlock { id: 42 },
        vec![rd(42)],
        Action::Pure,
    ))
    .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2d：SendFile → 文件目标 写可见性（R3c MEDIUM-1，D-039 对齐修复）。
// op_send_file 输出到文件此前走 `g.write(&buf)` 且**无显式 flush** —— 与
// R1 flaky 根因（write_all 异步落盘）同一类窗口：SendFile op 返回后立即
// 同步读可观察到旧内容。修复（executor.rs op_send_file 文件路径补 flush）
// 后本测试升级为严格回归：64 轮每轮重写源文件 → SendFile 拷贝 → 立即同步
// 读目标尾部，必须观察到新内容（失败即断言，R1 同款 fail-fast 风格）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn r2_sendfile_file_target_visibility() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("sf-src.txt");
    let dst = dir.path().join("sf-dst.txt");
    std::fs::write(&src, b"0000000000").unwrap();
    std::fs::write(&dst, b"0000000000").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    for i in 0..64u8 {
        let payload = [b'P' + (i % 26); 4];
        // 外部重写源文件（模拟输入侧更新），SendFile 拷贝前 4 字节到目标。
        std::fs::write(&src, payload).unwrap();
        // RFC-05：Replace 使旧 fd 失效（registry 活性唯一真相），每轮重开
        // src/dst 新 fd；dst 显式 Seek 到本轮落点 [4i, 4i+4)（fresh fd 游标 0）。
        let dst_in = dst.clone();
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: src.clone(),
                    flags: read_only_flags(),
                },
                vec![rd_path(src.clone())],
                move |v| {
                    let sfd = fd_of(&v);
                    syscall(
                        DataOp::Open {
                            path: dst_in.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(dst_in.clone())],
                        move |v| Action::Pure(Value::List(vec![Value::Fd(sfd), v])),
                    )
                },
            ))
            .unwrap();
        let (sfd, dfd) = pair_of(&v);
        let off = 4 * i as usize;
        rt.run_blocking(syscall(
            DataOp::Seek {
                fd: dfd,
                offset: off as i64,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![rd(dfd)],
            Action::Pure,
        ))
        .unwrap();
        rt.run_blocking(syscall(
            DataOp::SendFile {
                out: dfd,
                input: sfd,
                offset: 0,
                len: 4,
            },
            vec![rd(sfd), wr(dfd)],
            Action::Pure,
        ))
        .unwrap();
        // SendFile op 返回后不得依赖任何中间操作兜底——立即同步读目标尾部
        // （第 i 次拷贝落在 [4i, 4i+4)）必须可见新内容；文件长度不足（OS 写
        // 未落盘、文件尚未伸长）同样视为旧内容。
        let got = std::fs::read(&dst).unwrap();
        assert!(
            got.len() >= off + 4 && got[off..off + 4] == payload[..],
            "第 {i} 轮：SendFile op 完成后新内容必须立即可观察（D-039 对齐；\
             修复前无 flush 时此处读到旧内容或文件未伸长）"
        );
        // 复位 A4（wr(dfd) 每轮至多一次）；旧 fd 随之失效（RFC-05），下一轮
        // 重开新 fd 复用同一文件。
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap();
    }
}
