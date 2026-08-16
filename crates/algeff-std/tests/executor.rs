//! A5 集成测试：直接调用 `TokioExecutor::execute` + `ResourceRegistry`
//! （不经 interpret——A2 的解释器在另一分支）。Registry 用 default + tempfile 目录。
//! g4 的 Timeout 取消场景走完整 interpret 链路（`Runtime` + `run_blocking`，
//! 复用 e2e.rs 的 D9 驱动模式：普通 `#[test]`）。

use std::time::Duration;

use algeff_core::{
    AccessMode, Action, DataOp, MmapProt, OpenFlags, PipeFlags, Resource, ResourceHandle,
    ResourceRegistry, ResourceUsage, Runtime, SysError, SyscallExecutor, Value,
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

// ── a2. Dup2 降级语义固化（审计 R1 契约-F6）──────────────────────────────

/// Dup2 语义因决策 D1（fd 全局单调、永不复用）退化为「先关 new_fd，再复制
/// old_fd 到新 fd」：结果 fd 恒 ≠ new_fd（POSIX dup2 的「精确落到 new_fd」
/// 不可实现）。审计发现全仓库 0 个 dup2 测试——本测试固化降级行为，防止
/// 未来实现漂移（若引入 fd 固定区违反 D1，本断言需随契约裁决同步更新）。
#[tokio::test]
async fn dup2_degrades_to_close_then_dup() {
    let mut ex = TokioExecutor::new();
    let mut reg = ResourceRegistry::new();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dup2.txt");
    std::fs::write(&path, b"dup2-payload").unwrap();

    let v = ex
        .execute(
            &DataOp::Open {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    ..Default::default()
                },
            },
            &mut reg,
        )
        .await
        .unwrap();
    let old_fd = fd_of(&v.0);

    // new_fd 取一个未占用的高编号：降级语义下结果 fd 是单调递增新 fd，非 new_fd。
    let new_fd = 10_000;
    let (v, _undo) = ex
        .execute(&DataOp::Dup2 { old_fd, new_fd }, &mut reg)
        .await
        .unwrap();
    let got = fd_of(&v);
    assert_ne!(
        got, new_fd,
        "D1 单调不复用：dup2 结果 fd 恒 ≠ new_fd（降级语义，文档化）"
    );
    assert!(got > old_fd, "新 fd 单调递增");

    // 降级后的 dup 语义仍成立：新 fd 与 old_fd 共享同一工作对象（读回同内容）。
    let (v, _) = ex
        .execute(&DataOp::Read { fd: got, len: 12 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"dup2-payload".to_vec()));
}

// ── a3. MAX_IO_LEN 超限不泄漏句柄（审计 R2-F3 修复）────────────────────────

/// 超限 len 的 Read 必须在 **take 之前** 拒绝（管道/TCP 路径）：修复前
/// `take_pipe_reader` 已从注册表移除句柄，随后超限早退 → 读半端被 drop
/// （对端 EOF）、注册表条目丢失、映射残留陈旧项 → 同 fd 后续操作 NotFound
/// （句柄被销毁）。修复后：超限 → InvalidInput，同 fd 仍可正常读写。
#[tokio::test]
async fn oversized_len_read_does_not_destroy_pipe_handle() {
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
        other => panic!("{other:?}"),
    };

    // 超限 len → InvalidInput（可捕获错误，非分配 abort）
    let e = exec_err(
        &mut ex,
        &mut reg,
        &DataOp::Read {
            fd: rfd,
            len: usize::MAX / 4,
        },
    )
    .await;
    assert_eq!(e, SysError::InvalidInput, "超限 len 返回 InvalidInput");

    // 句柄未被销毁：写端可写、读端可读回（修复前此处 NotFound/写端 EOF）
    ex.execute(
        &DataOp::Write {
            fd: wfd,
            data: b"still-alive".to_vec(),
        },
        &mut reg,
    )
    .await
    .unwrap();
    let (v, _) = ex
        .execute(&DataOp::Read { fd: rfd, len: 11 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(
        v,
        Value::Bytes(b"still-alive".to_vec()),
        "超限失败后同 fd 仍可读（句柄未被泄漏销毁）"
    );
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

// ── g4. Timeout 取消不毒化 MutexLock id（R-1 MEDIUM 批 8：claim 取消泄漏修复）──
// 蓝图：Timeout{ Sequential{ MutexLock(id) → Sleep(200ms) 慢操作 }, 20ms, on_timeout }。
//
// 语义分叉（审计 R2 适配）：
// - 墙钟（无 virtual-clock）：Sleep 真实 200ms → 20ms 超时触发 → rfc0809 取消传播
//   回滚 MutexLock undo → 锁立即释放，同 id 可重入（RFC-09 目标）；
// - virtual-clock：Sleep 瞬时完成（虚拟推进 10s…本蓝图 200ms → 虚拟 200ms ≥ 20ms
//   → 虚拟通道判定超时），但 run_virtual_timeout 为 post-check 语义（inner 完整执行、
//   效果保留、无取消）→ 锁保留，probe WouldBlock，recover 后释放。

/// 墙钟语义：取消传播回滚 undo → 锁立即释放。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn mutex_claim_released_on_timeout_cancel() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let id = 77u64;
    let claim = vec![ResourceUsage {
        resource: Resource::Fd(id),
        mode: AccessMode::Write,
    }];
    // MutexLock(id) 后接慢操作 Sleep(200ms)：短 duration(20ms) 必 Elapsed。
    let inner = Action::Sequential {
        current: Box::new(Action::Syscall {
            op: DataOp::MutexLock { id },
            resources: claim.clone(),
            next: Box::new(move |_| Action::Sleep {
                duration: Duration::from_millis(200),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let blueprint = Action::Timeout {
        action: Box::new(inner),
        duration: Duration::from_millis(20),
        on_timeout: Box::new(Action::Pure(Value::Unit)),
    };
    // 执行蓝图：20ms 后 Elapsed → 取消传播（rfc0809）：回滚 inner 已入栈 undo
    // （MutexLock 的释放 undo 被立即执行）→ 锁与仲裁占坑释放，on_timeout 生效。
    assert_eq!(rt.run_blocking(blueprint).unwrap(), Value::Unit);
    // 新语义（rfc0809 取消传播）：取消即回滚——占坑已释放，同 id 立即可重入
    // （旧语义「占坑待 recover 释放」已被取消协议取代）。
    let probe = Action::Syscall {
        op: DataOp::MutexLock { id },
        resources: Vec::new(),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    assert_eq!(
        rt.run_blocking(probe).unwrap(),
        Value::Unit,
        "取消传播：超时回滚 undo → 锁立即释放，同 id 可重入（RFC-09 目标）"
    );
    // recover（Replace：先 recover 后执行 target，D10）→ undo 释放占坑与物理锁。
    assert_eq!(
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap(),
        Value::Unit
    );
    // 同一 executor 再次 MutexLock 同 id：占坑已释放 → 成功（非永久 WouldBlock）。
    let again = Action::Syscall {
        op: DataOp::MutexLock { id },
        resources: claim,
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    assert_eq!(rt.run_blocking(again).unwrap(), Value::Unit);
}

/// virtual-clock 语义：post-check 无取消——inner 效果保留（锁保留），probe
/// WouldBlock；recover（Replace）后释放 → 同 id 可重入（非永久毒化）。
#[cfg(feature = "virtual-clock")]
#[test]
fn mutex_claim_kept_on_virtual_timeout_cancel() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let id = 77u64;
    let claim = vec![ResourceUsage {
        resource: Resource::Fd(id),
        mode: AccessMode::Write,
    }];
    let inner = Action::Sequential {
        current: Box::new(Action::Syscall {
            op: DataOp::MutexLock { id },
            resources: claim.clone(),
            next: Box::new(move |_| Action::Sleep {
                duration: Duration::from_millis(200),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let blueprint = Action::Timeout {
        action: Box::new(inner),
        duration: Duration::from_millis(20),
        on_timeout: Box::new(Action::Pure(Value::Unit)),
    };
    // VC：Sleep 瞬时（虚拟 200ms ≥ 20ms → 虚拟通道判定超时，post-check 语义）。
    assert_eq!(rt.run_blocking(blueprint).unwrap(), Value::Unit);
    // inner 效果保留（无取消）：锁仍持有 → 同 id Lock WouldBlock。
    let probe = Action::Syscall {
        op: DataOp::MutexLock { id },
        resources: Vec::new(),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    assert_eq!(
        rt.run_blocking(probe).unwrap_err(),
        SysError::WouldBlock,
        "VC post-check：inner 效果保留，锁未释放（与墙钟取消传播语义分叉）"
    );
    // recover 释放锁 → 同 id 可重入（非永久毒化）。
    assert_eq!(
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap(),
        Value::Unit
    );
    let again = Action::Syscall {
        op: DataOp::MutexLock { id },
        resources: claim,
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    assert_eq!(rt.run_blocking(again).unwrap(), Value::Unit);
}

// ── g5. MutexLock 串行 smoke 50 轮（审计 R4-stress #1 如实化）──
// 审计发现：本测试 8 任务经 Arc<tokio::sync::Mutex<TokioExecutor>> 互斥（锁
// 跨整个 execute 持有）→ 任务间零并发；每轮 fresh id → 轮内无争用；2ms
// timeout 因 op 微秒级完成永不触发（实测 50×8 全程 0.01s）——**不是并发/
// 取消压力测试**（旧注释「高并发多任务…取消与完成路径交错」与实现不符）。
// 实际价值：串行 smoke（无 panic、轮末无占坑泄漏）。真正的取消×仲裁临界区
// 由内部确定性测试（std Mutex 直接持锁构造）与 r3c 8×30 风暴（真超时取消
// 187/240 轮）覆盖。

#[tokio::test]
async fn mutex_lock_timeout_stress_50_rounds_no_panic() {
    let ex = std::sync::Arc::new(tokio::sync::Mutex::new(TokioExecutor::new()));
    for round in 0..50u64 {
        let id = 8000 + round;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let ex = ex.clone();
            tasks.push(tokio::spawn(async move {
                let mut reg = ResourceRegistry::new();
                let mut g = ex.lock().await;
                // 短 Timeout（2ms）包裹 execute：竞争（WouldBlock）与取消混合。
                let res = tokio::time::timeout(
                    Duration::from_millis(2),
                    g.execute(&DataOp::MutexLock { id }, &mut reg),
                )
                .await;
                // 成功路径立即执行 undo（释放占坑与物理锁），保证轮末无残留。
                if let Ok(Ok((_v, Some(undo)))) = res {
                    undo.await;
                }
            }));
        }
        for t in tasks {
            t.await
                .expect("任务 panic（取消 × 完成交错不得 panic/abort）");
        }
        // 轮末：同 id 必须可重新获取（任何取消泄漏 → 永久 WouldBlock）。
        let mut reg = ResourceRegistry::new();
        let mut g = ex.lock().await;
        let (_v, undo) = g
            .execute(&DataOp::MutexLock { id }, &mut reg)
            .await
            .expect("轮末应能重新获取锁（无占坑泄漏）");
        drop(g);
        if let Some(u) = undo {
            u.await;
        }
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

// ── i. blocker-3/RFC-07：Dup 共享管道半端后 IO 成功、关闭 dup 后原 fd 恢复 ──
// 修复前（RFC-07）：管道半端经 Arc::get_mut 独占（take/put_back 轮换），Dup 共享
// → 共享下 get_mut 失败 → InvalidInput；修复后（文件式 Arc<Mutex> 双表）Dup/Fork
// 共享下 lock 可用 → 读/写成功；关闭 dup 释放共享后原 fd 仍可寻址（不丢句柄，
// blocker-3 语义保持）。

#[tokio::test]
async fn pipe_dup_read_shared_then_recover() {
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

    // RFC-07 修复：Dup 共享 Arc<Mutex<ReadHalf>> → 读端 lock 仍可用 → 读成功
    //（修复前共享下 Arc::get_mut 失败 → InvalidInput）。
    let v = ex
        .execute(&DataOp::Dup { fd: rfd }, &mut reg)
        .await
        .unwrap();
    let dup = fd_of(&v.0);
    let (v, _) = ex
        .execute(&DataOp::Read { fd: rfd, len: 4 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"ping".to_vec()), "Dup 共享后读应成功");

    // 关闭 dup 释放共享 → 原 fd 必须仍可读（关闭不丢句柄；修复前共享下
    // 取/放轮换映射错乱 → NotFound 且管道被整体关闭）。
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
async fn pipe_dup_write_shared_then_recover() {
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

    // RFC-07 修复：Dup 共享 Arc<Mutex<WriteHalf>> → 写端 lock 仍可用 → 写成功
    //（修复前共享下 Arc::get_mut 失败 → InvalidInput）。
    let v = ex
        .execute(&DataOp::Dup { fd: wfd }, &mut reg)
        .await
        .unwrap();
    let dup = fd_of(&v.0);
    ex.execute(
        &DataOp::Write {
            fd: wfd,
            data: b"ping".to_vec(),
        },
        &mut reg,
    )
    .await
    .unwrap();

    // 关闭 dup → 原写端恢复可用，数据可达读端（关闭不回归）。
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
        .execute(&DataOp::Read { fd: rfd, len: 8 }, &mut reg)
        .await
        .unwrap();
    assert_eq!(v, Value::Bytes(b"pingpong".to_vec()));
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
