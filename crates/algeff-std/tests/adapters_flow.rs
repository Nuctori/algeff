//! A5 批 3：值流组合器测试（RFC-A5-3）——`and_then`/`then`/`seq` 的
//! 构造形态断言 + 经真实 `Runtime`+`TokioExecutor` 的执行级验证
//! （复用批 2 e2e 模式：D9 驱动方式，`run_blocking` 在主线程 interpret）。
//!
//! 覆盖 pdr.md §14 的编程体验：fd 值经 and_then 链的词法捕获贯穿
//! （connect → write → read → close 的 TCP 客户端蓝图）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use algeff_core::{
    Action, DataOp, OpenFlags, ReadOnly, ResourceInner, ResourceUsage, Runtime, TypedResource,
    Value, WriteOnly,
};
use algeff_std::adapters;
use algeff_std::TokioExecutor;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ── 本地辅助（adapters.rs 私有辅助的复制；src/ 冻结不可改）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
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

/// TCP 客户端读循环：TcpRead 收满 expected 字节（TCP 流可能分片到达，
/// 复用批 2 e2e 的 tcp_read_all 模式）。
fn tcp_recv_all(fd: u64, expected: usize, acc: Vec<u8>) -> Action {
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
                tcp_recv_all(fd, expected - b.len(), all)
            }
            other => panic!("期望 Bytes，得到 {other:?}"),
        },
    )
}

/// 蓝图示例（pdr.md §14 编程体验）：connect → write → read → close，
/// fd 值经 and_then 链的词法捕获贯穿。executor 的 TCP 走专用
/// DataOp（TcpWrite/TcpRead），无预包装适配器，此处以 syscall 助手
/// 显式构造；组合器（and_then）负责接续与值交付。
fn tcp_client_blueprint(
    addr: SocketAddr,
    payload: Vec<u8>,
    recv_len: usize,
    got: Arc<Mutex<Option<Vec<u8>>>>,
) -> Action {
    adapters::and_then(adapters::connect(addr), move |v| {
        let fd = fd_of(&v);
        adapters::and_then(
            syscall(
                DataOp::TcpWrite {
                    fd,
                    data: payload.clone(),
                },
                vec![wr(fd)],
                Action::Pure,
            ),
            move |_| {
                adapters::and_then(tcp_recv_all(fd, recv_len, Vec::new()), move |v| {
                    let bytes = match v {
                        Value::Bytes(b) => b,
                        other => panic!("期望 Bytes，得到 {other:?}"),
                    };
                    *got.lock().unwrap() = Some(bytes);
                    adapters::close(fd)
                })
            },
        )
    })
}

// ── a. Pure 短路：Pure(v) 接 and_then(f) → f 收到 v ────────────────────

#[test]
fn and_then_pure_passthrough() {
    let received: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let got = received.clone();
    let a = adapters::and_then(Action::Pure(Value::U64(42)), move |v| {
        *got.lock().unwrap() = Some(v);
        Action::Pure(Value::Str("f 已接续".to_string()))
    });
    // Pure 无运行时步骤：f 在构造期即收到 prev 的值。
    assert_eq!(*received.lock().unwrap(), Some(Value::U64(42)));
    // 结果即 f 的返回（无多余 Sequential 帧）。
    assert!(matches!(a, Action::Pure(Value::Str(s)) if s == "f 已接续"));
}

// ── b. Syscall 接续：单 syscall Action（返回 Fd）接 and_then(f) → f 收到 fd ──

#[test]
fn and_then_after_syscall_structural() {
    let flags = OpenFlags {
        read: true,
        write: true,
        ..Default::default()
    };
    let received: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let got = received.clone();
    let a = adapters::and_then(
        adapters::open_file(PathBuf::from("/tmp/x.txt"), flags),
        move |v| {
            *got.lock().unwrap() = Some(v);
            Action::Pure(Value::Unit)
        },
    );
    match a {
        Action::Sequential { current, next } => {
            // current 为原 syscall（open_file 的 Open 节点）。
            assert!(matches!(
                *current,
                Action::Syscall {
                    op: DataOp::Open { .. },
                    ..
                }
            ));
            // next 即 f 的包装：交付 Fd → f 收到 fd 值。
            let out = next(Value::Fd(77));
            assert_eq!(*received.lock().unwrap(), Some(Value::Fd(77)));
            assert!(matches!(out, Action::Pure(Value::Unit)));
        }
        _ => panic!("and_then 对非 Pure 输入应构造 Sequential 包装"),
    }
}

#[test]
fn and_then_after_syscall_executes_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flow.txt");
    let flags = OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    };
    let payload: Vec<u8> = b"via and_then chain".to_vec();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let done: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let done2 = done.clone();
    let payload2 = payload.clone();
    // open_file → and_then(write) → and_then(close)：fd 经闭包词法捕获贯穿。
    let action = adapters::and_then(adapters::open_file(path.clone(), flags), move |v| {
        let fd = fd_of(&v);
        adapters::and_then(adapters::write(fd, payload2.clone()), move |_| {
            *done2.lock().unwrap() = true;
            adapters::close(fd)
        })
    });

    let v = rt.run_blocking(action).unwrap();
    assert_eq!(v, Value::Unit);
    assert!(*done.lock().unwrap(), "write 步骤未执行");
    // 全链路：文件已打开写入并关闭，内容落盘。
    assert_eq!(std::fs::read(&path).unwrap(), payload);
}

// ── c. seq 折叠：seq(vec![a, b, c]) 结构断言嵌套深度 ──────────────────

#[test]
fn seq_folds_chain() {
    // 左折叠 then(then(a, b), c) → 两层 Sequential 嵌套。
    let s = adapters::seq(vec![
        adapters::get_time(),
        adapters::get_time(),
        adapters::get_time(),
    ]);
    match s {
        Action::Sequential { current, next } => {
            match *current {
                Action::Sequential {
                    current: a,
                    next: a_next,
                } => {
                    assert!(matches!(
                        *a,
                        Action::Syscall {
                            op: DataOp::GetTime,
                            ..
                        }
                    ));
                    // 内层 next 忽略值 → b。
                    assert!(matches!(
                        a_next(Value::Unit),
                        Action::Syscall {
                            op: DataOp::GetTime,
                            ..
                        }
                    ));
                }
                _ => panic!("seq 左折叠：内层应为 Sequential(then(a, b))"),
            }
            // 外层 next 忽略值 → c。
            assert!(matches!(
                next(Value::Unit),
                Action::Syscall {
                    op: DataOp::GetTime,
                    ..
                }
            ));
        }
        _ => panic!("seq 应构造 Sequential 链"),
    }
    // 退化：空列表 → Pure(Unit)；单元素 → 原样返回。
    assert!(matches!(
        adapters::seq(Vec::new()),
        Action::Pure(Value::Unit)
    ));
    assert!(matches!(
        adapters::seq(vec![Action::Pure(Value::U64(9))]),
        Action::Pure(Value::U64(9))
    ));
}

// ── then 独立覆盖：忽略值，固定接续 next ──────────────────────────────

#[test]
fn then_ignores_value() {
    let a = adapters::then(adapters::get_time(), Action::Pure(Value::U64(5)));
    match a {
        Action::Sequential { current, next } => {
            assert!(matches!(
                *current,
                Action::Syscall {
                    op: DataOp::GetTime,
                    ..
                }
            ));
            // 无论当前值是什么，next 恒返回固定动作。
            assert!(matches!(next(Value::U64(123)), Action::Pure(Value::U64(5))));
        }
        _ => panic!("then 应构造忽略值的 Sequential 包装"),
    }
}

// ── d. TCP 客户端蓝图：组合器链经 Runtime 执行，连本地 echo 断言往返 ──

#[test]
fn tcp_client_blueprint_echo() {
    // 1KB 非平凡 payload（避免全零巧合与单字节模式）。
    let payload: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
    let n = payload.len();

    // 服务端线程：tokio 原生 echo（bind → accept → read_exact → write_all）。
    // 经 mpsc 通道把绑定后的真实地址交给主线程（Runtime 不可跨线程，D9）。
    let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
    let srv_len = n;
    let server = std::thread::spawn(move || {
        let srt = tokio::runtime::Runtime::new().unwrap();
        srt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; srv_len];
            sock.read_exact(&mut buf).await.unwrap();
            sock.write_all(&buf).await.unwrap();
            let _ = sock.shutdown().await;
            buf
        })
    });
    let addr = rx.recv().unwrap();

    // 客户端：组合器构造的蓝图经 Runtime 全链路 interpret（主线程）。
    let got: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(tcp_client_blueprint(addr, payload.clone(), n, got.clone()))
        .unwrap();

    // 链末为 close → Unit；往返字节在 read 步骤捕获。
    assert_eq!(v, Value::Unit);
    assert_eq!(got.lock().unwrap().as_deref(), Some(payload.as_slice()));

    // 服务端同样收满 1KB（往返一致）。
    let echoed = server.join().unwrap();
    assert_eq!(echoed, payload);
}
