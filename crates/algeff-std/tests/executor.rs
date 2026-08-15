//! A5 集成测试：直接调用 `TokioExecutor::execute` + `ResourceRegistry`
//! （不经 interpret——A2 的解释器在另一分支）。Registry 用 default + tempfile 目录。

use std::time::Duration;

use algeff_core::{
    DataOp, OpenFlags, PipeFlags, ResourceHandle, ResourceRegistry, SyscallExecutor, Value,
};
use algeff_std::TokioExecutor;

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

// ── a. 文件写入/读取往返 ───────────────────────────────────────────────

#[tokio::test]
async fn file_write_read_roundtrip() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.txt");

    let v = ex
        .execute(
            &DataOp::Open {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            &mut reg,
        )
        .await
        .unwrap();
    let fd = fd_of(&v.0);

    let (v, _undo) = ex
        .execute(
            &DataOp::Write {
                fd,
                data: b"hello world".to_vec(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    assert_eq!(v, Value::Unit);

    ex.execute(
        &DataOp::Seek {
            fd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        &mut reg,
    )
    .await
    .unwrap();

    let (v, _) = ex
        .execute(&DataOp::Read { fd, len: 11 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"hello world".to_vec()));
}

// ── b. Write 撤销恢复文件原内容 ────────────────────────────────────────

#[tokio::test]
async fn undo_restores_file_content() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("undo.txt");
    std::fs::write(&path, b"original content").unwrap();

    let v = ex
        .execute(
            &DataOp::Open {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            &mut reg,
        )
        .await
        .unwrap();
    let fd = fd_of(&v.0);

    let (_, undo) = ex
        .execute(
            &DataOp::Write {
                fd,
                data: b"changed content!".to_vec(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let undo = undo.expect("小文件 Write 应返回 Full 撤销（undo）");
    undo.await;

    assert_eq!(std::fs::read(&path).unwrap(), b"original content");
}

// ── c. Rename 撤销恢复原名 ─────────────────────────────────────────────

#[tokio::test]
async fn rename_undo() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"data").unwrap();

    let (_, undo) = ex
        .execute(
            &DataOp::Rename {
                from: a.clone(),
                to: b.clone(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    assert!(b.exists() && !a.exists());

    undo.expect("Rename 应返回 undo").await;
    assert!(a.exists() && !b.exists());
}

// ── d. TCP echo 全链路 ─────────────────────────────────────────────────

#[tokio::test]
async fn tcp_echo_roundtrip() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

    let v = ex
        .execute(
            &DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let lfd = fd_of(&v.0);
    let addr = match reg.lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        _ => panic!("期望 TcpListener 句柄"),
    };

    let v = ex
        .execute(&DataOp::TcpConnect { addr }, &mut reg)
        .await
        .unwrap();
    let cfd = fd_of(&v.0);

    ex.execute(
        &DataOp::TcpWrite {
            fd: cfd,
            data: b"ping".to_vec(),
        },
        &mut reg,
    )
    .await
    .unwrap();

    let v = ex
        .execute(&DataOp::TcpAccept { listener: lfd }, &mut reg)
        .await
        .unwrap();
    let sfd = match &v.0 {
        Value::List(l) => fd_of(&l[0]),
        other => panic!("期望 List，得到 {other:?}"),
    };

    let (v, _) = ex
        .execute(&DataOp::TcpRead { fd: sfd, len: 4 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"ping".to_vec()));
}

// ── e. Dup 共享句柄 ────────────────────────────────────────────────────

#[tokio::test]
async fn dup_shares_handle() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dup.txt");

    let v = ex
        .execute(
            &DataOp::Open {
                path,
                flags: OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            &mut reg,
        )
        .await
        .unwrap();
    let fd = fd_of(&v.0);

    ex.execute(
        &DataOp::Write {
            fd,
            data: b"hello".to_vec(),
        },
        &mut reg,
    )
    .await
    .unwrap();

    let v = ex.execute(&DataOp::Dup { fd }, &mut reg).await.unwrap();
    let dup = fd_of(&v.0);

    // 共享句柄：原 fd 上的 seek 会影响 dup fd 的游标（同一文件描述）。
    ex.execute(
        &DataOp::Seek {
            fd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        &mut reg,
    )
    .await
    .unwrap();

    // 取走原 fd（注册表 token 移除），dup fd 仍可读。
    assert!(reg.take(fd).is_some());
    let (v, _) = ex
        .execute(&DataOp::Read { fd: dup, len: 5 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"hello".to_vec()));
}

// ── f. 管道双工（writer 写 → reader 读）───────────────────────────────

#[tokio::test]
async fn pipe_duplex() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

    let (v, _) = ex
        .execute(
            &DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    let (rfd, wfd) = match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List，得到 {other:?}"),
    };

    ex.execute(
        &DataOp::Write {
            fd: wfd,
            data: b"ping".to_vec(),
        },
        &mut reg,
    )
    .await
    .unwrap();

    let (v, _) = ex
        .execute(&DataOp::Read { fd: rfd, len: 4 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"ping".to_vec()));
}

// ── g. 互斥锁互斥（并发 MutexLock 同一 id 串行化）─────────────────────

#[tokio::test]
async fn mutex_lock_exclusion() {
    let ex = std::sync::Arc::new(tokio::sync::Mutex::new(TokioExecutor::new()));
    let mut reg = ResourceRegistry::new();
    let op = DataOp::MutexLock { id: 7 };

    let (_v, undo1) = ex.lock().await.execute(&op, &mut reg).await.unwrap();

    let ex2 = ex.clone();
    let mut reg2 = ResourceRegistry::new();
    let t = tokio::spawn(async move {
        let mut g = ex2.lock().await;
        g.execute(&op, &mut reg2).await.unwrap()
    });

    // 给第二个任务到达阻塞点的机会；此时它必须仍被第一把锁阻塞。
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!t.is_finished(), "并发 MutexLock 应被第一把锁阻塞");

    // 释放第一把锁：执行 undo（等价解释器 recover 路径）→ 第二个任务获得锁并完成。
    if let Some(u) = undo1 {
        u.await;
    }
    let (_v2, _undo2) = tokio::time::timeout(Duration::from_secs(2), t)
        .await
        .expect("第二把锁等待超时")
        .unwrap();
}

// ── h. 子进程退出码 ────────────────────────────────────────────────────

#[tokio::test]
async fn spawn_wait_exit_code() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

    // 平台差异：Windows 用 cmd /C exit 3；Unix 用 sh -c 'exit 3'。
    #[cfg(windows)]
    let cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "exit", "3"]);
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "exit 3"]);
        c
    };

    let (v, _) = ex.execute(&DataOp::Spawn { cmd }, &mut reg).await.unwrap();
    let pid = match v {
        Value::Pid(p) => p,
        other => panic!("期望 Pid，得到 {other:?}"),
    };

    let (v, _) = ex.execute(&DataOp::Wait { pid }, &mut reg).await.unwrap();
    assert_eq!(v, Value::U64(3));
}
