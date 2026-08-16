//! R5 对抗审计（终轮，分块 B —— 最后新颖面）：Invoke 节点语义 —— 假执行器。
//!
//! 攻击方法论：与 R3b/R4b core 侧相同——真实 `Runtime`（`run_blocking` →
//! `interpret_impl` 全链路）驱动；本文件的本体是**自定义假执行器**，覆写
//! `SyscallExecutor::invoke` 记录每次调用的 (foreign_id, captures, deterministic)
//! 三元组，攻击 R1-R4 **从未覆盖的 Invoke 正向语义**：
//!
//! R4b 只测了 invoke 的 **trait 默认实现**（ENOSYS 透传）；interpreter.rs 只
//! 测默认 ENOSYS。**没有任何既有测试覆写 invoke 验证正向行为**。本文件攻击：
//!
//! 1. **captures 传递**：蓝图 `Action::Invoke.captures`（非平凡 ResourceSet，
//!    多资源多模式）必须**原样**到达执行器（执行器侧收到完全相同的集合，
//!    相等语义断言）；`yields` 由解释器丢弃（`yields: _`）——变更 yields
//!    不影响执行器收到什么、不影响结果（不变量锁定）。
//! 2. **deterministic 标志**：`deterministic: true/false` 两态都原样到达
//!    执行器（执行器记录的是蓝图声明的值）。
//! 3. **next 接续**：执行器 invoke 的返回值原样进入 `next`，链上继续（两个
//!    Invoke 串联：值贯穿 + 调用顺序记录）。
//! 4. **错误传播**：执行器 invoke 返回错误 → 解释器原样透传（不包装/改码），
//!    Catch 可捕获；不压 undo。
//! 5. **并行 Fork 共享通道**：invoke 经 `Arc<Mutex>` 共享通道（`invoke_via`
//!    Shared 路径）在真并行分支内互斥调用，两分支记录均落地、值经 combine
//!    合并 —— Invoke 在 D14 并行路径下语义成立。
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use algeff_core::{
    Action, BoxFuture, DataOp, Id, Owned, ReadOnly, ResourceInner, ResourceRegistry, ResourceSet,
    ResourceUsage, Runtime, SysError, SyscallExecutor, TypedResource, UndoOp, Value, WriteOnly,
};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn rd_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Path(path)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}
fn ow_foreign(id: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Foreign(id)).into_usage()
}

/// 记录型 invoke 执行器（R5b 本体）：`execute` 一律 Ok((Unit, None))；
/// `invoke` 记录 (foreign_id, captures 克隆, deterministic) 三元组并返回
/// 由 foreign_id 派生的确定值（`Value::U64(foreign_id * 1000 + 捕获数)`）。
/// 可选 `fail_on`：对指定 foreign_id 返回固定错误（错误传播测试用）。
struct RecordingInvokeExecutor {
    calls: Arc<Mutex<Vec<(Id, ResourceSet, bool)>>>,
    fail_on: Option<Id>,
}

impl RecordingInvokeExecutor {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            fail_on: None,
        }
    }
}

impl SyscallExecutor for RecordingInvokeExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async { Ok((Value::Unit, None)) })
    }

    fn invoke<'a>(
        &'a mut self,
        foreign_id: Id,
        captures: &'a ResourceSet,
        deterministic: bool,
    ) -> BoxFuture<'a, Result<Value, SysError>> {
        let n_captures = captures.len();
        self.calls
            .lock()
            .unwrap()
            .push((foreign_id, captures.clone(), deterministic));
        if Some(foreign_id) == self.fail_on {
            return Box::pin(async { Err(SysError::Other(99)) });
        }
        let v = Value::U64(foreign_id * 1000 + n_captures as u64);
        Box::pin(async move { Ok(v) })
    }
}

/// 非平凡 captures：3 类资源 4 种模式（Fd Read / Fd Write / Path Read /
/// Path Write / Foreign Own），用于断言 captures 原样到达执行器。
fn nontrivial_captures() -> ResourceSet {
    vec![
        rd(7),
        wr(9),
        rd_path(PathBuf::from("/data/cap.bin")),
        wr_path(PathBuf::from("/data/cap.bin")),
        ow_foreign(55),
    ]
}

/// 与 captures 对称的另一组 yields（仅用于验证 yields 被解释器丢弃）。
fn nontrivial_yields() -> ResourceSet {
    vec![
        rd(1),
        wr_path(PathBuf::from("/out/yield.bin")),
        ow_foreign(66),
    ]
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1：captures 原样传递 + deterministic 标志两态 + next 值流接续。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn invoke_captures_reach_executor_exactly_and_deterministic_true() {
    let ex = RecordingInvokeExecutor::new();
    let calls = ex.calls.clone();
    let mut rt = Runtime::new(Box::new(ex));
    let captures = nontrivial_captures();

    let v = rt
        .run_blocking(Action::Invoke {
            foreign_id: 42,
            captures: captures.clone(),
            yields: vec![],
            deterministic: true,
            next: Box::new(Action::Pure),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::U64(42 * 1000 + captures.len() as u64),
        "invoke 返回值经 next 原样收敛"
    );

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1, "恰好一次 invoke 调用");
    let (fid, got_caps, det) = &calls[0];
    assert_eq!(*fid, 42, "foreign_id 原样到达执行器");
    assert_eq!(
        got_caps, &captures,
        "captures 原样到达执行器（多资源多模式逐项相等）"
    );
    assert!(*det, "deterministic=true 原样到达执行器");
    assert!(rt.undo_stack().is_empty(), "invoke 不产生 undo");
}

#[test]
fn invoke_deterministic_false_and_two_node_chain_value_flow_order() {
    let ex = RecordingInvokeExecutor::new();
    let calls = ex.calls.clone();
    let mut rt = Runtime::new(Box::new(ex));

    // 两个 Invoke 串联：第一个的返回值进入 next，next 内构造第二个 Invoke
    // （foreign_id=8、deterministic=false），值流贯穿 + 调用顺序可记录。
    let v = rt
        .run_blocking(Action::Invoke {
            foreign_id: 7,
            captures: vec![rd(1)],
            yields: vec![],
            deterministic: false,
            next: Box::new(move |first| {
                assert_eq!(
                    first,
                    Value::U64(7 * 1000 + 1),
                    "第一个 invoke 返回值进入 next"
                );
                Action::Invoke {
                    foreign_id: 8,
                    captures: vec![wr(3), rd_path(PathBuf::from("/p"))],
                    yields: vec![],
                    deterministic: false,
                    next: Box::new(move |second| {
                        assert_eq!(
                            second,
                            Value::U64(8 * 1000 + 2),
                            "第二个 invoke 返回值进入 next"
                        );
                        Action::Pure(Value::List(vec![first, second]))
                    }),
                }
            }),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::List(vec![Value::U64(7001), Value::U64(8002)]),
        "两节点 Invoke 链值流保真"
    );

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "两个 Invoke 恰好各调用一次");
    assert_eq!(calls[0].0, 7, "第一个 invoke 的 foreign_id");
    assert!(!calls[0].2, "第一个 invoke deterministic=false 原样到达");
    assert_eq!(calls[1].0, 8, "第二个 invoke 的 foreign_id");
    assert!(!calls[1].2, "第二个 invoke deterministic=false 原样到达");
    assert_eq!(calls[1].1, vec![wr(3), rd_path(PathBuf::from("/p"))]);
    assert!(rt.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：yields 字段惰性（解释器丢弃）——变更 yields 不影响行为。
// 锁定 runtime.rs `Invoke { yields: _, .. }` 的丢弃语义为可观察不变量。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn invoke_yields_field_inert_varying_it_changes_nothing() {
    // 两组完全对称的运行：仅 yields 不同（空 vs 非平凡 3 项）。
    // 执行器记录、最终值、undo 三者都必须完全一致 —— yields 是蓝图级声明
    // （由宿主解释/消费），解释器不把它传给执行器、不参与语义。
    let mut results = Vec::new();
    for yields in [vec![], nontrivial_yields()] {
        let ex = RecordingInvokeExecutor::new();
        let calls = ex.calls.clone();
        let mut rt = Runtime::new(Box::new(ex));
        let captures = nontrivial_captures();
        let v = rt
            .run_blocking(Action::Invoke {
                foreign_id: 5,
                captures: captures.clone(),
                yields,
                deterministic: true,
                next: Box::new(Action::Pure),
            })
            .unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, 5);
        assert_eq!(calls[0].1, captures, "yields 不影响执行器收到的 captures");
        assert!(calls[0].2);
        results.push((v, rt.undo_stack().len()));
    }
    assert_eq!(
        results[0], results[1],
        "yields 空/非平凡两组运行结果完全一致（yields 惰性）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：执行器 invoke 错误原样透传 + Catch 捕获 + 运行时不毒化。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn invoke_error_passthrough_catch_handles_and_runtime_alive() {
    let ex = RecordingInvokeExecutor {
        calls: Arc::new(Mutex::new(Vec::new())),
        fail_on: Some(99),
    };
    let calls = ex.calls.clone();
    let mut rt = Runtime::new(Box::new(ex));

    // Catch{ Invoke(99) } → handler 记录错误并返回 U64(1)；
    // 随后同 Runtime 上再执行一个正常 Invoke(3) —— 错误后运行时未被毒化。
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Invoke {
                foreign_id: 99,
                captures: vec![rd(1)],
                yields: vec![],
                deterministic: true,
                next: Box::new(Action::Pure),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::Other(99), "执行器 invoke 错误原样透传");
                Action::Pure(Value::U64(1))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1), "Catch 捕获 invoke 错误后收敛");

    // 错误不压 undo；运行时继续执行正常 invoke。
    assert!(rt.undo_stack().is_empty(), "invoke 错误不产生 undo");
    let v = rt
        .run_blocking(Action::Invoke {
            foreign_id: 3,
            captures: vec![],
            yields: vec![],
            deterministic: false,
            next: Box::new(Action::Pure),
        })
        .unwrap();
    assert_eq!(v, Value::U64(3000), "错误后同 Runtime 继续 invoke 正常");

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "两次 invoke 均到达执行器（含失败那次）");
    assert_eq!(calls[0].0, 99);
    assert_eq!(calls[1].0, 3);
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4：invoke 经并行 Fork 共享通道（Arc<Mutex>）互斥调用 ——
// 真并行分支内各一次 invoke，记录均落地、值经 combine 合并。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn invoke_in_parallel_fork_branches_shared_channel_records_merged() {
    let ex = RecordingInvokeExecutor::new();
    let calls = ex.calls.clone();
    let mut rt = Runtime::new(Box::new(ex));

    // 两分支各一个 Invoke（资源声明不同但 invoke 不参与静态冲突收集——
    // collect_syscall_resources 只收集 Syscall 节点 → 无资源 → can_parallel
    // = true → 真并行路径 → invoke_via 经 Arc<Mutex> 共享通道互斥调用）。
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(Action::Invoke {
                foreign_id: 11,
                captures: vec![rd(1)],
                yields: vec![],
                deterministic: true,
                next: Box::new(Action::Pure),
            }),
            right: Box::new(Action::Invoke {
                foreign_id: 22,
                captures: vec![wr(2)],
                yields: vec![],
                deterministic: false,
                next: Box::new(Action::Pure),
            }),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();

    // 两分支值经 combine 合并：11*1000+1 与 22*1000+1（分支并行无序，比对集合）。
    let got = match v {
        Value::List(l) => l,
        other => panic!("期望 List，得到 {other:?}"),
    };
    let mut got_ids: Vec<u64> = got
        .iter()
        .map(|x| match x {
            Value::U64(n) => *n,
            other => panic!("期望 U64，得到 {other:?}"),
        })
        .collect();
    got_ids.sort();
    assert_eq!(
        got_ids,
        vec![11_001, 22_001],
        "并行分支 invoke 返回值经 combine 合并保真"
    );

    // 两条记录均落地（顺序无关，按 foreign_id 检索断言）。
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "并行两分支 invoke 各调用一次");
    let rec_11 = calls
        .iter()
        .find(|(id, _, _)| *id == 11)
        .expect("左分支记录");
    let rec_22 = calls
        .iter()
        .find(|(id, _, _)| *id == 22)
        .expect("右分支记录");
    assert_eq!(rec_11.1, vec![rd(1)], "左分支 captures 原样");
    assert!(rec_11.2, "左分支 deterministic=true 原样");
    assert_eq!(rec_22.1, vec![wr(2)], "右分支 captures 原样");
    assert!(!rec_22.2, "右分支 deterministic=false 原样");
    assert!(rt.undo_stack().is_empty(), "invoke 全路径不产生 undo");
}
