//! R4b 对抗审计（第 4 轮 B 块：隔离与边界）—— algeff-core 部分：错误枚举边界
//! 透传 + 多执行器契约独立性（D9）。
//!
//! 攻击方法论：与 R1-R3 相同的「真实 Runtime 全链路」约定，但本文件的正
//! 反面样本由**自定义假执行器**驱动（R4b 攻击面 2/3 的本体）：
//!
//! - **攻击面 3（错误枚举边界）**：14 种 SysError + Other(i32) 全部经假执行器
//!   返回，断言解释器原样透传（不替换、不包装、不改码、不压 undo）；
//!   Sequential 链上错误传播且跳过后续闭包；Catch handler 收到完全相同的
//!   错误变体；WatchSignal/Invoke 的 trait 默认实现（ENOSYS=Other(38)）经
//!   共享通道原样透传。
//! - **攻击面 2（多执行器）**：解释器不得依赖 TokioExecutor 的具体行为
//!   （D9 契约独立性）——任意满足 `SyscallExecutor` 契约的执行器经
//!   `Runtime::new` 驱动，返回值进入 next、undo 压栈/recover 执行、Fork
//!   并行共享通道（`Arc<Mutex<Box<dyn>>`）均按契约工作；两个 Runtime 各配
//!   行为不同的假执行器跑同一蓝图结构，结果各自独立。
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 与 `run_blocking` 在 tokio 上下文之外调用。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use algeff_core::{
    Action, BoxFuture, DataOp, OpenFlags, ResourceRegistry, ResourceUsage, Runtime, SysError,
    SyscallExecutor, UndoCapability, Value,
};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

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

// ── 假执行器（R4b 本体）──────────────────────────────────────────────

/// 固定错误执行器：任何 op 都返回固定 SysError（错误透传测试用）。
struct FixedErrorExecutor {
    err: SysError,
}

impl SyscallExecutor for FixedErrorExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        let err = self.err;
        Box::pin(async move { Err(err) })
    }
}

/// 脚本执行器：按调用队列依次返回预设 (Value, UndoCapability)；队列耗尽后
/// 返回固定 fallback。`calls` 统计 execute 调用次数（并行 Fork 通道计数用）。
struct ScriptExecutor {
    queue: Arc<Mutex<VecDeque<(Value, UndoCapability)>>>,
    fallback: Value,
    calls: Arc<AtomicUsize>,
}

impl ScriptExecutor {
    fn with_values(vals: Vec<Value>, fallback: Value) -> Self {
        let items: VecDeque<(Value, UndoCapability)> =
            vals.into_iter().map(|v| (v, UndoCapability::Identity)).collect();
        Self {
            queue: Arc::new(Mutex::new(items)),
            fallback,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SyscallExecutor for ScriptExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (v, undo) = self
            .queue
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or((self.fallback.clone(), UndoCapability::Identity));
        Box::pin(async move { Ok((v, undo)) })
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：错误枚举边界 —— 14 种 SysError + Other(i32) 经执行器返回后，
// 解释器必须原样透传。每个错误码一个检查点。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn err_all_14_variants_and_other_passthrough_exact() {
    // 14 种 POSIX 错误 + Other(i32) 兜底：经假执行器返回后解释器原样透传
    // （不替换、不包装、不改码、不压 undo）。
    let variants = [
        SysError::NotFound,
        SysError::PermissionDenied,
        SysError::WouldBlock,
        SysError::Interrupted,
        SysError::TimedOut,
        SysError::ConnectionReset,
        SysError::ConnectionRefused,
        SysError::BrokenPipe,
        SysError::StorageFull,
        SysError::InvalidInput,
        SysError::AlreadyExists,
        SysError::NotADirectory,
        SysError::IsADirectory,
        SysError::CrossDevice,
        SysError::Other(777),
    ];
    assert_eq!(variants.len(), 15, "14 种 + Other");
    for (i, expected) in variants.iter().enumerate() {
        let mut rt = Runtime::new(Box::new(FixedErrorExecutor { err: *expected }));
        let got = rt
            .run_blocking(syscall(DataOp::Close { fd: 999 }, vec![], Action::Pure))
            .unwrap_err();
        assert_eq!(&got, expected, "第 {i} 个错误变体必须原样透传（相等语义）");
        assert_eq!(got.code(), expected.code(), "第 {i} 个：errno 映射一致");
        assert!(rt.undo_stack().is_empty(), "第 {i} 个：错误透传不产生 undo");
    }
}

#[test]
fn err_sequential_propagates_and_skips_remaining_steps() {
    let next_called = Arc::new(AtomicUsize::new(0));
    let tail_called = Arc::new(AtomicUsize::new(0));
    let n1 = next_called.clone();
    let n2 = tail_called.clone();
    let mut rt = Runtime::new(Box::new(FixedErrorExecutor {
        err: SysError::BrokenPipe,
    }));

    let e = rt
        .run_blocking(Action::Sequential {
            current: Box::new(syscall(DataOp::Close { fd: 999 }, vec![], move |v| {
                n1.fetch_add(1, Ordering::SeqCst);
                Action::Pure(v)
            })),
            next: Box::new(move |_| {
                n2.fetch_add(1, Ordering::SeqCst);
                Action::Pure(Value::Unit)
            }),
        })
        .unwrap_err();
    assert_eq!(e, SysError::BrokenPipe, "错误经 Sequential 原样冒出");
    assert_eq!(
        next_called.load(Ordering::SeqCst),
        0,
        "失败 syscall 的 next 闭包不得执行"
    );
    assert_eq!(
        tail_called.load(Ordering::SeqCst),
        0,
        "链上后续 action 不得执行"
    );
    assert!(rt.undo_stack().is_empty(), "失败路径不产生 undo");
}

#[test]
fn err_catch_handler_receives_exact_variant_and_recovers() {
    let variants = [
        SysError::NotFound,
        SysError::WouldBlock,
        SysError::StorageFull,
        SysError::Other(-7),
    ];
    for expected in variants {
        let mut rt = Runtime::new(Box::new(FixedErrorExecutor { err: expected }));
        let v = rt
            .run_blocking(Action::Catch {
                action: Box::new(syscall(DataOp::Close { fd: 999 }, vec![], Action::Pure)),
                handler: Box::new(move |e| Action::Pure(Value::Str(format!("caught:{e}")))),
            })
            .unwrap();
        assert_eq!(
            v,
            Value::Str(format!("caught:{expected}")),
            "Catch handler 收到与执行器返回完全相同的错误变体（含 Display）"
        );
        assert!(
            rt.undo_stack().is_empty(),
            "Catch 不触碰撤销栈（错误无 undo）"
        );
    }
}

#[test]
fn err_watch_signal_and_invoke_default_enosys_passthrough() {
    // 假执行器未覆写 watch_signal/invoke → trait 默认实现返回 Other(38)
    // （ENOSYS）→ 解释器经共享通道原样透传。
    let mut rt = Runtime::new(Box::new(FixedErrorExecutor {
        err: SysError::NotFound, // execute 的返回与默认方法互不干扰
    }));
    let e = rt
        .run_blocking(Action::WatchSignal {
            signal: 2,
            next: Box::new(Action::Pure),
        })
        .unwrap_err();
    assert_eq!(e, SysError::Other(38), "watch_signal 默认 ENOSYS 原样透传");

    let e = rt
        .run_blocking(Action::Invoke {
            foreign_id: 1,
            captures: vec![],
            yields: vec![],
            deterministic: true,
            next: Box::new(Action::Pure),
        })
        .unwrap_err();
    assert_eq!(e, SysError::Other(38), "invoke 默认 ENOSYS 原样透传");
    assert!(rt.undo_stack().is_empty());
}

#[test]
fn err_repeated_failures_never_push_undo() {
    // 连续 5 次失败：每次错误原样透传且撤销栈保持空（错误 op 的 undo 不落地）。
    let mut rt = Runtime::new(Box::new(FixedErrorExecutor {
        err: SysError::StorageFull,
    }));
    for i in 0..5 {
        let e = rt
            .run_blocking(syscall(DataOp::Close { fd: 999 }, vec![], Action::Pure))
            .unwrap_err();
        assert_eq!(e, SysError::StorageFull, "第 {i} 次透传一致");
        assert!(rt.undo_stack().is_empty(), "第 {i} 次：错误不产生 undo");
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：多执行器 —— 解释器不依赖 TokioExecutor 具体行为（D9 契约
// 独立性）。任意满足 SyscallExecutor 契约的执行器经 Runtime::new 驱动：
// 值流、undo 流、Fork 并行共享通道均按契约工作。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn fake_executor_drives_interpreter_values_and_undo_contract() {
    // 假执行器经 Runtime::new 驱动完整解释器：返回值原样进入 next、undo
    // 压栈、recover 执行——解释器不依赖 TokioExecutor 的任何具体行为。
    let undo_runs = Arc::new(AtomicUsize::new(0));
    let ur = undo_runs.clone();
    let undo = Box::pin(async move {
        ur.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let queue = Arc::new(Mutex::new(VecDeque::from([
        (Value::Fd(7), UndoCapability::Identity),
        (Value::Bytes(b"XYZ".to_vec()), UndoCapability::Identity),
        (Value::U64(42), UndoCapability::Invertible(undo)),
    ])));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut rt = Runtime::new(Box::new(ScriptExecutor {
        queue: queue.clone(),
        fallback: Value::Unit,
        calls: calls.clone(),
    }));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: PathBuf::from("/fake/f.txt"),
                flags: OpenFlags::default(),
            },
            vec![],
            move |v| {
                assert_eq!(v, Value::Fd(7), "Open 返回值原样进入 next");
                syscall(DataOp::Read { fd: 7, len: 3 }, vec![], move |v| {
                    assert_eq!(v, Value::Bytes(b"XYZ".to_vec()), "Read 返回值原样进入 next");
                    syscall(
                        DataOp::Write {
                            fd: 7,
                            data: b"z".to_vec(),
                        },
                        vec![],
                        Action::Pure,
                    )
                })
            },
        ))
        .unwrap();
    assert_eq!(
        v,
        Value::U64(42),
        "链尾 Write 返回值透传到 run_blocking 结果"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "解释器经共享通道恰好调用 3 次执行器"
    );
    assert_eq!(rt.undo_stack().len(), 1, "Write 的 undo 已压入撤销栈");
    assert_eq!(undo_runs.load(Ordering::SeqCst), 0, "undo 尚未执行");

    // Replace（D10：recover + reg.clear）执行假执行器提供的 undo 闭包。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        undo_runs.load(Ordering::SeqCst),
        1,
        "recover 执行了假执行器的 undo"
    );
    assert!(rt.undo_stack().is_empty(), "recover 后撤销栈空");
}

#[test]
fn two_runtimes_distinct_executors_isolated_and_parallel_fork() {
    // 同一蓝图结构，两个 Runtime 各配行为不同的脚本执行器 → 各自结果不同：
    // 结果完全由执行器契约行为决定，解释器不内嵌 TokioExecutor 行为。
    let mut rt_a = Runtime::new(Box::new(ScriptExecutor::with_values(
        vec![Value::Fd(1), Value::U64(10), Value::U64(100)],
        Value::U64(999),
    )));
    let mut rt_b = Runtime::new(Box::new(ScriptExecutor::with_values(
        vec![Value::Fd(999), Value::U64(9990), Value::U64(99900)],
        Value::U64(888),
    )));

    // 蓝图结构相同（Open → Read → Write）；next 内断言每一步的值正是该
    // Runtime 执行器的契约返回值。
    let blueprint = |expect_open: Value, expect_read: Value, expect_final: Value| {
        syscall(
            DataOp::Open {
                path: PathBuf::from("/x"),
                flags: OpenFlags::default(),
            },
            vec![],
            move |v| {
                assert_eq!(v, expect_open, "Open 值由执行器契约决定");
                let fd = match v {
                    Value::Fd(f) => f,
                    other => panic!("期望 Fd，得到 {other:?}"),
                };
                syscall(DataOp::Read { fd, len: 1 }, vec![], move |v| {
                    assert_eq!(v, expect_read, "Read 值由执行器契约决定");
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"w".to_vec(),
                        },
                        vec![],
                        move |v| {
                            assert_eq!(v, expect_final, "Write 值由执行器契约决定");
                            Action::Pure(v)
                        },
                    )
                })
            },
        )
    };

    let va = rt_a
        .run_blocking(blueprint(Value::Fd(1), Value::U64(10), Value::U64(100)))
        .unwrap();
    assert_eq!(va, Value::U64(100), "A 的假执行器值流");
    let vb = rt_b
        .run_blocking(blueprint(
            Value::Fd(999),
            Value::U64(9990),
            Value::U64(99900),
        ))
        .unwrap();
    assert_eq!(vb, Value::U64(99900), "B 的假执行器值流");

    // rt_a 上并行 Fork（空资源 → fork_conflict=false → 真并行共享通道）：
    // 两分支各一次 execute（队列已耗尽 → fallback），值经 combine 合并。
    // 证明 run_fork_parallel 的 Arc<Mutex> 通道对任意 SyscallExecutor 契约
    // 生效（D9 独立性在并行路径同样成立）。
    let v = rt_a
        .run_blocking(Action::Fork {
            left: Box::new(syscall(DataOp::Close { fd: 999 }, vec![], Action::Pure)),
            right: Box::new(syscall(DataOp::Close { fd: 999 }, vec![], Action::Pure)),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::List(vec![Value::U64(999), Value::U64(999)]),
        "并行 Fork 分支值经假执行器合并（队列耗尽走 fallback）"
    );
    assert_eq!(
        rt_a.undo_stack().len(),
        0,
        "假执行器未提供 undo → 两运行时撤销栈均独立为空"
    );
    assert_eq!(rt_b.undo_stack().len(), 0);
    // A 执行器调用计数：蓝图 3 + Fork 2 = 5。
    // （无法从外部读取 calls，经最终断言间接验证：Fork 结果已是 fallback
    // 值且未 panic，说明队列恰好在蓝图后耗尽。）
}
