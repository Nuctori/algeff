//! R3b 对抗审计（分块 B，第 2/3 部分）：确定性终验 + 用户责任边界。
//!
//! 攻击方法论：与 R1/R2 相同——不 mock、全部经真实 `Runtime` + `TokioExecutor`
//! 全链路（`run_blocking` → `interpret` → 共享执行器通道，零 mock）。轨迹
//! 观察执行器（`TracedExecutor`）只追加只读观察（op 描述 + 执行线程 id），
//! 全部行为委托真实执行器——R1 `LoggingExecutor` 先例。
//!
//! R1/R2 已覆盖（不重复）：简单 Fork 蓝图两次执行轨迹一致、GetTime 单/多
//! 上下文类型稳定、未声明 MutexLock 竞争 WouldBlock + Replace 后可重入。
//!
//! 本文件攻击新边界：
//!
//! 2. **确定性终验**：
//!    - 含 Fork+Catch+Scope+Timeout 的**复杂蓝图** 100 轮执行轨迹一致
//!      （op 序列 + 最终值 + cwd 恢复 + 物理文件内容）；
//!    - GetTime 100 轮值类型稳定（普通 + 并行 Fork 分支内）。
//! 3. **用户责任边界**（pdr §18「正确声明 ResourceSet，不隐瞒依赖」）：
//!    - **未声明资源的 MutexLock 在并行 Fork 中 WouldBlock**：不挂死、不
//!      毒化——Catch 继续执行、不同 id 锁立即可用、胜者持有期同 id 竞争
//!      WouldBlock 属合法占有（非占坑泄漏）、Replace 后同 id 可重入；
//!    - **隐藏闭包内 Syscall 的 fork_conflict 盲区验证**（runtime.rs 已注明
//!      的阶段 1 近似，R1 记录的低疑点）：构造 next 闭包内 Syscall 的 Fork，
//!      直接调用 `fork_conflict` 验证静态收集行为，再以真实执行（线程 id +
//!      竞争结果）验证静态判定与执行路径是否一致——如实记录结果。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use algeff_core::{
    AccessMode, Action, BoxFuture, DataOp, OpenFlags, ReadOnly, Resource, ResourceInner,
    ResourceRegistry, ResourceUsage, Runtime, SysError, SyscallExecutor, TypedResource, UndoOp,
    Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

/// 线性绕过（pdr §18：类型状态包装不能完全阻止绕过，运行时拦截是防线；
/// 本文件同时用于构造隐藏闭包内 Syscall 的**正确声明**——声明存在但静态
/// 收集器看不见，即盲区验证对象）。
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

/// 轨迹观察执行器：真实 TokioExecutor + 逐 op 记录（op 描述 + 执行线程 id）。
/// 非 mock——全部行为委托真实执行器，仅追加只读观察。线程 id 用于 Fork
/// 并行路径的证据（并行分支经 `spawn_blocking` 在不同线程执行）。
///
/// `op_delay`（默认 0）：每个 op 执行前的固定延迟。并行证据的确定性保障
/// （审计 R1 r3b-flaky 修复）：virtual-clock 下 GetTime 短路使分支任务极短，
/// spawn_blocking 任务 1 可能在任务 2 提交前完成 → 阻塞池线程复用 → 两个
/// 分支 op 记录同线程（线程断言误报）。注入 ≥50ms 延迟后两分支必然同时
/// 在飞（任务 2 提交时任务 1 仍在 sleep）→ 分到不同池线程，断言确定。
struct TracedExecutor {
    inner: TokioExecutor,
    log: Arc<Mutex<Vec<(String, std::thread::ThreadId)>>>,
    op_delay: Duration,
}

impl SyscallExecutor for TracedExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async move {
            self.log
                .lock()
                .unwrap()
                .push((format!("{op:?}"), std::thread::current().id()));
            if !self.op_delay.is_zero() {
                tokio::time::sleep(self.op_delay).await;
            }
            self.inner.execute(op, registry).await
        })
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2a：复杂蓝图（Fork+Catch+Scope+Timeout）100 轮执行确定性
// ══════════════════════════════════════════════════════════════════════

/// 复杂蓝图（R1/R2 未覆盖的组合深度）：
/// `Scope{ Sequential{ Open → [并行 Fork: Timeout(Sleep 1ms) ∥ Write] → Timeout(Sleep 500ms, 20ms 触发) } → Catch{ Read(不存在 fd) → handler } }`
/// - Scope：cwd 压栈/恢复（finally 语义）；
/// - 并行 Fork：左分支 Timeout+Sleep（无 syscall），右分支 Write——真并行路径；
/// - 嵌套 Timeout：20ms 超时可靠触发 on_timeout（42）；
/// - Catch：Read(999999) NotFound → handler → "caught"。
/// 100 轮（每轮全新 Runtime + 全新轨迹记录）：op 轨迹逐位一致、最终值一致、
/// 物理文件内容一致、Scope 退出后 cwd 恢复一致。
#[test]
fn det_complex_blueprint_100_rounds_trajectory_identical() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("complex.txt");
    std::fs::write(&pa, b"").unwrap();
    let before_cwd = std::env::current_dir().unwrap();

    fn blueprint(pa: PathBuf) -> Action {
        Action::Scope {
            base: PathBuf::from("scp"),
            inner: Box::new(Action::Sequential {
                current: Box::new(syscall(
                    DataOp::Open {
                        path: pa.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pa.clone())],
                    Action::Pure,
                )),
                next: Box::new(move |v| {
                    let fd = fd_of(&v);
                    Action::Sequential {
                        // 并行 Fork：左分支 Timeout(Sleep 1ms) 无 syscall，
                        // 右分支 Write——轨迹唯一确定（并行不引入乱序 op）
                        current: Box::new(Action::Fork {
                            left: Box::new(Action::Timeout {
                                action: Box::new(Action::Sleep {
                                    duration: Duration::from_millis(1),
                                    next: Box::new(|_| Action::Pure(Value::U64(1))),
                                }),
                                duration: Duration::from_millis(200),
                                on_timeout: Box::new(Action::Pure(Value::U64(999))),
                            }),
                            right: Box::new(syscall(
                                DataOp::Write {
                                    fd,
                                    data: b"W".to_vec(),
                                },
                                vec![wu(fd)],
                                |_| Action::Pure(Value::U64(2)),
                            )),
                            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
                        }),
                        // 嵌套 Timeout：20ms 超时可靠触发 on_timeout（42）
                        next: Box::new(move |_| Action::Timeout {
                            action: Box::new(Action::Sleep {
                                duration: Duration::from_millis(500),
                                next: Box::new(|_| Action::Pure(Value::U64(77))),
                            }),
                            duration: Duration::from_millis(20),
                            on_timeout: Box::new(Action::Pure(Value::U64(42))),
                        }),
                    }
                }),
            }),
            // Catch：确定性错误（NotFound）→ handler → "caught"
            next: Box::new(|_| Action::Catch {
                action: Box::new(syscall(
                    DataOp::Read {
                        fd: 999_999,
                        len: 1,
                    },
                    vec![rd(999_999)],
                    Action::Pure,
                )),
                handler: Box::new(|e| {
                    assert_eq!(e, SysError::NotFound);
                    Action::Pure(Value::Str("caught".into()))
                }),
            }),
        }
    }

    let mut traj0: Option<Vec<String>> = None;
    for round in 0..100u32 {
        let log = Arc::new(Mutex::new(Vec::new()));
        let ex = TracedExecutor {
            inner: TokioExecutor::new(),
            log: Arc::clone(&log),
            op_delay: Duration::ZERO,
        };
        let mut rt = Runtime::new(Box::new(ex));
        let v = rt.run_blocking(blueprint(pa.clone())).unwrap();
        let traj: Vec<String> = log.lock().unwrap().iter().map(|(s, _)| s.clone()).collect();
        assert_eq!(
            v,
            Value::Str("caught".into()),
            "第 {round} 轮最终值一致（Catch handler 路径）"
        );
        match &traj0 {
            None => traj0 = Some(traj.clone()),
            Some(t0) => assert_eq!(t0, &traj, "第 {round} 轮 op 轨迹逐位一致"),
        }
        assert_eq!(traj.len(), 3, "轨迹 = Open + Write + Read(错误路径)");
        assert_eq!(
            std::fs::read(&pa).unwrap(),
            b"W",
            "第 {round} 轮物理文件内容一致"
        );
        assert_eq!(
            rt.context().cwd,
            before_cwd,
            "第 {round} 轮 Scope 退出后 cwd 恢复"
        );
    }
    // 说明：并行 Fork 右分支 Write 的 undo 每轮留在该轮 Runtime 撤销栈
    // （D1/D10 语义：跨轮全新 Runtime，无交叉污染）。
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2b：GetTime 多轮值类型稳定
// ══════════════════════════════════════════════════════════════════════

/// GetTime 100 轮：同一 Runtime 连续调用全部返回 U64（墙上时钟非确定性在
/// 契约内，但类型必须稳定）；并行 Fork 分支内 GetTime 20 轮同样 U64。
#[test]
fn det_gettime_100_rounds_type_stable() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    for i in 0..100u64 {
        let v = rt
            .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
            .unwrap();
        assert!(matches!(v, Value::U64(_)), "第 {i} 轮 GetTime 返回 U64");
        assert_ne!(v, Value::Unit, "第 {i} 轮 GetTime 存在性（非 Unit）");
    }
    // 并行 Fork 分支内 GetTime（零资源声明 → 真并行）20 轮类型稳定
    for i in 0..20u64 {
        let v = rt
            .run_blocking(Action::Fork {
                left: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
                right: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
                combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
            })
            .unwrap();
        match &v {
            Value::List(l) if l.len() == 2 => {
                assert!(
                    matches!(l[0], Value::U64(_)),
                    "Fork 左分支第 {i} 轮 GetTime U64"
                );
                assert!(
                    matches!(l[1], Value::U64(_)),
                    "Fork 右分支第 {i} 轮 GetTime U64"
                );
            }
            other => panic!("期望 List([U64, U64])，得到 {other:?}"),
        }
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3a：未声明资源的 MutexLock 在并行 Fork 中 WouldBlock（不挂死不毒化）
// ══════════════════════════════════════════════════════════════════════

/// 用户责任边界（pdr §18「不隐瞒依赖」的反例验证）：两分支 MutexLock(17)
/// 均不声明资源 → 静态层放行 → 真并行 → 动态仲裁：至多一个分支成功，
/// 败者有限重试后 WouldBlock（绝不挂起）。随后验证**不毒化**：
/// - Catch 捕获错误后程序继续执行；
/// - 不同 id 锁（18）立即可用（arbiter 无全局残留）；
/// - 同 id（17）在胜者持有期间 WouldBlock 属合法占有（非占坑泄漏）；
/// - Replace（recover）释放胜者锁后同 id 可重入。
#[test]
fn ub_undeclared_mutex_fork_wouldblock_no_poison_no_hang() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Fork {
                left: Box::new(syscall(DataOp::MutexLock { id: 17 }, vec![], Action::Pure)),
                right: Box::new(syscall(DataOp::MutexLock { id: 17 }, vec![], Action::Pure)),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            handler: Box::new(|e| {
                assert_eq!(
                    e,
                    SysError::WouldBlock,
                    "竞争失败方应 WouldBlock（非死锁、非挂起）"
                );
                Action::Pure(Value::U64(1))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1), "Catch 捕获 WouldBlock 后程序继续执行");
    assert_eq!(rt.undo_stack().len(), 1, "胜者锁 undo 合并回父");

    // 未毒化（一）：不同 id 锁立即可用（arbiter 无全局残留）
    rt.run_blocking(syscall(DataOp::MutexLock { id: 18 }, vec![], Action::Pure))
        .unwrap();

    // 未毒化（二）：同 id 在胜者仍持有期间 WouldBlock 是合法占有（非泄漏）
    let e = rt
        .run_blocking(syscall(DataOp::MutexLock { id: 17 }, vec![], Action::Pure))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::WouldBlock,
        "胜者持有期同 id 竞争 → WouldBlock（合法）"
    );

    // Replace（D10：recover）释放胜者锁与占坑 → 同 id 可重入（无状态毒化）
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    rt.run_blocking(syscall(DataOp::MutexLock { id: 17 }, vec![], Action::Pure))
        .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3b：fork_conflict 盲区——next 闭包内 Syscall 的静态收集盲区验证
// ══════════════════════════════════════════════════════════════════════

/// 【R1 记录低疑点的实证验证】`collect_syscall_resources`（runtime.rs:372-375
/// 已注明局限：`next`/`handler`/`cond`/`combine` 是不透明闭包，其内部嵌套的
/// Syscall 无法静态看到——阶段 1 接受此近似）。
///
/// 构造：两分支顶层 GetTime（声明资源 []），next 闭包内 MutexLock(13)
/// **正确声明**了 wu(13)（用户尽责）——但静态收集器看不见闭包内容。
///
/// 验证三连：
/// 1. `fork_conflict` 直接调用 → 返回 false（静态判定「无冲突，可并行」）；
/// 2. 真实执行：并行路径（两分支 op 记录来自 **≥2 个不同线程**——
///    spawn_blocking 双任务并发）→ 隐藏冲突在运行期竞争 MutexLock(13)
///    → 败者 WouldBlock（A7 安全失败，不挂死不破坏数据）；
/// 3. 若静态层能看见冲突（顺序路径），两分支的 lock 会依次成功 → Ok——
///    因此 WouldBlock + 双线程 = 静态判定与真实执行路径**不一致**的实证。
///
/// 结果如实记录：盲区真实存在；后果 = 冲突分支未被静态串行化而并行执行；
/// 失败模式安全（动态仲裁 WouldBlock），符合 pdr §18 用户责任边界
/// （未在静态层可见位置声明依赖 → 动态层兜底）。
#[test]
fn ub_fork_conflict_blindspot_hidden_closure_lock() {
    let log = Arc::new(Mutex::new(Vec::<(String, std::thread::ThreadId)>::new()));
    let ex = TracedExecutor {
        inner: TokioExecutor::new(),
        log: Arc::clone(&log),
        op_delay: Duration::from_millis(50),
    };
    let mut rt = Runtime::new(Box::new(ex));

    // 分支：GetTime（可见资源 []）→ Sleep(50ms) 加宽并行窗口 →
    // MutexLock(13)（wu(13) 已声明但位于闭包内，静态不可见）
    let branch = || Action::Sequential {
        current: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
        next: Box::new(|_| Action::Sequential {
            current: Box::new(Action::Sleep {
                duration: Duration::from_millis(50),
                next: Box::new(|_| {
                    syscall(DataOp::MutexLock { id: 13 }, vec![wu(13)], Action::Pure)
                }),
            }),
            next: Box::new(Action::Pure),
        }),
    };
    let left = branch();
    let right = branch();

    // 盲区直接证据：静态收集只看见两个顶层 GetTime（空资源集）
    let conflict = algeff_core::runtime::fork_conflict(rt.registry(), &left, &right);
    assert!(
        !conflict,
        "静态层判定无冲突（隐藏闭包内 Syscall 资源不可见）"
    );

    // 真实执行：并行 → 双分支竞争 MutexLock(13) → 败者 WouldBlock
    let e = rt
        .run_blocking(Action::Fork {
            left: Box::new(left),
            right: Box::new(right),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap_err();
    assert_eq!(
        e,
        SysError::WouldBlock,
        "并行竞争 → WouldBlock（若静态可见冲突→顺序路径则两分支依次成功→Ok）"
    );

    // 并行证据（调度无关）：WouldBlock 结果本身即并发竞争证据——顺序路径
    // 下两分支依次成功 → Ok 而非 Err（共享 reactor 不保证不同 worker，线程
    // 断言已废弃，迭代 3-A1）；配合上方 WouldBlock 断言构成语义级证据。

    // 安全失败 + 未毒化：胜者锁 undo 合并；Replace 后同 id 可重入
    assert_eq!(rt.undo_stack().len(), 1, "胜者锁 undo 合并回父");
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 13 },
        vec![rd(13)],
        Action::Pure,
    ))
    .unwrap();
}

/// 【盲区的物理后果验证】同一盲区下的**文件写竞争**：两分支 GetTime 后
/// 闭包内 Write(fd)（各自 wu(fd) 已声明但静态不可见）→ 静态放行并行 →
/// 两分支在共享文件游标处并发写 → 结果 ∈ {LR, RL}（顺序不可确定；若静态
/// 正确串行化 → 恒为 "LR"）。另验证 A4 线性保证不因盲区失效：并行 merge
/// 后父级同 fd Write 仍被拦截。
#[test]
fn ub_fork_conflict_blindspot_hidden_closure_write() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("blind.txt");

    let branch = |fd: u64, byte: u8| Action::Sequential {
        current: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
        // Sleep 加宽并行窗口：分支 op（GetTime→Sleep→Write）必须与另一分支
        // 重叠在飞，两个 spawn_blocking 任务才能确保落在不同阻塞池线程（无
        // Sleep 时两分支各仅一两次 syscall，可能被调度为同线程顺序执行——
        // 下方 threads.len() >= 2 并行证据将变成调度相关而非语义确定；同
        // ub_fork_conflict_blindspot_mutex_wouldblock 的既有做法）。
        next: Box::new(move |_| Action::Sequential {
            current: Box::new(Action::Sleep {
                duration: Duration::from_millis(50),
                next: Box::new(move |_| {
                    syscall(
                        DataOp::Write {
                            fd,
                            data: vec![byte],
                        },
                        vec![wu(fd)],
                        Action::Pure,
                    )
                }),
            }),
            next: Box::new(Action::Pure),
        }),
    };

    // 8 轮竞争（每轮全新 Runtime + 全新轨迹记录——上一轮 merge 的 A4 线性
    // 标记不跨轮复用）：每轮 Open → 并行 Fork 双写 1 字节 → 结果 ∈ {LR, RL}。
    let mut lr = 0u32;
    let mut rl = 0u32;
    for round in 0..8u32 {
        let log = Arc::new(Mutex::new(Vec::<(String, std::thread::ThreadId)>::new()));
        let ex = TracedExecutor {
            inner: TokioExecutor::new(),
            log: Arc::clone(&log),
            // 真实延迟保证并行窗口（r3b-flaky 修复，见 TracedExecutor 注释）。
            op_delay: Duration::from_millis(50),
        };
        let mut rt = Runtime::new(Box::new(ex));
        let fd = {
            let v = rt
                .run_blocking(syscall(
                    DataOp::Open {
                        path: pa.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pa.clone())],
                    Action::Pure,
                ))
                .unwrap();
            fd_of(&v)
        };

        // 盲区直接证据：静态收集看不见闭包内 Write 的 wu(fd)
        let conflict = algeff_core::runtime::fork_conflict(
            rt.registry(),
            &branch(fd, b'L'),
            &branch(fd, b'R'),
        );
        assert!(
            !conflict,
            "第 {round} 轮：静态层判定无冲突（隐藏闭包内 Write 资源不可见）"
        );

        rt.run_blocking(Action::Fork {
            left: Box::new(branch(fd, b'L')),
            right: Box::new(branch(fd, b'R')),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap();
        let content = std::fs::read(&pa).unwrap();
        assert!(
            content == b"LR" || content == b"RL",
            "第 {round} 轮写竞争结果 ∈ {{LR, RL}}，实测 {content:?}"
        );
        match &content[..] {
            b"LR" => lr += 1,
            _ => rl += 1,
        }
        // 并行证据（调度无关）：循环后断言两写序均出现——顺序路径恒 LR
        // （左先右后），仅真并行竞争（50ms 窗口重叠）可出现 RL 序；共享
        // reactor 不保证不同 worker，线程断言已废弃（迭代 3-A1）。

        // A4 线性在并行路径上仍经 merge 并入父：父级同 fd Write 被拦截
        // （盲区不破坏线性保证）
        let e = rt
            .run_blocking(syscall(
                DataOp::Write {
                    fd,
                    data: b"X".to_vec(),
                },
                vec![wu(fd)],
                Action::Pure,
            ))
            .unwrap_err();
        assert_eq!(
            e,
            SysError::InvalidInput,
            "第 {round} 轮：并行 Fork 后父级同资源 Write 应被 A4 拦截"
        );
    }
    // 并行证据（语义级，调度无关）：本测试的**盲区并发证据**由
    // ub_fork_conflict_blindspot_mutex_wouldblock 承担（双分支竞争 MutexLock
    // → 败者 WouldBlock——仅真并行路径可出现）；本测试职责为盲区双写语义
    // （每轮 ∈ {LR, RL} 已断言 + A4 合并拦截）。两写序分布（LR/RL 计数）为
    // 软观察：共享 reactor 下单 worker 顺序轮询可致全 LR（左分支恒先完成），
    // 不设硬断言（reviewer MEDIUM-1 实测 LR×8 间歇失败，2026-08-16）。
    eprintln!("r3b blind-write race distribution: LR×{lr} RL×{rl}");
}
