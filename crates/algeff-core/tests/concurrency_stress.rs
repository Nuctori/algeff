//! 并发压力测试（A6 Verification 批 4，pdr.md §19.4 loom 的替代策略）。
//!
//! 背景：loom 与 tokio async 集成成本高（spec §19.4 允许以 tokio 原生并发测试替代）；
//! A5 未合并前不触碰真实 IO —— 本文件与 `execution_axioms.rs` 同构，全部以自建
//! MockExecutor（记录 op/undo 序列、可配置返回值、可分配 fd）驱动真实解释器。
//!
//! 本批验证两个契约事实在真实多线程调度下的保持：
//!   1. **D13 隔离-合并模式**：`ResourceRegistry: Clone`，Fork 并行时子任务以
//!      隔离副本执行、互不污染；并发场景下各任务 registry 的 fd 分配序列一致；
//!   2. **解释器可重放性**：同一蓝图在并发任务间、同任务多轮间（含 recover）
//!      结果与调用序列完全一致；
//!   3. **A7 动态仲裁**（`ResourceArbiter`，tla/scheduler.tla 的工程载体）：
//!      原子占坑 + 失败回滚 + 有限重试在真实并发争用下保持互斥不变量
//!      （任意时刻至多一个持有者）、无死锁、无丢失唤醒。
//!
//! 工程约束（冻结签名所致，本文件予以遵守）：
//! - `interpret` 的 future **非 Send**（`&mut dyn SyscallExecutor` 无 Send 超
//!   trait）→ 不能直接 `tokio::spawn(interpret(...))`；`Action` 亦非 Clone/Send
//!   （`Box<dyn FnOnce>` 无 Send）→ 每个任务内部通过共享的 `fn` 构造器重建蓝图；
//! - `Runtime::new` 须在 tokio 上下文之外调用（D9）→ 解释器驱动统一走
//!   `spawn_blocking`（阻塞线程位于 tokio 上下文之外），在阻塞线程内以
//!   current-thread runtime `block_on`（`drive`）驱动，与 `execution_axioms.rs` 同构；
//! - 外部 `tokio::spawn` N=8 提供真实并发调度（多线程 runtime），
//!   内部 `spawn_blocking` 承担非 Send 的 interpret 驱动。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use algeff_core::action::{Action, DataOp, OpenFlags, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{
    AccessMode, Resource, ResourceArbiter, ResourceHandle, ResourceRegistry, ResourceSet,
    ResourceUsage,
};
use algeff_core::runtime::{interpret, Context, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoCapability};

/// 本地 current-thread runtime 驱动（interpret future 非 Send，只能在阻塞线程内
/// `block_on`；`spawn_blocking` 线程位于 tokio 上下文之外，满足 D9）。
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

/// execute 的返回结果配置（本文件只使用 Value 分支；错误传播路径已在
/// execution_axioms.rs 的 exec_syscall_error_propagates 覆盖）。
#[derive(Clone)]
enum MockOutcome {
    Value(Value),
}

/// 可配置 Mock 执行器：记录 op 调用序列与 fd 分配序列（D13 隔离断言载体），
/// 可按 op 描述返回 Value/Err/undo。`Open`/`PipeOpen` 走注册表分配全局唯一
/// fd（决策 D1），其余 op 走响应表。
#[derive(Default)]
struct MockExecutor {
    /// 每次 execute 的 op 描述（调用顺序）。
    log: Arc<Mutex<Vec<String>>>,
    /// undo 执行记录（recover 顺序）。
    undo_log: Arc<Mutex<Vec<String>>>,
    /// registry 分配的 fd 序列（每次 Open/PipeOpen 追加）。
    alloc_log: Arc<Mutex<Vec<u64>>>,
    /// op 描述 → 返回结果（未配置 → Ok(Value::Unit)）。
    responses: HashMap<String, MockOutcome>,
    /// 是否在 Ok 时附带 undo。
    with_undo: bool,
}

impl MockExecutor {
    fn new() -> Self {
        Self::default()
    }

    fn respond(&mut self, desc: &str, out: MockOutcome) {
        self.responses.insert(desc.to_string(), out);
    }

    fn ops(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn undo_ops(&self) -> Vec<String> {
        self.undo_log.lock().unwrap().clone()
    }

    fn alloc_log(&self) -> Vec<u64> {
        self.alloc_log.lock().unwrap().clone()
    }
}

/// op → 稳定描述串（测试按此配置响应并断言调用序）。
/// Open 附带路径、Write 附带 fd 与 data 长度，区分同 fd 的多次写。
fn describe(op: &DataOp) -> String {
    match op {
        DataOp::Open { path, .. } => format!("open:{}", path.display()),
        DataOp::Write { fd, data } => format!("write:{fd}:{}", data.len()),
        DataOp::Read { fd, len } => format!("read:{fd}:{len}"),
        DataOp::GetTime => "gettime".to_string(),
        other => format!("{other:?}"),
    }
}

impl SyscallExecutor for MockExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        let desc = describe(op);
        // Open/PipeOpen 由注册表分配 fd 且**不产生 undo**（简化：其逆操作 Close 不建模，
        // 聚焦 Write 撤销路径——保证本文件断言中的 undo 序列恰为各 Write 的 LIFO 逆序）。
        let is_open = matches!(op, DataOp::Open { .. } | DataOp::PipeOpen { .. });
        Box::pin(async move {
            self.log.lock().unwrap().push(desc.clone());
            let cap: UndoCapability = if self.with_undo && !is_open {
                let label = format!("undo({desc})");
                let undo_log = self.undo_log.clone();
                UndoCapability::Invertible(Box::pin(async move {
                    undo_log.lock().unwrap().push(label);
                    Ok(())
                }))
            } else {
                UndoCapability::Identity
            };
            // Open/PipeOpen：由任务自己的 registry 隔离副本分配全局唯一 fd（D1/D13），
            // 并记录分配序列 —— 并发下「各 registry 分配序列一致」即隔离未被破坏的证据。
            if is_open {
                let fd =
                    registry.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
                self.alloc_log.lock().unwrap().push(fd);
                return Ok((Value::Fd(fd), cap));
            }
            let out = match self.responses.get(&desc).cloned() {
                Some(MockOutcome::Value(v)) => v,
                None => Value::Unit,
            };
            Ok((out, cap))
        })
    }
}

/// 构造返回自身结果的单步 Syscall（next = 恒等 Pure）。
fn syscall_step(op: DataOp, resources: Vec<ResourceUsage>) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(Action::Pure),
    }
}

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

fn open_step(path: &str) -> Action {
    syscall_step(
        DataOp::Open {
            path: PathBuf::from(path),
            flags: OpenFlags {
                read: true,
                ..OpenFlags::default()
            },
        },
        vec![usage(Resource::Path(path.to_string()), AccessMode::Read)],
    )
}

fn fork_same_fd_write(fd: u64) -> Action {
    // 左右分支对**同一 fd** 做 Write：共享 registry 时第二次 Write 会被 A4 线性
    // 检查拒绝（consumed 已含该资源）；只有 D13 的 Clone 隔离（Fork 分支各持
    // 独立 registry 副本）才可能两侧都成功 —— 结果 U64(10)+U64(20)=30 即隔离证据。
    Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Write {
                fd,
                data: vec![0xAA],
            },
            vec![usage(Resource::Fd(fd), AccessMode::Write)],
        )),
        right: Box::new(syscall_step(
            DataOp::Write {
                fd,
                data: vec![0xAA, 0xBB],
            },
            vec![usage(Resource::Fd(fd), AccessMode::Write)],
        )),
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(x), Value::U64(y)) => Action::Pure(Value::U64(x + y)),
            _ => Action::Pure(Value::U64(0)),
        }),
    }
}

/// 压力蓝图：含 Syscall（Open×3 / Write×2）/ Choose（恒真取 then）/ Fork（同 fd
/// 双写，D13 隔离）/ Replace（收尾，D10：先 recover 再 target）。
///
/// 执行路径：Open /a → fd1 → Open /b → fd2 → Choose(true) → Fork(10+20=30) →
/// Open /c → fd3 → Write fd2 → Replace{target: Pure(100)}。Replace 把前面 3 个
/// Write 压入的 undo op 全部 LIFO 撤销，最终返回 100 且撤销栈清空。
fn stress_blueprint() -> Action {
    Action::Sequential {
        current: Box::new(open_step("/a")),
        next: Box::new(|v_a| {
            let fd_a = match v_a {
                Value::Fd(f) => f,
                _ => u64::MAX,
            };
            Action::Sequential {
                current: Box::new(open_step("/b")),
                next: Box::new(move |v_b| {
                    let fd_b = match v_b {
                        Value::Fd(f) => f,
                        _ => u64::MAX,
                    };
                    Action::Sequential {
                        current: Box::new(Action::Choose {
                            cond: Box::new(|_cur: &Value| true),
                            then_branch: Box::new(fork_same_fd_write(fd_a)),
                            else_branch: Box::new(Action::Replace {
                                target: Box::new(Action::Pure(Value::U64(99))),
                            }),
                        }),
                        next: Box::new(move |_fork_v| Action::Sequential {
                            current: Box::new(open_step("/c")),
                            next: Box::new(move |_v_c| Action::Sequential {
                                current: Box::new(syscall_step(
                                    DataOp::Write {
                                        fd: fd_b,
                                        data: vec![0xCC],
                                    },
                                    vec![usage(Resource::Fd(fd_b), AccessMode::Write)],
                                )),
                                next: Box::new(|_w| Action::Replace {
                                    target: Box::new(Action::Pure(Value::U64(100))),
                                }),
                            }),
                        }),
                    }
                }),
            }
        }),
    }
}

/// 可重放性蓝图：同压力蓝图但**不以 Replace 收尾**（Replace 位于 Choose 的恒假
/// 分支，保留节点覆盖），以 Write fd2 结束 → 撤销栈留下 3 个 undo op，供每轮
/// **显式 recover**（round = interpret + recover，验证可重放性在并发下保持）。
fn replay_blueprint() -> Action {
    Action::Sequential {
        current: Box::new(open_step("/a")),
        next: Box::new(|v_a| {
            let fd_a = match v_a {
                Value::Fd(f) => f,
                _ => u64::MAX,
            };
            Action::Sequential {
                current: Box::new(open_step("/b")),
                next: Box::new(move |v_b| {
                    let fd_b = match v_b {
                        Value::Fd(f) => f,
                        _ => u64::MAX,
                    };
                    Action::Sequential {
                        current: Box::new(Action::Choose {
                            cond: Box::new(|_cur: &Value| true),
                            then_branch: Box::new(fork_same_fd_write(fd_a)),
                            else_branch: Box::new(Action::Replace {
                                target: Box::new(Action::Pure(Value::U64(0))),
                            }),
                        }),
                        next: Box::new(move |_fork_v| {
                            syscall_step(
                                DataOp::Write {
                                    fd: fd_b,
                                    data: vec![0xCC],
                                },
                                vec![usage(Resource::Fd(fd_b), AccessMode::Write)],
                            )
                        }),
                    }
                }),
            }
        }),
    }
}

/// 配置共享的 MockExecutor 响应（fd 分配序列对每个隔离副本确定：1, 2, …）。
fn configured_executor() -> MockExecutor {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("write:1:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:1:2", MockOutcome::Value(Value::U64(20)));
    ex.respond("write:2:1", MockOutcome::Value(Value::U64(50)));
    ex
}

struct StressOutcome {
    value: Result<Value, SysError>,
    fds: Vec<u64>,
    undo_left: usize,
    ops: Vec<String>,
    undos: Vec<String>,
}

/// 单任务独立执行：独立 (Context, UndoStack, Registry 隔离副本) + 各自 MockExecutor，
/// 在 spawn_blocking 线程内（tokio 上下文之外）驱动同一压力蓝图。
fn run_stress_isolated(base: ResourceRegistry) -> StressOutcome {
    let mut ex = configured_executor();
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = base; // D13 隔离副本：子任务状态完全独立
    let value = drive(interpret(
        stress_blueprint(),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    StressOutcome {
        value,
        fds: ex.alloc_log(),
        undo_left: undo.len(),
        ops: ex.ops(),
        undos: ex.undo_ops(),
    }
}

// ── (a) D13 隔离-合并模式：8 任务并发跑同一蓝图，状态互不污染 ──────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn parallel_runs_isolated_state() {
    // D13 前置：父 registry 先分配一个 fd（0），8 个子任务各持一个隔离 Clone。
    let mut parent = ResourceRegistry::new();
    parent.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    let base = parent.clone();

    // N=8 并发任务：tokio::spawn 提供真实多线程调度；interpret 非 Send →
    // 实际驱动下沉到 spawn_blocking 线程（tokio 上下文之外，满足 D9）。
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let base = base.clone();
            tokio::spawn(async move {
                tokio::task::spawn_blocking(move || run_stress_isolated(base))
                    .await
                    .expect("spawn_blocking 任务 panic")
            })
        })
        .collect();

    let mut outcomes = Vec::new();
    for h in handles {
        outcomes.push(h.await.expect("并发任务 panic"));
    }

    // 8 个结果一致（同一蓝图在并发下可重放）
    for o in &outcomes {
        assert_eq!(
            o.value,
            Ok(Value::U64(100)),
            "8 任务同一蓝图（Syscall/Choose/Fork/Replace）结果一致"
        );
    }
    // 各 registry 隔离副本独立：fd 分配序列一致（互不污染）
    for o in &outcomes {
        assert_eq!(o.fds, vec![1, 2, 3], "隔离副本的 fd 分配序列一致");
        assert_eq!(
            o.ops,
            vec![
                "open:/a".to_string(),
                "open:/b".to_string(),
                "write:1:1".to_string(),
                "write:1:2".to_string(),
                "open:/c".to_string(),
                "write:2:1".to_string(),
            ],
            "op 调用序列一致（Fork 同 fd 双写经 D13 隔离均成功）"
        );
        assert_eq!(
            o.undo_left, 0,
            "Replace 先 recover 后 target（D10），撤销栈已清空"
        );
        assert_eq!(
            o.undos,
            vec![
                "undo(write:2:1)".to_string(),
                "undo(write:1:2)".to_string(),
                "undo(write:1:1)".to_string(),
            ],
            "LIFO 逆序撤销"
        );
    }
    // 父 registry 未被任何子任务触碰：next_fd 仍为 1（子任务 Clone 未污染父）
    let next_fd = parent.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert_eq!(next_fd, 1, "隔离副本未污染父 registry 的 fd 分配");
}

// ── (b) A7 动态仲裁：共享 arbiter 上的互斥不变量与无死锁/无丢失唤醒 ─────────

/// 单任务：有限重试占坑（A7：原子占坑 + 失败回滚 + 不阻塞等待）→ 持有期间断言
/// 计数不变量（至多一个持有者）→ 释放。返回是否成功完成。
async fn arbiter_task(
    arbiter: Arc<tokio::sync::Mutex<ResourceArbiter>>,
    holders: Arc<AtomicUsize>,
    contention: Arc<AtomicUsize>,
    set: ResourceSet,
) -> bool {
    // 失败回滚 + 有限重试：try_claim 失败时自身状态完全不变，直接重试；
    // 不提供阻塞等待 → 不存在循环等待链（命题 P5）。
    let mut attempts = 0u32;
    loop {
        if arbiter.lock().await.try_claim(&set) {
            break;
        }
        contention.fetch_add(1, Ordering::SeqCst);
        attempts += 1;
        assert!(
            attempts < 10_000,
            "有限重试内应能获占坑（持有方必然归还，无死锁/无丢失唤醒）"
        );
        tokio::task::yield_now().await;
    }
    // 持有期间计数不变量：任意时刻至多一个持有者。
    let prev = holders.fetch_add(1, Ordering::SeqCst);
    assert_eq!(prev, 0, "任意时刻至多一个持有者（互斥）");
    tokio::task::yield_now().await; // 制造交错窗口，暴露并发破口
    assert_eq!(holders.load(Ordering::SeqCst), 1, "持有期间计数保持 1");
    holders.fetch_sub(1, Ordering::SeqCst);
    // 释放后另一任务可获（其余任务在有限重试中必然成功）。
    arbiter.lock().await.release(&set);
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_arbiter_claims() {
    let arbiter = Arc::new(tokio::sync::Mutex::new(ResourceArbiter::new()));
    let holders = Arc::new(AtomicUsize::new(0));
    let contention = Arc::new(AtomicUsize::new(0));
    // 相同互斥集合（Write/Own 独占 + Read，集合级原子占坑 → 集合整体互斥）。
    let set = vec![
        usage(Resource::Fd(1), AccessMode::Write),
        usage(Resource::Fd(2), AccessMode::Read),
        usage(Resource::Fd(3), AccessMode::Own),
    ];

    // N=8 并发争用同一仲裁器。
    let h0 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h1 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h2 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h3 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h4 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h5 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h6 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));
    let h7 = tokio::spawn(arbiter_task(
        arbiter.clone(),
        holders.clone(),
        contention.clone(),
        set.clone(),
    ));

    // tokio::join! 全完成断言：无死锁、无丢失唤醒。
    let (r0, r1, r2, r3, r4, r5, r6, r7) = tokio::join!(h0, h1, h2, h3, h4, h5, h6, h7);
    for r in [r0, r1, r2, r3, r4, r5, r6, r7] {
        assert!(
            r.expect("arbiter 并发任务 panic"),
            "所有任务均成功占坑并释放"
        );
    }

    // 非真空性：确曾发生争用失败重试（说明并发窗口真实存在）。
    assert!(
        contention.load(Ordering::SeqCst) > 0,
        "8 任务争用互斥集合应产生至少一次占坑失败重试"
    );
    // 最终无占坑残留。
    let arb = arbiter.lock().await;
    assert!(!arb.held(&Resource::Fd(1)), "释放后 Fd(1) 无占坑残留");
    assert!(!arb.held(&Resource::Fd(2)), "释放后 Fd(2) 无占坑残留");
    assert!(!arb.held(&Resource::Fd(3)), "释放后 Fd(3) 无占坑残留");
}

// ── (c) 可重放性在并发下的保持：8 任务 × 3 轮（含 recover）结果一致 ─────────

struct ReplayOutcome {
    values: Vec<Result<Value, SysError>>,
    undo_lens: Vec<usize>,
    fds: Vec<u64>,
    undos: Vec<String>,
}

/// 单任务：同一可重放性蓝图跑 3 轮，每轮独立 (Context, UndoStack, Registry 隔离
/// 副本) + 共享 MockExecutor；每轮解释后**显式 recover**（3 个 undo op，LIFO）。
fn run_replay_rounds(base: ResourceRegistry) -> ReplayOutcome {
    let mut ex = configured_executor();
    let mut values = Vec::new();
    let mut undo_lens = Vec::new();
    for _round in 0..3 {
        let mut ctx = Context::new();
        let mut undo = UndoStack::new();
        let mut reg = base.clone();
        let v = drive(interpret(
            replay_blueprint(),
            &mut ctx,
            &mut undo,
            &mut reg,
            &mut ex,
        ));
        values.push(v);
        undo_lens.push(undo.len());
        // 含 recover：每轮显式撤销（LIFO 逆序执行 + 栈清空）。
        drive(undo.recover()).unwrap();
        assert!(undo.is_empty(), "recover 后撤销栈清空");
    }
    ReplayOutcome {
        values,
        undo_lens,
        fds: ex.alloc_log(),
        undos: ex.undo_ops(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn replay_under_concurrency() {
    let mut parent = ResourceRegistry::new();
    parent.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    let base = parent.clone();

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let base = base.clone();
            tokio::spawn(async move {
                tokio::task::spawn_blocking(move || run_replay_rounds(base))
                    .await
                    .expect("spawn_blocking 任务 panic")
            })
        })
        .collect();

    let mut outcomes = Vec::new();
    for h in handles {
        outcomes.push(h.await.expect("并发任务 panic"));
    }

    // 轮间与任务间结果一致（可重放性在并发下保持）。
    for o in &outcomes {
        assert_eq!(
            o.values,
            vec![Ok(Value::U64(50)), Ok(Value::U64(50)), Ok(Value::U64(50)),],
            "同一蓝图 3 轮结果一致（轮间可重放）"
        );
        assert_eq!(
            o.undo_lens,
            vec![3, 3, 3],
            "每轮解释后撤销栈留 3 个 undo op"
        );
        assert_eq!(
            o.fds,
            vec![1, 2, 1, 2, 1, 2],
            "每轮全新 registry 隔离副本，fd 分配序列重放一致"
        );
        assert_eq!(o.undos.len(), 9, "3 轮 × 3 个 LIFO 撤销全部执行");
    }
    // 8 个任务整体一致（任务间可重放）。
    let first = &outcomes[0];
    for o in &outcomes[1..] {
        assert_eq!(o.values, first.values, "任务间结果一致");
        assert_eq!(o.undo_lens, first.undo_lens, "任务间撤销栈状态一致");
        assert_eq!(o.fds, first.fds, "任务间 fd 分配序列一致");
        assert_eq!(o.undos, first.undos, "任务间 undo 执行序列一致");
    }
}
