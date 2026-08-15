//! A5 集成测试：直接调用 `TokioExecutor::execute` + `ResourceRegistry`
//! （不经 interpret——A2 的解释器在另一分支）。Registry 用 default + tempfile 目录。

use std::time::Duration;

use algeff_core::{
    DataOp, MmapProt, OpenFlags, PipeFlags, ResourceHandle, ResourceRegistry, SysError,
    SyscallExecutor, Value,
};
use algeff_std::TokioExecutor;

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

/// 期望 execute 返回错误并取出（Ok 类型含非 Debug 的 UndoOp，不能 unwrap_err）。
async fn exec_err(ex: &mut TokioExecutor, reg: &mut ResourceRegistry, op: &DataOp) -> SysError {
    match ex.execute(op, reg).await {
        Err(e) => e,
        Ok(_) => panic!("期望错误，得到成功"),
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

// ── g. 互斥锁互斥（并发 MutexLock 同一 id：仲裁占坑 + 有限重试）─────────────

#[tokio::test]
async fn mutex_lock_exclusion() {
    // 并发 MutexLock 同一 id：D16 接入后竞争失败方在有限重试（8×1ms）后返回
    // WouldBlock，不再阻塞等待（A7：失败回滚 + 有限重试，绝不挂起）。
    // 旧断言「第二把锁被阻塞」同步更新为「快速失败不挂死」。
    let ex = std::sync::Arc::new(tokio::sync::Mutex::new(TokioExecutor::new()));
    let mut reg = ResourceRegistry::new();
    let op = DataOp::MutexLock { id: 7 };

    let (_v, undo1) = ex.lock().await.execute(&op, &mut reg).await.unwrap();

    let ex2 = ex.clone();
    let mut reg2 = ResourceRegistry::new();
    let t = tokio::spawn(async move {
        let mut g = ex2.lock().await;
        g.execute(&DataOp::MutexLock { id: 7 }, &mut reg2).await
    });

    // 第一把锁仍持有 → 第二把必须在有限重试内返回 WouldBlock（不挂死）。
    let res = tokio::time::timeout(Duration::from_secs(2), t)
        .await
        .expect("第二把锁挂死")
        .unwrap();
    match res {
        Err(SysError::WouldBlock) => {} // 预期：竞争失败 → 有限重试后快速失败
        Ok(_) => panic!("持锁期间竞争应 WouldBlock，却成功获取锁"),
        Err(e) => panic!("持锁期间竞争应 WouldBlock，得到 {e:?}"),
    }

    // 释放第一把锁：执行 undo（等价解释器 recover 路径）→ 重新获取成功。
    if let Some(u) = undo1 {
        u.await;
    }
    let (_v2, _undo2) = ex.lock().await.execute(&op, &mut reg).await.unwrap();
}

// ── g2. 并发双任务：至多一个持有，不挂死（D16 接入验证）──────────────────

#[tokio::test]
async fn mutex_lock_arbiter_contention() {
    // 两个并发 execute 同一 id（共享同一执行器 → 同一仲裁器与物理锁）：
    // 仲裁器独占占坑 → 至多一个同时持有；竞争失败方有限重试后 WouldBlock，
    // 两个任务都在超时内返回（不挂死）。释放成功方后失败方可重新获取。
    let ex = std::sync::Arc::new(tokio::sync::Mutex::new(TokioExecutor::new()));
    let ex_a = ex.clone();
    let ex_b = ex.clone();
    let (ra, rb) = tokio::join!(
        async move {
            let mut reg = ResourceRegistry::new();
            let mut g = ex_a.lock().await;
            tokio::time::timeout(
                Duration::from_secs(2),
                g.execute(&DataOp::MutexLock { id: 42 }, &mut reg),
            )
            .await
            .expect("任务 A 挂死")
        },
        async move {
            let mut reg = ResourceRegistry::new();
            let mut g = ex_b.lock().await;
            tokio::time::timeout(
                Duration::from_secs(2),
                g.execute(&DataOp::MutexLock { id: 42 }, &mut reg),
            )
            .await
            .expect("任务 B 挂死")
        },
    );
    let (undo_opt, a_ok, b_ok) = match (ra, rb) {
        (Ok((_, u)), Err(SysError::WouldBlock)) => (u, true, false),
        (Err(SysError::WouldBlock), Ok((_, u))) => (u, false, true),
        (Ok(_), Ok(_)) => panic!("两个并发 MutexLock 都成功：独占占坑被破坏"),
        _ => panic!("竞争失败方应返回 WouldBlock，而非其他错误/挂死"),
    };
    // 独占不变量：至多一个同时持有（恰好一个 Ok）。
    assert!(a_ok ^ b_ok, "至多一个同时持有（A={a_ok} B={b_ok}）");
    // 释放成功方的锁（undo 已同时释放 arbiter 占坑）→ 重新获取成功。
    undo_opt.expect("MutexLock 应返回 undo").await;
    let mut reg = ResourceRegistry::new();
    let mut g = ex.lock().await;
    let (_v, _u) = g
        .execute(&DataOp::MutexLock { id: 42 }, &mut reg)
        .await
        .expect("释放后应能重新获取锁");
}

// ── g3. MutexUnlock 显式路径释放 arbiter 占坑（幂等）────────────────────

#[tokio::test]
async fn mutex_unlock_releases_arbiter() {
    // lock → unlock → 再次 lock：MutexUnlock 同步释放 arbiter 占坑（幂等：
    // 第二次 unlock / undo 已释放时是 no-op），claim 不泄漏 → 重新可锁。
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let op = DataOp::MutexLock { id: 3 };

    let (_v1, undo1) = ex.execute(&op, &mut reg).await.unwrap();
    // 显式 unlock（返回 None undo，但必须同步释放 arbiter 占坑）。
    ex.execute(&DataOp::MutexUnlock { id: 3 }, &mut reg)
        .await
        .unwrap();
    // 再次 lock：若 unlock 未释放占坑 → try_claim 失败 → WouldBlock。
    let (_v2, undo2) = ex.execute(&op, &mut reg).await.unwrap();
    // 二次 unlock 幂等（释放 lock#2 重新建立的占坑）。
    ex.execute(&DataOp::MutexUnlock { id: 3 }, &mut reg)
        .await
        .unwrap();
    // undo1（recover 路径）此时全部 no-op：slot 已空 + release 幂等，
    // 不得破坏 lock#2 已释放的状态。
    undo1.expect("MutexLock 应返回 undo").await;
    if let Some(u2) = undo2 {
        u2.await;
    }
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

// ── i. blocker-3：Dup 共享后 IO 错误路径不得丢句柄 ────────────────────
// 修复前：take 后 Arc::get_mut 失败（Dup 共享 → InvalidInput）直接 ? 返回，
// registry 条目被删、内部映射悬空、Arc 被 drop，fd 永久损坏；修复后错误路径
// 恢复注册表条目与内部映射，关闭 dup 释放共享后原 fd 仍可用。

#[tokio::test]
async fn pipe_dup_read_invalid_then_recover() {
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

    // Dup 共享 Arc → 读端无法 &mut → InvalidInput。
    let v = ex
        .execute(&DataOp::Dup { fd: rfd }, &mut reg)
        .await
        .unwrap();
    let dup = fd_of(&v.0);
    let err = exec_err(&mut ex, &mut reg, &DataOp::Read { fd: rfd, len: 4 }).await;
    assert_eq!(err, SysError::InvalidInput, "Dup 共享后读应 InvalidInput");

    // 关闭 dup 释放共享 → 原 fd 必须仍可读（错误路径已恢复注册表条目与内部映射，
    // 修复前此处 pipe_reader_fds 悬空 → NotFound 且管道被整体关闭）。
    ex.execute(&DataOp::Close { fd: dup }, &mut reg)
        .await
        .unwrap();
    let (v, _) = ex
        .execute(&DataOp::Read { fd: rfd, len: 4 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"ping".to_vec()));
}

#[tokio::test]
async fn pipe_dup_write_invalid_then_recover() {
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

    // Dup 共享 Arc → 写端无法 &mut → InvalidInput。
    let v = ex
        .execute(&DataOp::Dup { fd: wfd }, &mut reg)
        .await
        .unwrap();
    let dup = fd_of(&v.0);
    let err = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Write {
            fd: wfd,
            data: b"pong".to_vec(),
        },
    )
    .await;
    assert_eq!(err, SysError::InvalidInput, "Dup 共享后写应 InvalidInput");

    // 关闭 dup → 原写端恢复可用，数据可达读端。
    ex.execute(&DataOp::Close { fd: dup }, &mut reg)
        .await
        .unwrap();
    ex.execute(
        &DataOp::Write {
            fd: wfd,
            data: b"pong".to_vec(),
        },
        &mut reg,
    )
    .await
    .unwrap();
    let (v, _) = ex
        .execute(&DataOp::Read { fd: rfd, len: 4 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"pong".to_vec()));
}

#[tokio::test]
async fn tcp_dup_io_invalid_then_recover() {
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

    // Dup 共享 → 读与关断均 InvalidInput，且错误路径不得丢句柄。
    let v = ex
        .execute(&DataOp::Dup { fd: sfd }, &mut reg)
        .await
        .unwrap();
    let dup = fd_of(&v.0);
    let err = exec_err(&mut ex, &mut reg, &DataOp::TcpRead { fd: sfd, len: 4 }).await;
    assert_eq!(
        err,
        SysError::InvalidInput,
        "Dup 共享后 TCP 读应 InvalidInput"
    );
    let err = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::TcpShutdown {
            fd: sfd,
            how: std::net::Shutdown::Write,
        },
    )
    .await;
    assert_eq!(
        err,
        SysError::InvalidInput,
        "Dup 共享后 TCP 关断应 InvalidInput"
    );

    // 关闭 dup 释放共享 → 原 fd 读/关断恢复可用。
    ex.execute(&DataOp::Close { fd: dup }, &mut reg)
        .await
        .unwrap();
    let (v, _) = ex
        .execute(&DataOp::TcpRead { fd: sfd, len: 4 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"ping".to_vec()));
    ex.execute(
        &DataOp::TcpShutdown {
            fd: sfd,
            how: std::net::Shutdown::Write,
        },
        &mut reg,
    )
    .await
    .unwrap();
}

// ── j. blocker-3：子进程 Wait 的 Dup 共享错误路径不得丢句柄 ─────────────
// 修复前：Wait 失败后 children 映射悬空 → 后续 Wait NotFound 且 Child 未 wait 被
// drop（进程/僵尸泄漏）；修复后 put_child_back 恢复映射，Close dup 后 Wait 成功。

#[tokio::test]
async fn child_dup_wait_invalid_then_recover() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();

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
    // 全新 registry 中首个 allocate 的 fd 为 0（D1 单调分配）；类型断言防漂移。
    let cfd = match reg.lookup(0) {
        Some(ResourceHandle::Child(_)) => 0u64,
        other => panic!("期望 fd 0 为 Child 句柄，得到 {other:?}"),
    };

    // Dup 共享 Child Arc → Wait 无法 &mut → InvalidInput。
    let v = ex
        .execute(&DataOp::Dup { fd: cfd }, &mut reg)
        .await
        .unwrap();
    let dup = fd_of(&v.0);
    let err = exec_err(&mut ex, &mut reg, &DataOp::Wait { pid }).await;
    assert_eq!(
        err,
        SysError::InvalidInput,
        "Dup 共享后 Wait 应 InvalidInput"
    );

    // 关闭 dup 释放共享 → Wait 恢复可用（错误路径已恢复 children 映射与注册表条目）。
    ex.execute(&DataOp::Close { fd: dup }, &mut reg)
        .await
        .unwrap();
    let (v, _) = ex.execute(&DataOp::Wait { pid }, &mut reg).await.unwrap();
    assert_eq!(v, Value::U64(3));
}

// ── k. medium-7：Mmap 按 len 截断 ─────────────────────────────────────

#[tokio::test]
async fn mmap_respects_len_truncation() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mmap.txt");
    std::fs::write(&path, b"abcdefghij").unwrap(); // 10 字节

    // len < 文件长：按 len 截断。
    let (v, _) = ex
        .execute(
            &DataOp::Mmap {
                path: path.clone(),
                len: 4,
                prot: MmapProt::default(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"abcd".to_vec()));

    // len ≥ 文件长：返回全部内容。
    let (v, _) = ex
        .execute(
            &DataOp::Mmap {
                path,
                len: 100,
                prot: MmapProt::default(),
            },
            &mut reg,
        )
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"abcdefghij".to_vec()));
}
