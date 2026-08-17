//! R6-F2 对抗修复回归（线性标记失败路径毒化，RFC-12）—— algeff-core 部分。
//!
//! 攻击方法论：与 R4b 相同的「真实 Runtime 全链路」约定，正反面样本由
//! **自定义假执行器**驱动（D9 契约独立性：解释器不依赖 TokioExecutor 的
//! 具体行为，任意满足 `SyscallExecutor` 契约的执行器都受同一修复约束）。
//!
//! 本文件锁定 R6-F2 缺陷（audit/r6 分支 adversarial_r6.rs §3 已证实的**同路径
//! 盲区**）修复后的行为：
//!
//! - 失败 Open(w)（如 exclusive 撞已存在 / 目录当文件写）→ 回滚路径 Write
//!   标记 → 同路径 Write 模式重试成功（修复前 InvalidInput）；
//! - 失败 Write(fd)（如只读 fd 写）→ 回滚 fd Write 标记 → 重试成功且仍保持
//!   至多一次（再写仍被 A4 拦截，标记计数不重复消费）；
//! - 失败 Own(Close) → 回滚 Own 终结标记 → fd 仍可继续使用（修复前
//!   InvalidInput）；
//! - 回滚只作用于本次失败 syscall 预插入的标记：早前成功 syscall 的消费记录
//!   不受影响（A4 成功路径语义不变）；Read/Append 不插标记、回滚无操作。
//! - 错误路径契约不变：物理错误原样透传、不压 undo。
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 与 `run_blocking` 在 tokio 上下文之外调用。

use std::collections::VecDeque;
use std::path::PathBuf;

use algeff_core::{
    Action, BoxFuture, DataOp, OpenFlags, Owned, ReadOnly, ResourceInner, ResourceRegistry,
    ResourceUsage, Runtime, SysError, SyscallExecutor, TypedResource, UndoCapability, Value, WriteOnly,
};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R4b 相同约定）──────────────

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

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

/// 脚本执行器：按调用队列依次返回预设结果；队列耗尽后返回 Ok(Unit) 兜底
/// （被 A4 拦截的调用不会到达执行器，故兜底在断言「重试后仍至多一次」时
/// 永远不会被消费）。`&mut self` 顺序访问即天然互斥（解释器单线程 trampoline）。
struct ScriptedExecutor {
    results: VecDeque<Result<(Value, UndoCapability), SysError>>,
}

impl SyscallExecutor for ScriptedExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        let r = self.results.pop_front().unwrap_or(Ok((Value::Unit, UndoCapability::Identity)));
        Box::pin(async move { r })
    }
}

// ══════════════════════════════════════════════════════════════════════
// RFC-12（R6-F2）：失败 syscall 回滚预插入的线性消费标记
// ══════════════════════════════════════════════════════════════════════

#[test]
fn failed_open_write_rolls_back_path_marker_same_path_retry_ok() {
    // 失败 Open(w)（如 exclusive 撞已存在）：check_linear 已插入路径 P 的
    // Write 标记 → 修复前残留 → 同路径 Write 模式重试被 A4 误拒 InvalidInput。
    let mut rt = Runtime::new(Box::new(ScriptedExecutor {
        results: VecDeque::from([
            Err(SysError::AlreadyExists), // 首次 Open(w) 物理失败
            Ok((Value::Fd(7), UndoCapability::Identity)),     // 重试 Open(rw) 成功
        ]),
    }));
    let p = PathBuf::from("/same/path.txt");

    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::AlreadyExists, "物理错误原样透传");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");
    assert!(rt.registry().lookup(0).is_none(), "失败不分配句柄");

    // 修复后：同路径 Write 模式重开成功（本次预插入的 Write 标记已回滚）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Fd(7), "同路径 Write 重试成功，不被 A4 误拒");
}

#[test]
fn failed_write_on_fd_rolls_back_then_retry_ok_and_at_most_once_kept() {
    // 失败 Write(fd)（如只读 fd 写）：回滚 fd 的 Write 标记 → 重试成功；
    // 重试成功后 Write 仍恰好消费一次（第三次 Write 被 A4 拦截——标记计数
    // 不因「失败-回滚-重试」而错乱）。
    let mut rt = Runtime::new(Box::new(ScriptedExecutor {
        results: VecDeque::from([
            Ok((Value::Fd(0), UndoCapability::Identity)),        // Open(rw) P 成功 → fd 0
            Err(SysError::PermissionDenied), // Write(fd 0) 物理失败（只读 fd 写）
            Ok((Value::U64(4), UndoCapability::Identity)),       // 重试 Write(fd 0) 成功
        ]),
    }));
    let p = PathBuf::from("/p.txt");

    let fd = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(fd, Value::Fd(0), "Open 成功返回 fd 0");

    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: 0,
                data: b"x".to_vec(),
            },
            vec![wr(0)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::PermissionDenied, "物理写失败原样透传");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // 修复后：同 fd 重试 Write 成功（修复前 fd 的 Write 标记残留 → InvalidInput）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: 0,
                data: b"y".to_vec(),
            },
            vec![wr(0)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::U64(4), "同 fd 重试 Write 成功");

    // A4 至多一次在重试成功后仍然成立（第三次 Write 被解释器拦截，不触达执行器）。
    let e2 = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: 0,
                data: b"z".to_vec(),
            },
            vec![wr(0)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e2,
        SysError::InvalidInput,
        "重试成功后仍至多一次（A4，不重复消费）"
    );
    assert!(rt.undo_stack().is_empty(), "被 A4 拦截的写不产生 undo");
}

#[test]
fn failed_close_rolls_back_own_terminal_marker_fd_still_usable() {
    // 失败 Own(Close)：回滚 Own 终结标记 → fd 仍可继续使用（修复前
    // owned_consumed 残留 → 任何 usage 都被拒 InvalidInput）。
    let mut rt = Runtime::new(Box::new(ScriptedExecutor {
        results: VecDeque::from([
            Ok((Value::Fd(0), UndoCapability::Identity)),  // Open 成功 → fd 0
            Err(SysError::BrokenPipe), // Close(fd 0) 物理失败
            Ok((Value::U64(4), UndoCapability::Identity)), // 随后 Write(fd 0) 成功
        ]),
    }));
    let p = PathBuf::from("/own.txt");

    rt.run_blocking(syscall(
        DataOp::Open {
            path: p.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(p.clone())],
        Action::Pure,
    ))
    .unwrap();

    let e = rt
        .run_blocking(syscall(DataOp::Close { fd: 0 }, vec![ow(0)], Action::Pure))
        .unwrap_err();
    assert_eq!(e, SysError::BrokenPipe, "物理关闭失败原样透传");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    let v = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: 0,
                data: b"x".to_vec(),
            },
            vec![wr(0)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::U64(4), "Own 终结标记已回滚，fd 仍可继续使用");
}

#[test]
fn rollback_only_touches_this_syscall_markers_success_path_axiom_kept() {
    // 回滚不得误删早前成功 syscall 的消费记录：Open(rw) 成功消费路径 P 的
    // Write 标记 → Write(fd) 失败只回滚 fd 标记 → 同路径再 Open(w) 仍被 A4
    // 拒绝（P 已消费，成功路径恰好一次不变）；fd 上 Read 不受影响（Read 不插
    // 标记、回滚无操作）。
    let mut rt = Runtime::new(Box::new(ScriptedExecutor {
        results: VecDeque::from([
            Ok((Value::Fd(0), UndoCapability::Identity)),        // Open(rw) P 成功（P 的 Write 标记消费）
            Err(SysError::PermissionDenied), // Write(fd 0) 物理失败（fd 标记回滚）
        ]),
    }));
    let p = PathBuf::from("/keep.txt");

    rt.run_blocking(syscall(
        DataOp::Open {
            path: p.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(p.clone())],
        Action::Pure,
    ))
    .unwrap();

    rt.run_blocking(syscall(
        DataOp::Write {
            fd: 0,
            data: b"x".to_vec(),
        },
        vec![wr(0)],
        Action::Pure,
    ))
    .unwrap_err();

    // 同路径再 Open(w)：P 的 Write 标记来自**成功的** Open，仍应拒绝（A4）。
    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::InvalidInput,
        "成功 Open 消费的路径 Write 标记不受失败 Write 的回滚影响"
    );

    // fd 上 Read 照常（Read 不插标记；回滚对 Read/Append 无操作）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: 0, len: 4 },
            vec![rd(0)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Unit, "Read 不受回滚影响（兜底值）");
}

/// 审计 B2：check_linear 批内部分失败的前缀回滚。
/// 资源批 [W(path1), W(path2)] 中 path2 已消费（早前成功 Open(w)）→
/// path1 的标记先插入、path2 检查失败 → 只回滚前缀 [W(path1)]：
/// path1 不被毒化（可重试），path2 的**早前**消费记录不被误删（仍拒绝）。
#[test]
fn batch_partial_check_failure_rolls_back_prefix_only() {
    let mut rt = Runtime::new(Box::new(ScriptedExecutor {
        results: VecDeque::from([
            Ok((Value::Fd(0), UndoCapability::Identity)), // Open(w) path2 成功 → path2 Write 标记消费
                                      // 双资源 Open：check_linear(path1) 插入 → check_linear(path2) 失败
                                      // （已消费）→ 前缀 [W(path1)] 回滚，错误透传
        ]),
    }));
    let p1 = PathBuf::from("/a.txt");
    let p2 = PathBuf::from("/b.txt");

    rt.run_blocking(syscall(
        DataOp::Open {
            path: p2.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(p2.clone())],
        Action::Pure,
    ))
    .unwrap();

    // 双资源批：path1 标记插入后 path2 检查失败（InvalidInput）
    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p1.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(p1.clone()), wr_path(p2.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::InvalidInput, "path2 已消费 → 批检查失败");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // path1 前缀标记已回滚 → 同路径 Write 模式重开成功（B2 修复后）
    rt.run_blocking(syscall(
        DataOp::Open {
            path: p1.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(p1.clone())],
        Action::Pure,
    ))
    .unwrap();

    // path2 的早前消费记录未被前缀回滚误删 → 仍被 A4 拒绝
    let e2 = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p2.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(p2.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e2,
        SysError::InvalidInput,
        "早前成功消费的 path2 标记不被前缀回滚误删（A4 恰好一次）"
    );
}
