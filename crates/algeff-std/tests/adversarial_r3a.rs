//! R3 对抗审计（第 3 轮，分块 A：Catch/错误恢复组合态 + Scope 边界 +
//! 撤销栈压力）。E2E 外部行为攻击，真实 Runtime + TokioExecutor，零 mock。
//!
//! R1 已覆盖：可逆深链/游标、线性绕过、并发 Fork、错误路径 put_back、
//! 值流、确定性、Scope 3 层 cwd 恢复、Timeout 保留 undo、Alloc 释放。
//! R2 已覆盖：fd 区间、arbiter-MutexLock、Fork 错误传播 + Catch、
//! Timeout 内 Fork / 孤儿分支、Sleep 0 / GetTime、Mmap 边界。
//! **R3a 攻击 R1/R2 未覆盖的组合态**：
//!
//! 1. **Catch/错误恢复组合态**：
//!    - Catch 捕获后 registry/undo 状态（Catch 不触碰撤销栈与注册表，
//!      捕获前累积的 Write undo 原样保留、可被后续 Replace 恢复）；
//!    - Catch 内 Replace：action 部分副作用（Write 已落盘）→ handler 内
//!      Replace = recover + clear → 副作用全部撤销（错误值处理与恢复组合）；
//!    - Catch 嵌套 Catch：内层处理 → 外层跳过；内层 handler 抛新错 →
//!      外层捕获（错误在嵌套 Catch 中的传播语义）；
//!    - Timeout 内 Catch：内层 Catch 处理错误 → Timeout 完成（不走
//!      on_timeout）且 undo 保留；外层 Catch 包 Timeout → on_timeout 自身
//!      出错被外层捕获（超时 + 错误恢复组合）；
//!    - 同一错误蓝图连续 3 次：结果一致（第二次起无状态毒化），3 条 undo
//!      累积后一次 Replace 逆序全恢复。
//! 2. **Scope 边界**：
//!    - Scope 内 Replace：cwd 恢复 + 撤销栈清空 + 句柄释放组合；
//!    - 嵌套 Scope 错误路径被外层 Catch 捕获：各级 cwd 全部恢复 +
//!      handler 在恢复后的 cwd 执行；
//!    - Fork 分支内 Scope（顺序路径，冲突 Fork）：左分支 Scope 错误经
//!      finally 恢复 cwd 后传播 → 外层 Catch；右分支 Scope 成功路径同恢复；
//!      两分支 Write undo 合并回父，后续 Replace 逆序全恢复。
//! 3. **撤销栈压力**：
//!    - 8 文件连续 Write（每文件一次，A4 线性）→ undo 栈 8 条 → Replace
//!      逆序全恢复；
//!    - 同文件两 fd 连续 Write → LIFO 逆序恢复（第二写先撤销、第一写后
//!      撤销，最终回到原始内容——顺序错误会得到不同内容，可判别）；
//!    - D10 复位：Replace 前同 fd 二写被 A4 拦截 → Replace 后同路径重开
//!      再写成功（线性标记清除、句柄释放、fd 单调）。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use algeff_core::{
    AccessMode, Action, DataOp, OpenFlags, ReadOnly, Resource, ResourceInner, ResourceUsage,
    Runtime, SysError, TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

/// 线性绕过（pdr §18：类型状态包装不能完全阻止绕过，运行时拦截是防线）。
fn wu(fd: u64) -> ResourceUsage {
    ResourceUsage {
        resource: Resource::Fd(fd),
        mode: AccessMode::Write,
    }
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

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

/// Open(path, rw+create) → Pure(Fd)。
fn open_fd(rt: &mut Runtime, path: PathBuf) -> u64 {
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path)],
            Action::Pure,
        ))
        .unwrap();
    fd_of(&v)
}

/// 确定的错误 syscall：Read 不存在 fd → NotFound（无 undo、无副作用）。
fn read_missing(fd: u64) -> Action {
    syscall(DataOp::Read { fd, len: 1 }, vec![rd(fd)], Action::Pure)
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1：Catch/错误恢复组合态
// ══════════════════════════════════════════════════════════════════════

/// Catch 捕获后 registry/undo 状态：Catch 只处理错误值，不触碰撤销栈与
/// 注册表——捕获前累积的 Write undo 原样保留、fd 仍可见，随后 Replace
/// 照常恢复（错误恢复与撤销链组合不丢失先前的效果）。
#[test]
fn catch_keeps_undo_and_registry_intact() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("keep.txt");
    std::fs::write(&pa, b"keep-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // Catch 之前的普通副作用：Open + Write（undo 入栈、fd 入注册表）。
    let fd = open_fd(&mut rt, pa.clone());
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"X".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 1);

    // Catch 捕获 Read(999999) 错误 → handler 返回值；undo/registry 不被动。
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(read_missing(999_999)),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "Catch 收到 NotFound");
                Action::Pure(Value::U64(7))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(7), "handler 返回值");
    assert_eq!(
        rt.undo_stack().len(),
        1,
        "Catch 不触碰撤销栈（Write undo 保留）"
    );
    assert!(rt.registry().lookup(fd).is_some(), "Catch 不释放注册表句柄");
    assert_eq!(std::fs::read(&pa).unwrap(), b"Xeep-original", "写已生效");

    // 错误捕获后 Replace：先前 Write 的 undo 仍可完整恢复。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"keep-original",
        "Catch 之后 Replace 恢复先前写"
    );
    assert!(rt.registry().lookup(fd).is_none(), "Replace 释放句柄");
}

/// Catch 内 Replace：action 已产生部分副作用（Open+Write 落盘）后出错 →
/// handler 内 Replace（recover + clear）把部分副作用全部撤销。验证
/// 「错误处理动作」与「恢复动作」在 Catch 内的组合语义。
#[test]
fn catch_handler_replace_recovers_partial_effects() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("partial.txt");
    std::fs::write(&pa, b"partial-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(pa.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"WXYZ".to_vec(),
                        },
                        vec![wr(fd)],
                        move |_| read_missing(999_999),
                    )
                },
            )),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "action 错误传给 handler");
                // handler 内 Replace：recover（撤掉 action 的部分 Write）+ clear。
                Action::Replace {
                    target: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
                }
            }),
        })
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "handler 内 Replace 执行 target");
    assert!(rt.undo_stack().is_empty(), "handler 内 Replace 清空撤销栈");
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"partial-original",
        "action 的部分写副作用被 handler 内 Replace 撤销"
    );
    assert!(
        rt.registry().lookup(0).is_none(),
        "Open fd(0) 句柄已被 Replace 释放"
    );
}

/// 嵌套 Catch：(a) 内层捕获 → Ok → 外层不触发；(b) 内层 handler 自身抛新错
/// → 外层捕获。验证错误在嵌套 Catch 间的传播与短路语义。
#[test]
fn catch_nested_inner_handled_outer_skipped_then_inner_handler_errors() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let outer_fired = Arc::new(std::sync::Mutex::new(0u32));

    // (a) 内层处理成功 → 外层 handler 不应执行。
    let of = Arc::clone(&outer_fired);
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Catch {
                action: Box::new(read_missing(999_999)),
                handler: Box::new(|e| {
                    assert_eq!(e, SysError::NotFound);
                    Action::Pure(Value::U64(11))
                }),
            }),
            handler: Box::new(move |_e| {
                *of.lock().unwrap() += 1;
                Action::Pure(Value::U64(999))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(11), "内层已处理，外层短路跳过");
    assert_eq!(*outer_fired.lock().unwrap(), 0, "外层 handler 未被触发");

    // (b) 内层 handler 抛新错（Read(888888)）→ 外层捕获。
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Catch {
                action: Box::new(read_missing(999_999)),
                handler: Box::new(|_| read_missing(888_888)),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "内层 handler 的新错误被外层捕获");
                Action::Pure(Value::U64(22))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(22));
    assert!(rt.undo_stack().is_empty(), "Read 错误不产生 undo");
}

/// Timeout 内 Catch：(a) 内层 Catch 处理错误 → Timeout 完成（不走 on_timeout）
/// 且 Write undo 保留（Timeout 与 Catch 组合不吞撤销链）；(b) 外层 Catch 包
/// Timeout：超时触发后 on_timeout 自身出错 → 外层 Catch 捕获（超时 + 恢复
/// 组合态）。
#[test]
fn catch_inside_timeout_and_timeout_error_caught() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("tc.txt");
    std::fs::write(&pa, b"tc-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // (a) Timeout(2s) 内 Open+Write+Catch(Read 错) → 毫秒级完成 → 31。
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(pa.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"TC!".to_vec(),
                        },
                        vec![wr(fd)],
                        move |_| Action::Catch {
                            action: Box::new(read_missing(999_999)),
                            handler: Box::new(|e| {
                                assert_eq!(e, SysError::NotFound);
                                Action::Pure(Value::U64(31))
                            }),
                        },
                    )
                },
            )),
            duration: Duration::from_secs(2),
            on_timeout: Box::new(Action::Pure(Value::U64(99))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(31), "内层 Catch 已处理，不触发 on_timeout");
    assert_eq!(rt.undo_stack().len(), 1, "Timeout 内 Write undo 保留");
    assert_eq!(std::fs::read(&pa).unwrap(), b"TC!original", "写已生效");
    // 组合路径撤销链完整：随后 Replace 恢复。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"tc-original",
        "Timeout+Catch 后撤销链完整"
    );

    // (b) Catch 包 Timeout：Sleep(10s) 被 50ms 超时打断 → on_timeout 的
    //     Read(777777) 出错 → Timeout 返回 Err → 外层 Catch 捕获。
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Timeout {
                action: Box::new(Action::Sleep {
                    duration: Duration::from_secs(10),
                    next: Box::new(Action::Pure),
                }),
                duration: Duration::from_millis(50),
                on_timeout: Box::new(read_missing(777_777)),
            }),
            handler: Box::new(|e| {
                assert_eq!(
                    e,
                    SysError::NotFound,
                    "on_timeout 自身的错误传播到外层 Catch"
                );
                Action::Pure(Value::U64(32))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(32));
    assert!(rt.undo_stack().is_empty(), "错误路径无 undo");
}

/// 同一错误蓝图连续执行 3 次（状态毒化检查）：每次结果一致（第二次起
/// 结果相同）、捕获的错误码一致、写副作用一致；3 条 Write undo 累积后
/// 一次 Replace 逆序全恢复。
///
/// 构造说明：蓝图对路径参数化（每次新文件）——A4 路径级线性要求同一
/// 路径 Write 至多一次，参数化使「同一错误蓝图」在无 Replace 情况下可
/// 连续执行并累积 undo，恰为「错误重复执行不残留状态」的最小真实场景。
#[test]
fn catch_same_error_blueprint_3_runs_no_poison() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for k in 0..3u8 {
        files.push(dir.path().join(format!("blueprint-{k}.txt")));
        std::fs::write(&files[k as usize], b"original").unwrap();
    }
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    for k in 1..=3u64 {
        let p = files[(k - 1) as usize].clone();
        let payload = format!("run{k:03}!!").into_bytes();
        let p_in = p.clone();
        let payload_in = payload.clone();
        let v = rt
            .run_blocking(Action::Catch {
                action: Box::new(syscall(
                    DataOp::Open {
                        path: p_in.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(p_in.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        syscall(
                            DataOp::Write {
                                fd,
                                data: payload_in,
                            },
                            vec![wr(fd)],
                            move |_| read_missing(999_999),
                        )
                    },
                )),
                handler: Box::new(move |e| {
                    assert_eq!(e, SysError::NotFound, "第 {k} 次蓝图错误码一致");
                    Action::Pure(Value::U64(42))
                }),
            })
            .unwrap();
        assert_eq!(v, Value::U64(42), "第 {k} 次执行结果一致（无状态毒化）");
        assert_eq!(
            rt.undo_stack().len(),
            k as usize,
            "第 {k} 次 undo 累积 {k} 条（Catch 不吞栈）"
        );
        assert_eq!(std::fs::read(&p).unwrap(), payload, "第 {k} 次写生效");
    }

    // 3 条 undo 一次 Replace 逆序全恢复（LIFO：第 3 写先撤、第 1 写最后撤）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    for k in 0..3u8 {
        assert_eq!(
            std::fs::read(&files[k as usize]).unwrap(),
            b"original",
            "第 {k} 文件：重复错误蓝图后撤销链逆序全恢复"
        );
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：Scope 边界
// ══════════════════════════════════════════════════════════════════════

/// Scope 内 Replace：cwd 在 Replace（recover+clear）执行后、Scope 退出时
/// 恢复；撤销栈清空、句柄释放、文件内容恢复——Scope 与 Replace 组合态。
#[test]
fn scope_inner_replace_restores_cwd_and_state() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("sc-replace.txt");
    std::fs::write(&pa, b"sc-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let v = rt
        .run_blocking(Action::Scope {
            base: PathBuf::from("sub/scope"),
            inner: Box::new(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(pa.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"REPL".to_vec(),
                        },
                        vec![wr(fd)],
                        move |_| Action::Replace {
                            target: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
                        },
                    )
                },
            )),
            next: Box::new(|_| Action::Pure(Value::U64(1))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1), "Scope next 在退出后执行");
    assert_eq!(rt.context().cwd, before, "Scope 内 Replace 后 cwd 恢复");
    assert!(
        rt.undo_stack().is_empty(),
        "Scope 内 Replace 已 recover 清栈"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"sc-original",
        "Scope 内 Replace 撤销了写"
    );
    assert!(
        rt.registry().lookup(0).is_none(),
        "Scope 内 Replace 释放句柄"
    );
}

/// 嵌套 Scope 错误路径 + 外层 Catch：两级 Scope 出错后 cwd 全恢复，错误被
/// 外层 Catch 捕获，handler 在恢复后的 cwd 执行（R1 已测裸嵌套 Scope 错误
/// cwd 恢复；本测试加 Catch 组合，验证错误在恢复后继续被处理）。
#[test]
fn nested_scope_error_caught_cwd_all_restored() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Scope {
                base: PathBuf::from("lvl-a"),
                inner: Box::new(Action::Scope {
                    base: PathBuf::from("lvl-b"),
                    inner: Box::new(read_missing(999_999)),
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "嵌套 Scope 错误传播到 Catch");
                syscall(DataOp::GetTime, vec![], Action::Pure)
            }),
        })
        .unwrap();
    assert!(
        matches!(v, Value::U64(_)),
        "handler 在 cwd 恢复后执行 GetTime"
    );
    assert_eq!(
        rt.context().cwd,
        before,
        "嵌套 Scope 错误路径 cwd 全恢复（Catch 组合）"
    );
    assert!(rt.undo_stack().is_empty(), "Read 错误无 undo");
}

/// Fork 分支内 Scope（顺序路径，冲突 Fork）：左分支 Scope 内 Write + 读错 →
/// 错误经 Scope finally 恢复 cwd 后传播 → 外层 Catch；右分支 Scope 内 Write
/// 成功（cwd 同样恢复）。两分支 Write undo 合并回父，Replace 逆序全恢复。
#[test]
fn fork_branch_scope_error_cwd_restored_effects_merged() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("fb-a.txt");
    std::fs::write(&pa, b"ABCDEFGH").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa.clone())],
            move |v| {
                let fd = fd_of(&v);
                // 两分支同 fd Write → 静态冲突 → 顺序路径（left→right，共享 ctx
                // 与 undo 栈）；每分支内嵌 Scope（base 不同）。
                Action::Catch {
                    action: Box::new(Action::Fork {
                        left: Box::new(Action::Scope {
                            base: PathBuf::from("dl"),
                            inner: Box::new(syscall(
                                DataOp::Write {
                                    fd,
                                    data: b"L".to_vec(),
                                },
                                vec![wu(fd)],
                                move |_| read_missing(999_999),
                            )),
                            next: Box::new(|_| Action::Pure(Value::Unit)),
                        }),
                        right: Box::new(Action::Scope {
                            base: PathBuf::from("dr"),
                            inner: Box::new(syscall(
                                DataOp::Write {
                                    fd,
                                    data: b"R".to_vec(),
                                },
                                vec![wu(fd)],
                                Action::Pure,
                            )),
                            next: Box::new(|_| Action::Pure(Value::Unit)),
                        }),
                        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
                    }),
                    handler: Box::new(|e| {
                        assert_eq!(e, SysError::NotFound, "左分支 Scope 内错误传播到外层 Catch");
                        Action::Pure(Value::U64(5))
                    }),
                }
            },
        ))
        .unwrap();
    assert_eq!(v, Value::U64(5), "Catch 捕获分支内错误");
    assert_eq!(
        rt.context().cwd,
        before,
        "Fork 分支内 Scope（错误/成功两路径）退出后 cwd 均恢复"
    );
    assert_eq!(rt.undo_stack().len(), 2, "两分支 Write undo 合并回父");
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"LRCDEFGH",
        "顺序路径 left 写 L、right 写 R（游标共享）"
    );

    // 分支 undo 逆序全恢复：先右分支（R→LBCDEFGH）再左分支（L→ABCDEFGH）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"ABCDEFGH",
        "Fork 分支 undo 逆序全恢复"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：撤销栈压力
// ══════════════════════════════════════════════════════════════════════

/// 8 文件连续 Write（每文件一次，A4 线性：每 fd 一写）→ undo 栈 8 条 →
/// Replace 逆序全恢复（长度、内容、句柄三层断言）。R1 只覆盖 2 文件 × 3 轮，
/// 本测试 8 文件规模 + 混合长短 payload（触发截断/扩展 undo 路径）。
#[test]
fn undo_8_files_consecutive_writes_all_inverse_restored() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut originals = Vec::new();
    for i in 0..8u8 {
        let p = dir.path().join(format!("m{i}.txt"));
        let orig = format!("original-{i:02}").into_bytes();
        std::fs::write(&p, &orig).unwrap();
        files.push(p);
        originals.push(orig);
    }
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 连续 8 文件各写一次；payload 长短混合（偶数长于原内容、奇数短于原内容）。
    for i in 0..8u64 {
        let p = files[i as usize].clone();
        let payload = if i % 2 == 0 {
            format!("payload-{i:04}-data").into_bytes() // 15 字节 > 原 11 字节
        } else {
            format!("p{i}").into_bytes() // 2 字节 < 原 11 字节
        };
        // POSIX 游标写（不截断）：短 payload 在尾部留下原内容残段。
        let mut expect = payload.clone();
        if payload.len() < originals[i as usize].len() {
            expect.extend_from_slice(&originals[i as usize][payload.len()..]);
        }
        let payload_in = payload.clone();
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: p.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(p.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: payload_in,
                        },
                        vec![wr(fd)],
                        Action::Pure,
                    )
                },
            ))
            .unwrap();
        assert_eq!(v, Value::Unit);
        assert_eq!(
            std::fs::read(&files[i as usize]).unwrap(),
            expect,
            "第 {i} 文件写生效（游标覆盖语义）"
        );
    }
    assert_eq!(rt.undo_stack().len(), 8, "8 条 undo 全部压栈");

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "Replace 清空 8 条 undo");
    for i in 0..8u64 {
        assert_eq!(
            std::fs::read(&files[i as usize]).unwrap(),
            originals[i as usize],
            "第 {i} 文件逆序撤销后恢复原内容（含长度截断恢复）"
        );
    }
}

/// 同文件两 fd 连续 Write（撤销栈压力 + LIFO 逆序判别）：第二 fd 的写基于
/// 第一写后的状态；Replace 恢复必须严格逆序——先撤第二写、再撤第一写。
/// 若实现按 FIFO 恢复，终态会得到 "12CDEFGH" 而非 "ABCDEFGH"（可判别）。
///
/// 构造说明：同一路径两次 Open(write) 会被 A4 路径级线性拦截（InvalidInput），
/// 故用**硬链接**（同 inode、两条路径、两个独立资源身份）获得指向同一物理
/// 文件的两个 fd——两条 undo 作用于同一 inode，LIFO 恢复顺序可判别。
#[test]
fn undo_same_file_two_fds_lifo_recover_inverse_order() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("two-fds.txt");
    let pb = dir.path().join("two-fds-link.txt");
    std::fs::write(&pa, b"ABCDEFGH").unwrap();
    std::fs::hard_link(&pa, &pb).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd1 = open_fd(&mut rt, pa.clone());
    let fd2 = open_fd(&mut rt, pb.clone());
    assert_ne!(fd1, fd2, "两个打开各自独立 fd（同一 inode）");

    // 第一写 "123"（游标 0）→ "123DEFGH"；第二写 "XY"（游标 0）→ "XY3DEFGH"。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd1,
            data: b"123".to_vec(),
        },
        vec![wr(fd1)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd2,
            data: b"XY".to_vec(),
        },
        vec![wr(fd2)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"XY3DEFGH",
        "两写叠加生效（硬链接同 inode）"
    );
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "同文件两条 undo（不同 fd 各一次）"
    );

    // LIFO：第二写 undo 先执行（→"123DEFGH"），第一写 undo 后执行（→"ABCDEFGH"）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"ABCDEFGH",
        "同文件双 undo 按 LIFO 逆序恢复（FIFO 会得到 12CDEFGH）"
    );
    assert!(rt.registry().lookup(fd1).is_none(), "fd1 句柄释放");
    assert!(rt.registry().lookup(fd2).is_none(), "fd2 句柄释放");
}

/// D10 复位验证：Replace（recover + clear）清除线性标记与句柄后，同资源
/// （同路径）可再 Write——Replace 前同 fd 二写被 A4 拦截，Replace 后重开
/// 再写成功且 fd 单调不复用。
#[test]
fn d10_replace_resets_linearity_same_resource_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("d10.txt");
    std::fs::write(&pa, b"d10-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd1 = open_fd(&mut rt, pa.clone());
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd1,
            data: b"Z".to_vec(),
        },
        vec![wr(fd1)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&pa).unwrap(), b"Z10-original");
    assert_eq!(rt.undo_stack().len(), 1);

    // Replace 前同 fd 二次 Write：use 语义允许（独立 undo 入栈）。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd1,
            data: b"W".to_vec(),
        },
        vec![wr(fd1)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 2, "二写各一个独立 undo");

    // Replace = recover + clear：文件恢复、句柄释放、线性标记清除（D10 复位）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"d10-original");
    assert!(rt.registry().lookup(fd1).is_none(), "旧 fd 句柄已释放");

    // 同资源（同路径）再 Write：新 fd 单调不复用（D1），不再被 A4 拦截。
    let fd2 = open_fd(&mut rt, pa.clone());
    assert!(fd2 > fd1, "fd 单调递增不复用（D1）");
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd2,
            data: b"Q".to_vec(),
        },
        vec![wr(fd2)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"Q10-original",
        "Replace 复位后同资源再写成功（线性标记已清除）"
    );
    assert_eq!(rt.undo_stack().len(), 1, "再写产生新 undo");
}
