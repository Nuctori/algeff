//! 修复后语义锁定（与 gap 文档 spec/semantic-undo-gaps.md 对应）：
//!
//! 1. **write-only 句柄写 → 写前读失败 → Err**：无法构造逆 → 报错（不再静默降级）。
//! 2. **undo 闭包失败上报**：mkdir 逆（remove_dir 非空失败）→ recover 返回 Err。
//! 3. **A4 use/move 拆分（已反转）**：Write 是 use 语义可多次（独立 undo +
//!    LIFO 撤销）；Own 是 move 语义终结一次。二写允许且撤销正确。
//! 4. **open+create 逆 = unlink（P1 已补）**：新建文件 Replace 后删除（真回归，
//!    不再静默残留）。

use std::path::PathBuf;

use algeff_core::{
    Action, DataOp, IdempotencyStatus, OpenFlags, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R4 相同约定）──────────────

fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
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

// ══════════════════════════════════════════════════════════════════════
// 修复 1：write-only 句柄 → 写前读失败 → 无法构造逆 → Err（不再静默降级）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn write_only_fd_write_rejected_when_undo_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("wo.txt");
    std::fs::write(&p, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // write-only 打开（无 read）→ op_write 写前读失败（Windows ACCESS_DENIED
    // / Unix EBADF）→ 无法构造逆 → Err（语义真回归：不静默降级）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd,
                data: b"CHANGED".to_vec(),
            },
            vec![wr(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::PermissionDenied,
        "写前读失败（无法构造撤销）→ 必须报错，而非带副作用无声成功"
    );
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"original".to_vec(),
        "写未生效（Err 前无副作用）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 修复 2：mkdir 逆（remove_dir）+ create 逆（unlink）组合 → 完全回归
// ══════════════════════════════════════════════════════════════════════

#[test]
fn mkdir_inverse_removes_dir_when_emptied_by_create_undo() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("sub");
    let f = d.join("file.txt");

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let f1 = f.clone();
    rt.run_blocking(do_! {
        dx::mkdir(&d, 0o755);
        let fd = dx::open(
            &f1,
            OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            },
        );
        dx::write(&fd, b"data".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    assert!(d.exists(), "前置：目录已创建");

    // Replace = recover（LIFO）：write 逆 → create 逆（unlink 删除文件，目录变空）
    // → mkdir 逆（remove_dir 成功）→ 完全回归：目录与文件都被删除。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(
        !d.exists() && !f.exists(),
        "create 逆删除文件 → 目录空 → mkdir 逆成功 → 完全回归（真回归）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 偏差 3：A4 过度拒绝——顺序多次 Write 同 fd（运行时本可确保每次独立撤销）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn sequential_multi_write_same_fd_allowed_use_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("multi.txt");
    std::fs::write(&p, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    // 第一次写：成功，Full 撤销入栈。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"first".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 1, "首次写有 undo");

    // 第二次写：允许（A4 use/move 拆分，D-0xx：Write 是 use 语义可多次）
    // → 独立 undo 入栈（写前读第二次覆盖区域）。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"second".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 2, "两次写各一个独立 undo");

    // LIFO 撤销正确：先还原第二次写（写回 first 覆盖区），再还原第一次
    // （写回 original 覆盖区）→ 回到 open 前状态（真回归）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"original".to_vec(),
        "两次写都被独立撤销（LIFO，语义真回归）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// P2：静态代数角色分类（DataOp::role）+ 显式不可逆声明（dx::irreversible）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn mutex_reentry_still_blocked_by_arbiter_after_a4_use_semantics() {
    // A4 use/move 拆分（Write 放宽为不限次数）后，互斥锁防重入仍由仲裁器
    // （A7 原子占坑）独立保证——同 id 二次 MutexLock 在仲裁层 WouldBlock，
    // 不依赖 Write 消费（blocker-3 独立验证）。
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    rt.run_blocking(syscall(DataOp::MutexLock { id: 9 }, vec![], Action::Pure))
        .unwrap();
    let e = rt
        .run_blocking(syscall(DataOp::MutexLock { id: 9 }, vec![], Action::Pure))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::WouldBlock,
        "同 id 二次 MutexLock → A7 仲裁 WouldBlock（不依赖 Write 消费）"
    );
}

#[test]
fn dataop_static_role_direct() {
    use algeff_core::{DataOp, OpenFlags, UndoRole};
    use std::path::PathBuf;

    let p = PathBuf::from("/x");
    assert_eq!(
        DataOp::Stat { path: p.clone() }.role(),
        UndoRole::Identity,
        "Stat 无副作用 → Identity"
    );
    assert_eq!(
        DataOp::Write {
            fd: 1,
            data: b"d".to_vec()
        }
        .role(),
        UndoRole::Invertible,
        "Write 可逆 → Invertible（静态，运行时写前读决定）"
    );
    assert_eq!(
        DataOp::Unlink { path: p.clone() }.role(),
        UndoRole::NonInvertible,
        "Unlink 删除不可逆 → NonInvertible"
    );
    assert_eq!(
        DataOp::Open {
            path: p,
            flags: OpenFlags::default()
        }
        .role(),
        UndoRole::Invertible,
        "Open 静态可逆（运行时按 flags/existed 细分）"
    );
    assert!(!DataOp::GetTime.is_deterministic(), "GetTime 不确定（P3）");
    assert!(
        DataOp::Stat {
            path: PathBuf::from("/y")
        }
        .is_deterministic(),
        "Stat 确定（P3）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 修复 4：open+create 逆 = unlink（P1）→ Replace 完全回归
#[test]
fn create_open_inverse_removes_new_file_on_replace() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("created.txt");
    assert!(!f.exists());

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let p = f.clone();
    rt.run_blocking(do_! {
        let fd = dx::open(
            &p,
            OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            },
        );
        dx::write(&fd, b"data".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    assert!(f.exists(), "前置：文件已创建");

    // Open(create) 逆 = unlink（文件原不存在时，P1 已补）→ Replace recover
    // 执行 write 逆 + unlink 逆 → 新建文件被删除（真回归：回到 open 前状态）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(
        !f.exists(),
        "Replace 后新建文件被删除（create 逆生效，真回归）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// D-0xx 幂等键状态机：重试去重 + 恰好一次（COMMITTED → REVERTED → 可重执行）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn idempotent_key_retry_returns_cached_result_without_reexecuting() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("idem.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let p1 = p.clone();

    // 带幂等键的副作用段：写文件（非幂等效应——重复执行会重复覆盖）。
    let make_effect = || {
        dx::idempotent(
            "charge:order-42",
            do_! {
                let fd = dx::open(
                    &p1,
                    OpenFlags {
                        read: true,
                        write: true,
                        create: true,
                        ..Default::default()
                    },
                );
                dx::write(&fd, b"charged".to_vec());
                dx::close(&fd);
                Value::U64(42) // 效应结果（如扣款单号）
            },
        )
    };

    // 第一次执行：真正发生副作用，键 COMMITTED + 缓存结果。
    let v1 = rt.run_blocking(make_effect()).unwrap();
    assert_eq!(v1, Value::U64(42));
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "charged");

    // 第二次执行（重试）：键 COMMITTED → 返回缓存，不重新执行（undo 栈不长）。
    let stack_len_before = rt.undo_stack().len();
    let v2 = rt.run_blocking(make_effect()).unwrap();
    assert_eq!(v2, Value::U64(42), "重试返回缓存结果");
    assert_eq!(
        rt.undo_stack().len(),
        stack_len_before,
        "重试不触发逆函数、不压新 undo（从未真正'新执行'）"
    );
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "charged");
}

#[test]
fn idempotent_key_exactly_once_across_replace() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("init.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let p1 = p.clone();

    let make_init = || {
        dx::idempotent(
            "init:db-schema",
            do_! {
                let fd = dx::open(
                    &p1,
                    OpenFlags {
                        read: true,
                        write: true,
                        create: true,
                        ..Default::default()
                    },
                );
                dx::write(&fd, b"schema-v1".to_vec());
                dx::close(&fd);
                Value::Unit
            },
        )
    };

    // 第一次：副作用发生，键 COMMITTED。
    rt.run_blocking(make_init()).unwrap();
    assert_eq!(
        rt.context()
            .idempotency
            .lock()
            .unwrap()
            .status_of("init:db-schema"),
        Some(IdempotencyStatus::Committed),
        "执行后键 COMMITTED"
    );

    // Replace：撤销该段副作用 → REVERT undo 执行 → 键 REVERTED。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        rt.context()
            .idempotency
            .lock()
            .unwrap()
            .status_of("init:db-schema"),
        Some(IdempotencyStatus::Reverted),
        "Replace 撤销后键 REVERTED（允许未来重执行——恰好一次语义）"
    );
    assert!(!p.exists(), "Replace 撤销副作用：create 逆删除新建文件");

    // REVERTED → 重新执行：副作用再次真正发生（热重载语义：卸载后重载可重新初始化）。
    rt.run_blocking(make_init()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        "schema-v1",
        "REVERTED 后重执行生效"
    );
    assert_eq!(
        rt.context()
            .idempotency
            .lock()
            .unwrap()
            .status_of("init:db-schema"),
        Some(IdempotencyStatus::Committed),
        "重执行后再次 COMMITTED"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 审计 blocker 回归：幂等段 inner 内嵌套 Replace → 不假 COMMIT（恰好一次保持）
// ══════════════════════════════════════════════════════════════════════

#[test]
fn idempotent_inner_replace_does_not_commit_stale() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("idem-replace.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let p1 = p.clone();

    // 幂等段 inner 内含 Replace：副作用被内部 Replace 自清理。
    // 断言：不 COMMIT（key 未记录）→ 重试重新执行，无假缓存。
    // 注：do_! 尾表达式必须是 Value，Replace 用 and_then 组合；Action 不可 Clone，
    // 重试用闭包重新构造。
    let make_effect = || {
        let p1 = p1.clone();
        dx::idempotent(
            "effect:inner-replace-2",
            dx::and_then(
                do_! {
                    let fd = dx::open(
                        &p1,
                        OpenFlags {
                            read: true,
                            write: true,
                            create: true,
                            ..Default::default()
                        },
                    );
                    dx::write(&fd, b"tmp".to_vec());
                    dx::close(&fd);
                    Value::Unit
                },
                |_| Action::Replace {
                    target: Box::new(Action::Pure(Value::U64(7))),
                },
            ),
        )
    };

    // 第一次执行：inner 先做副作用（写文件）→ 内部 Replace 撤销全部 → 文件不存在。
    let v = rt.run_blocking(make_effect()).unwrap();
    assert_eq!(v, Value::U64(7));
    assert!(
        !p.exists(),
        "内部 Replace 撤销副作用：新建文件被删除（create 逆）"
    );
    assert_eq!(
        rt.context()
            .idempotency
            .lock()
            .unwrap()
            .status_of("effect:inner-replace-2"),
        None,
        "内部 Replace 自清理 → 不 COMMIT（key 未记录，防假缓存）"
    );

    // 重试：key 未记录 → 重新执行（inner 的 Replace 再次自清理，无重复副作用）。
    let v2 = rt.run_blocking(make_effect()).unwrap();
    assert_eq!(v2, Value::U64(7));
    assert!(!p.exists(), "重试重新执行，内部 Replace 再次自清理");
}

#[test]
fn fork_conflict_same_idempotency_key_serialized() {
    use algeff_core::runtime::fork_conflict;

    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.txt");
    let p2 = dir.path().join("b.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 两分支各做一个同 key 的幂等段（资源不相交——纯幂等键冲突判定）。
    let left = dx::idempotent(
        "effect:once",
        do_! {
            let fd = dx::open(
                &p1,
                OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            );
            dx::write(&fd, b"L".to_vec());
            dx::close(&fd);
            Value::Unit
        },
    );
    let right = dx::idempotent(
        "effect:once",
        do_! {
            let fd = dx::open(
                &p2,
                OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            );
            dx::write(&fd, b"R".to_vec());
            dx::close(&fd);
            Value::Unit
        },
    );

    // 静态冲突判定：同幂等键 → can_parallel=false → 串行。
    assert!(
        fork_conflict(rt.registry(), &left, &right),
        "同幂等键两分支 → 冲突（串行，防并行重复执行破坏恰好一次）"
    );

    // 执行：串行路径下第一个 COMMITTED，第二个命中缓存不重执行——
    // 但注意：第二个是独立幂等段（同 key），串行执行时第二个查表 COMMITTED
    // → 跳过 inner → 只第一个真实执行。
    rt.run_blocking(Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    })
    .unwrap();
    // 恰好一次：同 key 只一个真实执行（一个文件有内容）。
    let l_ok = std::fs::read_to_string(&p1).unwrap_or_default() == "L";
    let r_ok = std::fs::read_to_string(&p2).unwrap_or_default() == "R";
    assert!(
        l_ok ^ r_ok,
        "同幂等键恰好一次：只有一个分支真实执行（串行 + 键去重）"
    );
}

#[test]
fn replace_rejected_then_reusable_same_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("reject-then-ok.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 段 1：管道写（NonInvertible，不可回滚）→ Replace 拒绝。
    let (_rfd, wfd) = {
        let v = rt.run_blocking(dx::pipe_open()).unwrap();
        match v {
            Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
            other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
        }
    };
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: wfd,
            data: b"x".to_vec(),
        },
        vec![wr(wfd)],
        Action::Pure,
    ))
    .unwrap();
    let e = rt
        .run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap_err();
    assert_eq!(
        e,
        SysError::PermissionDenied,
        "含管道写（NonInvertible）→ Replace 拒绝"
    );

    // 段 2：全新纯可逆段 → Replace 必须成功（MEDIUM-2 修复：拒绝后 flag 已重置，
    // 不再永久楔死）。
    std::fs::write(&p, "before").unwrap();
    rt.run_blocking(do_! {
        let fd = dx::open(
            &p,
            OpenFlags {
                read: true,
                write: true,
                ..Default::default()
            },
        );
        dx::write(&fd, b"temporary".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&p).unwrap(),
        "before",
        "拒绝后新段 Replace 成功（flag 已重置，可逆部分恢复）"
    );
}
