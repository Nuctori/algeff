//! A3 交换律双序 commutation 测试（G4 终验条件 2，spec/g4-closure.md §5 条件-2）。
//!
//! 背景：g4-closure.md §1 A3 行残余-2「left∥right 与 right∥left 双序 commutation
//! 执行级测试未单列」→ §5 条件-2「A3 执行级双序 commutation 等价测试（对称 combine
//! 下 left∥right vs right∥left）补录」。本文件补录该测试，验证交换律的执行级表现：
//! 不相交资源 `Fork{a,b}` 与 `Fork{b,a}` 的执行结果一致（工程语义：结果与分支顺序
//! 无关，pdr.md §四 A3 `a∥b = b∥a`）。
//!
//! 覆盖：
//!   - 静态层：proptest 随机资源对 → `can_parallel(a,b) == can_parallel(b,a)`
//!     （A3 静态前提对称性）+ `can_parallel_with`（append 顺序无关 opt-in）同样对称；
//!   - 执行级（自建 MockExecutor，模式参考 tests/execution_axioms.rs）：
//!     - `fork_commutation_disjoint`：异资源双序 Fork（`{Syscall(r1), Syscall(r2)}`
//!       vs `{Syscall(r2), Syscall(r1)}`）分别经 interpret（Direct 通道 → 顺序路径）
//!       与 Runtime（Shared 通道 → can_parallel=true 真并行路径）执行，两蓝图
//!       最终值一致（combine 确定性求和）+ op 集合一致（顺序路径序列随分支序反转、
//!       并行路径顺序不确定，统一断言多重集合相等而非序列相等）；
//!     - `fork_commutation_same_value`：同值不同序 Pure 场景（A1/A3 联合）——
//!       `Fork{left: Pure(1), right: Pure(2)}` 交换序后 combine 结果一致，且
//!       Pure 分支不触碰执行器（零 op）。
//!
//! A2 批 5 差异说明：A2 批 5（fd 区间分割修复）**已合并**（`38bca67`）。本测试全部使用
//! `Fd(1)/Fd(2)` 静态声明的**纯资源隔离**场景，不依赖运行时 fd 分配区间语义
//! （不新分配 fd、无子 registry 句柄并入父的区间交互），因此修复前后断言均不受影响。
//!
//! 工程约束（同 execution_axioms.rs）：`interpret` 的 future 因冻结签名
//! `&mut dyn SyscallExecutor`（trait 无 `Send` 超 trait）而**非 Send**，用普通
//! `#[test]` + 本地 current-thread runtime 驱动（`drive`）；`Runtime::new` 须在
//! tokio 上下文之外调用（D9）。命名含公理编号属任务强制命名，故文件级允许
//! non_snake_case。

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use algeff_core::action::{Action, DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{AccessMode, Resource, ResourceRegistry, ResourceSet, ResourceUsage};
use algeff_core::runtime::{interpret, Context, Runtime, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoOp};
use proptest::prelude::*;

/// 本地 current-thread runtime 驱动（interpret/recover future 非 Send）。
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

/// execute 的返回结果配置。
#[derive(Clone)]
enum MockOutcome {
    Value(Value),
}

/// 可配置 Mock 执行器：记录 op 调用序列，可按 op 描述返回 Value/Err/undo。
/// 模式参考 tests/execution_axioms.rs（log 用 Arc<Mutex> 使并行路径两子任务
/// 可并发写入同一日志）。
#[derive(Default)]
struct MockExecutor {
    /// 每次 execute 的 op 描述（调用顺序）。
    log: Arc<Mutex<Vec<String>>>,
    /// op 描述 → 返回结果（未配置 → Ok(Value::Unit)）。
    responses: HashMap<String, MockOutcome>,
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
}

/// op → 稳定描述串（测试按此配置响应并断言调用序）。
fn describe(op: &DataOp) -> String {
    match op {
        DataOp::Read { fd, len } => format!("read:{fd}:{len}"),
        DataOp::Write { fd, data } => format!("write:{fd}:{}", data.len()),
        DataOp::GetTime => "gettime".to_string(),
        other => format!("{other:?}"),
    }
}

impl SyscallExecutor for MockExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        let desc = describe(op);
        Box::pin(async move {
            self.log.lock().unwrap().push(desc.clone());
            let out = match self.responses.get(&desc).cloned() {
                Some(MockOutcome::Value(v)) => v,
                None => Value::Unit,
            };
            Ok((out, None))
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

/// op 日志多重集合比较：排序后逐元素相等。
/// 并行路径下两子任务并发、op 顺序不确定 → 断言多重集合相等而非序列相等
/// （顺序路径同样成立：序列只是随分支序反转，集合不变）。
fn assert_same_multiset(a: &[String], b: &[String]) {
    let mut a = a.to_vec();
    let mut b = b.to_vec();
    a.sort();
    b.sort();
    assert_eq!(a, b, "op 多重集合应一致（执行顺序可能不同）");
}

// ── 静态层：can_parallel 对称性（pdr.md §四 A3 静态前提）────────────────

/// 小宇宙资源生成器：提高资源碰撞概率，让 `can_parallel` 冲突分支被充分覆盖。
/// （与 tests/axioms.rs 的 arb 生成器同构，本文件独立复用于双序对称性。）
fn arb_resource() -> impl Strategy<Value = Resource> {
    prop_oneof![
        (0u64..8).prop_map(Resource::Fd),
        (0u32..4).prop_map(Resource::Pid),
        ((0usize..4), (0usize..4)).prop_map(|(a, b)| Resource::MemRange(a, b)),
        (0u64..4).prop_map(Resource::Foreign),
        (0u64..4).prop_map(|n| Resource::Path(format!("/r{n}"))),
        Just(Resource::Signal),
    ]
}

fn arb_mode() -> impl Strategy<Value = AccessMode> {
    prop_oneof![
        Just(AccessMode::Read),
        Just(AccessMode::Write),
        Just(AccessMode::Append),
        Just(AccessMode::Own),
    ]
}

fn arb_usage() -> impl Strategy<Value = ResourceUsage> {
    (arb_resource(), arb_mode()).prop_map(|(resource, mode)| ResourceUsage { resource, mode })
}

fn arb_resource_set() -> impl Strategy<Value = ResourceSet> {
    proptest::collection::vec(arb_usage(), 0..8)
}

proptest! {
    /// A3 交换律对称性（G4 条件 2 静态前提）：随机 ResourceSet 对下
    /// `can_parallel` 与 `can_parallel_with`（append 顺序无关 opt-in）均交换不变。
    #[test]
    fn a3_can_parallel_symmetric(ref a in arb_resource_set(), ref b in arb_resource_set()) {
        let reg = ResourceRegistry::new();
        prop_assert_eq!(reg.can_parallel(a, b), reg.can_parallel(b, a));
        prop_assert_eq!(
            reg.can_parallel_with(a, b, true),
            reg.can_parallel_with(b, a, true)
        );
    }
}

// ── 执行级公共构造 ─────────────────────────────────────────────────────

/// combine：确定性求和（可交换的合并函数，保证双序结果可比）。
fn combine_sum(l: Value, r: Value) -> Action {
    match (l, r) {
        (Value::U64(a), Value::U64(b)) => Action::Pure(Value::U64(a + b)),
        _ => Action::Pure(Value::Unit),
    }
}

/// 异资源双序蓝图对：
/// - `ab` = Fork{left: Syscall(Read r1), right: Syscall(Read r2)}
/// - `ba` = Fork{left: Syscall(Read r2), right: Syscall(Read r1)}
///
/// 每次调用重建（Action 被 interpret 消费，不可复用）。
fn disjoint_fork_pair() -> (Action, Action) {
    let r1 = usage(Resource::Fd(1), AccessMode::Read);
    let r2 = usage(Resource::Fd(2), AccessMode::Read);
    let mk = |l: Action, r: Action| Action::Fork {
        left: Box::new(l),
        right: Box::new(r),
        combine: Box::new(combine_sum),
    };
    let ab = mk(
        syscall_step(DataOp::Read { fd: 1, len: 4 }, vec![r1.clone()]),
        syscall_step(DataOp::Read { fd: 2, len: 4 }, vec![r2.clone()]),
    );
    let ba = mk(
        syscall_step(DataOp::Read { fd: 2, len: 4 }, vec![r2]),
        syscall_step(DataOp::Read { fd: 1, len: 4 }, vec![r1]),
    );
    (ab, ba)
}

/// read:1:4 → 10、read:2:4 → 20（combine 求和期望 30）。
fn cfg_reads() -> MockExecutor {
    let mut ex = MockExecutor::new();
    ex.respond("read:1:4", MockOutcome::Value(Value::U64(10)));
    ex.respond("read:2:4", MockOutcome::Value(Value::U64(20)));
    ex
}

/// 经 interpret（Direct 通道 → Fork 恒顺序执行）跑蓝图，返回 (值, op 日志)。
fn run_sequential_with(
    a: Action,
    mk_ex: impl FnOnce() -> MockExecutor,
) -> (Result<Value, SysError>, Vec<String>) {
    let mut ex = mk_ex();
    let v = drive(interpret(
        a,
        &mut Context::new(),
        &mut UndoStack::new(),
        &mut ResourceRegistry::new(),
        &mut ex,
    ));
    let ops = ex.ops();
    (v, ops)
}

/// 经 Runtime（Shared 通道 → can_parallel=true 时真并行）跑蓝图，返回 (值, op 日志)。
fn run_parallel_with(
    a: Action,
    mk_ex: impl FnOnce() -> MockExecutor,
) -> (Result<Value, SysError>, Vec<String>) {
    let ex = mk_ex();
    let log = Arc::clone(&ex.log);
    let mut rt = Runtime::new(Box::new(ex));
    let v = rt.run_blocking(a);
    let ops = log.lock().unwrap().clone();
    (v, ops)
}

// ── 执行级：双序 commutation（G4 条件 2）──────────────────────────────

/// 异资源双序 Fork 等价：Fork{a,b} 与 Fork{b,a} 执行结果一致。
///
/// 调度双路径全覆盖（pdr.md §四 A3 工程语义——结果与分支顺序无关）：
/// 1. interpret（Direct 通道，顺序路径）：最终值一致 + op 多重集合一致
///    （顺序路径序列确定：随分支序反转，左分支先执行）；
/// 2. Runtime（Shared 通道，can_parallel=true → 真并行路径）：最终值一致 +
///    op 多重集合一致（并行下序列不确定，断言集合相等而非序列相等）。
#[test]
fn fork_commutation_disjoint() {
    // A3 静态前提：异资源 Read×Read 不相交 → can_parallel=true（调度进并行路径）
    let r1 = usage(Resource::Fd(1), AccessMode::Read);
    let r2 = usage(Resource::Fd(2), AccessMode::Read);
    assert!(
        ResourceRegistry::new().can_parallel(&vec![r1], &vec![r2]),
        "异资源 Read×Read 应可并行（pdr.md §四 A3 前提）"
    );

    // 顺序路径（interpret Direct 通道）
    let (ab, ba) = disjoint_fork_pair();
    let (v_ab, ops_ab) = run_sequential_with(ab, cfg_reads);
    let (v_ba, ops_ba) = run_sequential_with(ba, cfg_reads);
    assert_eq!(
        v_ab, v_ba,
        "顺序路径：Fork{{a,b}} 与 Fork{{b,a}} 最终值一致（A3 交换律执行级）"
    );
    assert_eq!(v_ab, Ok(Value::U64(30)), "combine 确定性求和：10+20=30");
    // 顺序路径序列确定：left→right，双序下序列反转（顺序实现说明）
    assert_eq!(
        ops_ab,
        vec!["read:1:4", "read:2:4"],
        "Fork{{a,b}} left→right"
    );
    assert_eq!(
        ops_ba,
        vec!["read:2:4", "read:1:4"],
        "Fork{{b,a}} left→right"
    );
    assert_same_multiset(&ops_ab, &ops_ba);

    // 并行路径（Runtime Shared 通道，can_parallel=true → spawn_blocking 真并行）
    let (ab, ba) = disjoint_fork_pair();
    let (v_ab_p, ops_ab_p) = run_parallel_with(ab, cfg_reads);
    let (v_ba_p, ops_ba_p) = run_parallel_with(ba, cfg_reads);
    assert_eq!(
        v_ab_p, v_ba_p,
        "并行路径：Fork{{a,b}} 与 Fork{{b,a}} 最终值一致（A3 交换律执行级）"
    );
    assert_eq!(v_ab_p, Ok(Value::U64(30)), "并行 combine 求和一致");
    assert_eq!(ops_ab_p.len(), 2, "并行路径左右分支各执行一个 op");
    assert_same_multiset(&ops_ab_p, &ops_ba_p);

    // 双路径交叉一致：同一蓝图在顺序/并行路径下最终值相同
    assert_eq!(v_ab, v_ab_p, "顺序与并行路径下 Fork{{a,b}} 值一致");
    assert_eq!(v_ba, v_ba_p, "顺序与并行路径下 Fork{{b,a}} 值一致");
}

/// 同值不同序 Pure 场景（A1/A3 联合）：Fork{left: Pure(1), right: Pure(2)}
/// 交换序后 combine 结果一致 —— 纯节点不触碰执行器（零 op），combine 仅对
/// 值流确定性求和，验证「结果与分支顺序无关」在纯值层的表现。
#[test]
fn fork_commutation_same_value() {
    let mk = |l: u64, r: u64| Action::Fork {
        left: Box::new(Action::Pure(Value::U64(l))),
        right: Box::new(Action::Pure(Value::U64(r))),
        combine: Box::new(combine_sum),
    };

    // 顺序路径
    let (v_12, ops_12) = run_sequential_with(mk(1, 2), MockExecutor::new);
    let (v_21, ops_21) = run_sequential_with(mk(2, 1), MockExecutor::new);
    assert_eq!(
        v_12, v_21,
        "顺序路径：Pure{{1,2}} 与 Pure{{2,1}} combine 结果一致（A3 纯值层）"
    );
    assert_eq!(v_12, Ok(Value::U64(3)), "combine 确定性求和：1+2=3");
    assert!(ops_12.is_empty() && ops_21.is_empty(), "Pure 分支不产生 op");

    // 并行路径（空资源集 can_parallel=true → 真并行，含空 registry 合并回父）
    let (v_12_p, ops_12_p) = run_parallel_with(mk(1, 2), MockExecutor::new);
    let (v_21_p, ops_21_p) = run_parallel_with(mk(2, 1), MockExecutor::new);
    assert_eq!(
        v_12_p, v_21_p,
        "并行路径：Pure{{1,2}} 与 Pure{{2,1}} combine 结果一致（A3 纯值层）"
    );
    assert_eq!(v_12_p, Ok(Value::U64(3)), "并行 combine 求和一致");
    assert!(
        ops_12_p.is_empty() && ops_21_p.is_empty(),
        "Pure 分支不产生 op"
    );

    // 双路径交叉一致
    assert_eq!(v_12, v_12_p, "顺序与并行路径下 Pure{{1,2}} 值一致");
    assert_eq!(v_21, v_21_p, "顺序与并行路径下 Pure{{2,1}} 值一致");
}
