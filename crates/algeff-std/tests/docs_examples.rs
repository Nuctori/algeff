//! 教程文档示例编译验证护栏（docs/getting-started.md / docs/example.md）：
//! 与 `readme_examples.rs` 同模式——文档示例改动后若不同步本文件，CI
//! 编译/运行即失败，防止教程与真实 API 漂移（「全部示例与测试文件逐字一致」
//! 的可验证载体，A6 教程体系）。
//!
//! 对应关系（示例体逐字，测试包裹为测试侧变体）：
//! - `docs_minimal_file_io_roundtrip` ⇔ getting-started.md「真实文件 IO」
//! - `docs_error_short_circuit`       ⇔ getting-started.md「错误短路上抛」
//! - `docs_catch_recovery`            ⇔ getting-started.md「dx::catch 恢复」
//! - `docs_plan_fork_mix`             ⇔ example.md「do_! 与 plan!/fork! 混合」
//! - `docs_lock_reentrant`            ⇔ example.md「锁：同 id 可重入」
//! - `docs_signal_repeatable`         ⇔ example.md「信号：可重复发送」
//! - `docs_tcp_echo_do`               ⇔ example.md「TCP echo do_! 版」

use std::net::Shutdown;
use std::time::Duration;

use algeff_core::prelude::*;
use algeff_core::{OpenFlags, ResourceHandle, SysError};
use algeff_macro::{do_, fork, plan};
use algeff_std::{dx, TokioExecutor};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn rt() -> Runtime {
    Runtime::new(Box::new(TokioExecutor::new()))
}

/// 打开文件（写 + 建）的通用 flags。
fn open_rw_create() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

// ── getting-started.md：第一个程序——纯蓝图（plan!）────────────────

#[test]
fn docs_first_program_pure() {
    let mut rt = rt();

    // 蓝图 = 数据：一段"先算 1+1，再算 2×3"的序列
    let blueprint = plan! {
        Action::Pure(Value::U64(1 + 1));
        Action::Pure(Value::U64(2 * 3));
    };

    let result = rt.run_blocking(blueprint);
    assert!(matches!(result, Ok(Value::Unit))); // plan! 链收敛于 Unit
}

// ── getting-started.md：真实文件 IO（do_! 7 行示例体）───────────────

#[test]
fn docs_minimal_file_io_roundtrip() {
    let mut rt = rt();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.txt");
    let flags = open_rw_create();

    // 语法就是普通 Rust：open/write/seek/read/close 直书，
    // fd 经 let 绑定贯穿，资源声明由 dx 按操作自动推导
    let blueprint = do_! {
        let fd = dx::open(&path, flags);
        dx::write(&fd, b"hello algeff".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data // 尾表达式 = 链的最终值
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"hello algeff".to_vec()));
    assert_eq!(std::fs::read(&path).unwrap(), b"hello algeff");
}

// ── getting-started.md：错误短路上抛（链首失败 → Err，非 panic）───────

#[test]
fn docs_error_short_circuit() {
    let mut rt = rt();
    // 不带 create 打开不存在的文件：Open 在链首失败 → 整链 Err，
    // 后续语句短路（Write/Read/Close 不执行）。
    let blueprint = do_! {
        let fd = dx::open("no_such.txt", OpenFlags { read: true, ..Default::default() });
        dx::write(&fd, b"x".to_vec());
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data // 打开失败时链首即 Err，不会执行到这里
    };

    let result = rt.run_blocking(blueprint);
    assert!(matches!(result, Err(SysError::NotFound)));
}

// ── getting-started.md：dx::catch 恢复（handler 收到真实 SysError）────

#[test]
fn docs_catch_recovery() {
    let mut rt = rt();
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no_such.txt");
    let fallback = dir.path().join("fallback.txt");
    let fb = fallback.clone(); // handler 需 'static：clone 后 move 进闭包

    let blueprint = do_! {
        let fd = dx::catch(
            dx::open(&missing, OpenFlags { read: true, ..Default::default() }),
            move |e| {
                // handler: SysError → 替代 Action，其值成为 catch 表达式的结果值
                assert!(matches!(e, SysError::NotFound));
                dx::open(fb.clone(), open_rw_create())
            },
        );
        dx::write(&fd, b"recovered".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"recovered".to_vec()));
    assert_eq!(
        std::fs::read(&fallback).unwrap(),
        b"recovered",
        "handler 的替代路径真实落盘"
    );
}

// ── example.md：do_! 与 plan!/fork! 混合（并发分叉写不同文件）────────

#[test]
fn docs_plan_fork_mix() {
    let mut rt = rt();
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("mix");
    let fa = sub.join("a.txt");
    let fb = sub.join("b.txt");

    // do_!/plan!/fork! 内嵌的 do_! 均预构建 Action 值（do_! 展开闭包要求
    // 'static、plan!/fork! 只做值组合）。
    let mkdir_act = do_! { dx::mkdir(&sub, 0o755); Value::Unit };
    let left_act = do_! {
        let f = dx::open(fa.clone(), open_rw_create());
        dx::write(&f, b"A".to_vec());
        dx::close(&f);
        Value::Unit
    };
    let right_act = do_! {
        let f = dx::open(fb.clone(), open_rw_create());
        dx::write(&f, b"B".to_vec());
        dx::close(&f);
        Value::Unit
    };
    let stat_act = do_! {
        let _ = dx::stat(&sub);
        Value::Unit
    };

    // do_!（命令式骨架）内嵌 plan!（声明式子步骤）与 fork!（并发分叉，
    // 左右分支写不同文件 → 无资源冲突）。
    let blueprint = do_! {
        plan! { mkdir_act };
        fork! {
            left: left_act,
            right: right_act,
        };
        stat_act;
        Value::U64(42)
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::U64(42));
    assert_eq!(std::fs::read(&fa).unwrap(), b"A");
    assert_eq!(std::fs::read(&fb).unwrap(), b"B");
}

// ── example.md：锁——同 id 可重入（lock→unlock→lock 成功）───────────

#[test]
fn docs_lock_reentrant() {
    let mut rt = rt();
    // lock → unlock → 再 lock 同 id：仲裁在 executor 层 arbiter
    // （占坑⟺持锁），A4 线性域不消费锁 id（空声明）→ 可重入。
    let blueprint = do_! {
        dx::mutex_lock(7);
        dx::mutex_unlock(7);
        dx::mutex_lock(7);
        dx::mutex_unlock(7);
        Value::Unit
    };
    rt.run_blocking(blueprint).unwrap();

    // 显式 unlock 已物理释放，undo 为幂等 no-op——两把锁的 undo 入栈。
    assert_eq!(rt.undo_stack().len(), 2);
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
}

// ── example.md：信号——二次发送不被 A4 拒绝（成败由物理层决定）──────

#[test]
fn docs_signal_repeatable() {
    let mut rt = rt();
    // 长存活子进程（平台差异：Unix sh -c sleep；Windows cmd timeout）。
    #[cfg(windows)]
    let cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "timeout", "60"]);
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "exec sleep 60"]);
        c
    };
    let pid = rt.run_blocking(dx::spawn(cmd)).unwrap();
    assert!(matches!(pid, Value::Pid(_)));

    // 第一次 SIGKILL（9）：路由 op_kill → 物理杀进程。
    rt.run_blocking(dx::send_signal(9, &pid)).unwrap();

    // 第二次 SIGKILL：SendSignal 空声明（A4 不消费 Signal/Pid），修复前
    // 在此被 A4 拒 InvalidInput；修复后进入物理层（已终止子进程 start_kill
    // 幂等成功，或平台层错误），但绝不可能是 A4 的 InvalidInput。
    let second = rt.run_blocking(dx::send_signal(9, &pid));
    match second {
        Ok(_) => {}
        Err(e) => assert!(
            !matches!(e, SysError::InvalidInput),
            "二次 SendSignal 不应被 A4 拒绝（InvalidInput），实测 {e:?}"
        ),
    }

    // 收割子进程（退出码 1 = 信号终止，平台差异不断言具体值）。
    let v = rt.run_blocking(dx::wait(&pid)).unwrap();
    assert!(matches!(v, Value::U64(_)));
}

// ── example.md：TCP echo do_! 版（accept → read → write → shutdown）──

#[test]
fn docs_tcp_echo_do() {
    let mut rt = rt();
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // 1. 先绑定（127.0.0.1:0 → 内核分配端口），从 registry 取回真实地址。
    let lfd = rt.run_blocking(dx::open_tcp(addr)).unwrap();
    let lfd = dx::expect_fd(&lfd);
    let real_addr: std::net::SocketAddr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener 句柄，得到 {other:?}"),
    };

    // 2. 客户端线程：tokio 原生 TcpStream 连接并收发小 payload（10s 超时防悬挂）。
    let payload: Vec<u8> = b"echo-do".to_vec(); // 小 payload：单次 TcpRead 即收满
    let n = payload.len();
    let client_payload = payload.clone();
    let client = std::thread::spawn(move || {
        let client_rt = tokio::runtime::Runtime::new().unwrap();
        client_rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut s = tokio::net::TcpStream::connect(real_addr).await.unwrap();
                s.write_all(&client_payload).await.unwrap();
                let mut buf = vec![0u8; client_payload.len()];
                s.read_exact(&mut buf).await.unwrap();
                buf
            })
            .await
            .expect("客户端连接/收发 10s 超时")
        })
    });

    // 3. 服务端 do_! 蓝图：accept → read → write(echo) → shutdown。
    //    注意：accept 返回 Value::List([Fd, Addr])，do_! 语句槽只接受 Action
    //    表达式，故用 dx::pure 包一层运行时提取（dx::expect_* 系列）——这是
    //    DX 层的已知丑点（dx-design.md §7），骨架/循环变体见 e2e.rs。
    let blueprint = do_! {
        let conn = dx::accept(&Value::Fd(lfd));
        let sfd = dx::pure(Value::Fd(dx::expect_fd(&dx::expect_list(&conn)[0])));
        let data = dx::tcp_read(&sfd, n);
        dx::tcp_write(&sfd, dx::expect_bytes(&data));
        dx::tcp_shutdown(&sfd, Shutdown::Both);
        Value::U64(n as u64)
    };
    let v = rt.run_blocking(blueprint).unwrap();

    let echoed = client.join().unwrap();
    assert_eq!(v, Value::U64(n as u64));
    assert_eq!(echoed, payload);
}
