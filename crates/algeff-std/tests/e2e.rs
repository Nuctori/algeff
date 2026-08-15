//! A5 批 2：真实端到端集成测试（不经 mock）——完整 interpret 链路
//! （`Runtime` + `TokioExecutor`，pdr.md §14 / contracts.md D9/D10/D14）。
//!
//! 驱动方式：全部为普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用（Runtime 自持
//! reactor）。recoverΓ 经 interpret 的 `Replace` 节点触发（D10：先 recover
//! 再执行 target），撤销交互保持在全 interpret 路径内。
//! 注意：`Box<dyn SyscallExecutor>` 非 Send，`Runtime` 不能跨线程移动——
//! TCP 测试的服务端 interpret 在主线程运行，tokio 原生客户端在线程内自建
//! 独立 tokio runtime。

use std::net::{Shutdown, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, PipeFlags, ReadOnly, ResourceHandle, ResourceInner,
    ResourceUsage, Runtime, SysError, TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── 本地辅助（adapters.rs 内同名私有辅助的复制；src/ 冻结不可改）──────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(fd) => *fd,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

/// 构造单个 Syscall 节点（next 为 CPS 延续）。
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

// ── a. 文件写读 + undo：interpret 全链路，recover 后内容恢复原样 ───────

#[test]
fn e2e_file_write_read_undo() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("undo.txt");
    let original: Vec<u8> = b"original content".to_vec();
    let orig_len = original.len();
    std::fs::write(&path, &original).unwrap();

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // Open → Write（小文件 Full 撤销：写前读 → undo 压栈；fd 经 CPS 贯穿）。
    let flags = OpenFlags {
        read: true,
        write: true,
        ..Default::default()
    };
    let fd_v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags,
            },
            vec![wr_path(path.clone())],
            move |v| {
                let fd = fd_of(&v);
                syscall(
                    DataOp::Write {
                        fd,
                        data: b"changed content!".to_vec(),
                    },
                    vec![wr(fd)],
                    move |_| Action::Pure(Value::Fd(fd)),
                )
            },
        ))
        .unwrap();
    let fd = fd_of(&fd_v);

    // Write 已生效，undo 已压栈（<1MB 文件 → Full 撤销策略）。
    assert_eq!(rt.undo_stack().len(), 1);
    assert_eq!(std::fs::read(&path).unwrap(), b"changed content!");

    // recover：经 interpret 的 Replace（D10：先 recover 再执行 target）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();

    // 文件内容恢复原样；撤销栈清空；registry 句柄仍在，且可继续 interpret 读回。
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(rt.undo_stack().is_empty());
    // D10（A2 批 4 对齐）：Replace = recover + reg.clear() —— 句柄与线性标记全部放弃。
    assert!(
        rt.registry().lookup(fd).is_none(),
        "Replace 清空 registry 句柄"
    );
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags,
            },
            vec![wr_path(path.clone())],
            move |v| {
                let fd2 = fd_of(&v);
                syscall(
                    DataOp::Seek {
                        fd: fd2,
                        offset: 0,
                        whence: std::io::SeekFrom::Start(0),
                    },
                    vec![rd(fd2)],
                    move |_| {
                        syscall(
                            DataOp::Read {
                                fd: fd2,
                                len: orig_len,
                            },
                            vec![rd(fd2)],
                            Action::Pure,
                        )
                    },
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(original));
}

// ── b. TCP echo 服务器：interpret 全链路 + tokio 原生客户端 ────────────

/// 循环 `TcpRead` 直至收满 expected 字节（TCP 流可能分片到达）。
fn tcp_read_all(fd: u64, expected: usize, acc: Vec<u8>) -> Action {
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
                tcp_read_all(fd, expected - b.len(), all)
            }
            other => panic!("期望 Bytes，得到 {other:?}"),
        },
    )
}

/// 服务端蓝图：TcpAccept → TcpRead(循环收满) → TcpWrite(echo) → TcpShutdown。
fn echo_server_chain(listener: u64, expected: usize) -> Action {
    syscall(
        DataOp::TcpAccept { listener },
        vec![rd(listener)],
        move |v| {
            let sfd = match v {
                Value::List(l) => fd_of(&l[0]),
                other => panic!("期望 List([Fd, Addr])，得到 {other:?}"),
            };
            Action::Sequential {
                current: Box::new(tcp_read_all(sfd, expected, Vec::new())),
                next: Box::new(move |v| {
                    let echo = match v {
                        Value::Bytes(b) => b,
                        other => panic!("期望 Bytes，得到 {other:?}"),
                    };
                    syscall(
                        DataOp::TcpWrite {
                            fd: sfd,
                            data: echo.clone(),
                        },
                        vec![wr(sfd)],
                        move |_| {
                            syscall(
                                DataOp::TcpShutdown {
                                    fd: sfd,
                                    how: Shutdown::Both,
                                },
                                vec![ow(sfd)],
                                move |_| Action::Pure(Value::U64(echo.len() as u64)),
                            )
                        },
                    )
                }),
            }
        },
    )
}

#[test]
fn e2e_tcp_echo_server() {
    // 1KB 非平凡 payload（避免全零巧合与单字节模式）。
    let payload: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // 先绑定（127.0.0.1:0 → 内核分配端口），从 registry 取回真实地址。
    let lfd = rt
        .run_blocking(syscall(
            DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let lfd = fd_of(&lfd);
    let addr: SocketAddr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener 句柄，得到 {other:?}"),
    };
    let n = payload.len();

    // 客户端线程：tokio 原生 TcpStream 连接并收发 1KB（10s 超时防悬挂）。
    let client_payload = payload.clone();
    let client = std::thread::spawn(move || {
        let client_rt = tokio::runtime::Runtime::new().unwrap();
        client_rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
                s.write_all(&client_payload).await.unwrap();
                let mut buf = vec![0u8; client_payload.len()];
                s.read_exact(&mut buf).await.unwrap();
                buf
            })
            .await
            .expect("客户端连接/收发 10s 超时")
        })
    });

    // 服务端：interpret 全链路（Accept→Read→Write→Shutdown）在主线程运行。
    let v = rt.run_blocking(echo_server_chain(lfd, n)).unwrap();

    let echoed = client.join().unwrap();
    assert_eq!(v, Value::U64(n as u64));
    assert_eq!(echoed, payload);
}

// ── c. 管道双工：PipeOpen → writer 写 → reader 读，经 interpret ────────

#[test]
fn e2e_pipe_duplex() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let payload: Vec<u8> = b"duplex payload via interpret".to_vec();
    let expect = payload.clone();

    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = match v {
                    Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
                    other => panic!("期望 List([rfd, wfd])，得到 {other:?}"),
                };
                syscall(
                    DataOp::Write {
                        fd: wfd,
                        data: payload.clone(),
                    },
                    vec![wr(wfd)],
                    move |_| {
                        syscall(
                            DataOp::Read {
                                fd: rfd,
                                len: payload.len(),
                            },
                            vec![rd(rfd)],
                            Action::Pure,
                        )
                    },
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(expect));
}

// ── d. Scope：经 interpret 后逻辑 cwd 恢复（成功与失败路径）────────────

#[test]
fn e2e_scope_cwd() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    // 成功路径：Scope 内部以 base 为基准切换 cwd，退出后恢复。
    let _ = rt
        .run_blocking(Action::Scope {
            base: PathBuf::from("sub/dir"),
            inner: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        })
        .unwrap();
    assert_eq!(rt.context().cwd, before);

    // 失败路径：inner 出错时同样恢复 cwd（finally 语义）。
    let err = rt
        .run_blocking(Action::Scope {
            base: PathBuf::from("other"),
            inner: Box::new(syscall(
                DataOp::Read {
                    fd: 999_999,
                    len: 1,
                },
                vec![rd(999_999)],
                Action::Pure,
            )),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        })
        .unwrap_err();
    assert!(matches!(err, SysError::NotFound));
    assert_eq!(rt.context().cwd, before);
}
