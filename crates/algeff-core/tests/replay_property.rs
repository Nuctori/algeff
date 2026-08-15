//! 随机蓝图 × interpret 的「执行-撤销-重放」属性测试（A6 Verification 批 5，最终属性）。
//!
//! 覆盖 pdr.md 三处要求：
//!   - §1.1 三大核心支柱（运行时代数层：trackΓ/recoverΓ、重放）+ §十八「确定性」——
//!     同一蓝图执行路径由输入唯一决定，可重放；
//!   - §四 公理 A6（撤销双态 w;w̄=1）与 A4（资源线性）在**执行级**的最终验证。
//!
//! 方法：proptest 随机生成 Action 树（IR `Bp`，深度 1..=4、节点数 ≤ 12，节点类型
//! Pure / Sequential / Choose(常量 cond) / Fork(两子树 Syscall，随机同/异资源) /
//! Syscall(随机 (资源, 模式) 对，Write/Own/Read，资源取有限池 Fd(1..=3)) /
//! Replace(随机子树)），经 `compile` 编译为真实 `Action` 后由真实解释器
//! `interpret` 驱动（自建 MockExecutor，与 execution_axioms.rs / concurrency_stress.rs
//! 同构），断言三个最终属性：
//!
//!   1. 重放一致性：同一随机蓝图在全新 (registry, undo, context) 上执行两次，
//!      两次 (最终 Value, MockExecutor op 序列, undo 序列, 撤销栈深) 完全一致
//!      （§1.1 支柱二 + §十八 确定性）；
//!   2. 撤销往返（A6 执行级）：执行 → `recover`（LIFO 逆序 + 栈清空）→ 全新状态
//!      重放同蓝图 → 轨迹一致（w;w̄=1 后初始态可重放）；
//!   3. 线性守恒（A4 执行级）：含 Write 的蓝图执行后，同资源第二次 Write 在同一
//!      registry 上被拒，且 **recover 之后仍被拒**——recover 恢复的是「状态」
//!      （撤销副作用）而非「线性标记」（consumed 集保留，实现如此，注释断言）。
//!
//! Fork 行为以当前实现为准：`interpret` 阶段 1（D14）= 静态冲突检测 + **顺序执行**
//! （left → right → combine），并行化尚未合并——因此 op 顺序恒为 left-before-right，
//! 对**全部**蓝图（含 Fork 同资源）断言全轨迹一致；若未来合并并行路径
//! （左右同资源 → can_parallel=false → 仍顺序化），属性 1/2 的断言依然成立。
//!
//! 工程约束（冻结签名所致，同批 3/4）：`interpret` 的 future 非 Send（`&mut dyn
//! SyscallExecutor` 无 Send 超 trait）→ 不能 `tokio::spawn` 直接驱动；全部用普通
//! `#[test]` + 本地 current-thread runtime `block_on`（每属性 128 cases，≥100 达标）。

use std::future::Future;
use std::sync::{Arc, Mutex};

use algeff_core::action::{Action, DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{AccessMode, Resource, ResourceRegistry, ResourceUsage};
use algeff_core::runtime::{interpret, Context, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoOp};
use proptest::prelude::*;

/// 本地 current-thread runtime 驱动（interpret future 非 Send；蓝图不含
/// Sleep/Timeout，无需 enable_all）。同一 case 内可复用多次 `block_on`。
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

// ── 蓝图 IR（Bp）：可派生 Clone/Debug，供 proptest 生成与重编译 ──────────
// Action 本身不可 Clone（NextFn 闭包），故随机蓝图以 IR 表示、每轮执行前
// `compile` 成全新 Action（闭包由 IR 数据重建 → 行为逐轮一致，保证可重放）。

#[derive(Debug, Clone, PartialEq)]
enum Bp {
    Pure(Value),
    /// Syscall 节点：有限资源池 Fd(1..=3) × (Write/Own/Read)。
    Syscall {
        res: u64,
        mode: BpMode,
        data_len: usize,
    },
    /// Sequential：先 current 后 next（next 忽略 current 值，语义等价 a;b）。
    Seq(Box<Bp>, Box<Bp>),
    /// Choose：常量 cond（确定性选支）。
    Choose(bool, Box<Bp>, Box<Bp>),
    /// Fork：两子树为 Syscall，随机同/异资源（D14 阶段 1 恒顺序执行）。
    Fork {
        left: Box<Bp>,
        right: Box<Bp>,
    },
    /// Replace：D10 —— 先 recover 再执行 target。
    Replace(Box<Bp>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BpMode {
    Write,
    Read,
    Own,
}

impl BpMode {
    fn access_mode(self) -> AccessMode {
        match self {
            BpMode::Write => AccessMode::Write,
            BpMode::Read => AccessMode::Read,
            BpMode::Own => AccessMode::Own,
        }
    }

    /// DataOp 构造（Own → Close，即 fd 终结语义）。
    fn to_op(self, fd: u64, data_len: usize) -> DataOp {
        match self {
            BpMode::Write => DataOp::Write {
                fd,
                data: vec![0xAA; data_len],
            },
            BpMode::Read => DataOp::Read { fd, len: data_len },
            BpMode::Own => DataOp::Close { fd },
        }
    }
}

/// 节点成本（budget 记账）：Pure/Syscall=1，Seq/Choose/Replace=1+子树，
/// Fork=3（自身 1 + 两个 Syscall 子树）。Invariant：子树成本 ≤ 子树 budget，
/// 顶层 budget=12 → 总节点数 ≤ 12（任务复杂度上限 ~12）。
fn count_nodes(bp: &Bp) -> usize {
    match bp {
        Bp::Pure(_) | Bp::Syscall { .. } => 1,
        Bp::Seq(a, b) | Bp::Choose(_, a, b) => 1 + count_nodes(a) + count_nodes(b),
        Bp::Fork { left, right } => 1 + count_nodes(left) + count_nodes(right),
        Bp::Replace(t) => 1 + count_nodes(t),
    }
}

fn arb_mode() -> impl Strategy<Value = BpMode> {
    prop_oneof![Just(BpMode::Write), Just(BpMode::Own), Just(BpMode::Read),]
}

fn arb_pure() -> impl Strategy<Value = Bp> {
    prop_oneof![
        Just(Value::Unit),
        any::<bool>().prop_map(Value::Bool),
        any::<u64>().prop_map(Value::U64),
        any::<String>().prop_map(Value::Str),
        proptest::collection::vec(any::<u64>(), 0..4)
            .prop_map(|v| Value::List(v.into_iter().map(Value::U64).collect())),
    ]
    .prop_map(Bp::Pure)
}

fn arb_syscall() -> impl Strategy<Value = Bp> {
    (0u64..3, arb_mode(), 1usize..4).prop_map(|(r, mode, data_len)| Bp::Syscall {
        res: r + 1, // 有限池：Fd(1)..=Fd(3)
        mode,
        data_len,
    })
}

/// Fork：左右子树为 Syscall，资源独立随机 → 同/异资源自然混合；combine 由
/// 顶层确定性 `merge_values` 承担。构造合法性：Fork 内资源不重叠 → can_parallel 真；
/// 重叠（Write×Write 等）→ 冲突检测为真但 D14 阶段 1 仍顺序执行（合法）。
fn arb_fork() -> impl Strategy<Value = Bp> {
    (arb_syscall(), arb_syscall()).prop_map(|(left, right)| Bp::Fork {
        left: Box::new(left),
        right: Box::new(right),
    })
}

/// 随机 Action 树：深度 1..=4（composite 层数），budget=12 → 节点数 ≤ 12。
/// 记账：叶子成本 1；Replace 起步成本 2（自身 1 + target ≥1）；Seq/Choose/Fork
/// 起步成本 3（自身 1 + 两子树 ≥1 / Fork 3）——budget 不足时对应选项不提供，
/// 维持 Invariant：子树成本 ≤ 子树 budget（children 预算和为 B-1，各 ≥1）。
fn arb_bp(depth: u32, budget: u32) -> impl Strategy<Value = Bp> {
    let leaf = prop_oneof![arb_pure(), arb_syscall()];
    if depth == 0 || budget <= 1 {
        leaf.boxed()
    } else if budget == 2 {
        // 一元 composite 起步成本 2：仅 Replace + 叶子。
        let composite = arb_bp(depth - 1, budget - 1).prop_map(|t| Bp::Replace(Box::new(t)));
        prop_oneof![leaf, composite].boxed()
    } else {
        let rest = budget - 1;
        let half = rest / 2;
        let composite = prop_oneof![
            (arb_bp(depth - 1, half), arb_bp(depth - 1, rest - half))
                .prop_map(|(a, b)| Bp::Seq(Box::new(a), Box::new(b))),
            (
                any::<bool>(),
                arb_bp(depth - 1, half),
                arb_bp(depth - 1, rest - half)
            )
                .prop_map(|(c, a, b)| Bp::Choose(c, Box::new(a), Box::new(b))),
            arb_fork(),
            arb_bp(depth - 1, rest).prop_map(|t| Bp::Replace(Box::new(t))),
        ];
        prop_oneof![leaf, composite].boxed()
    }
}

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

/// IR → Action。所有闭包只捕获 IR 克隆出的数据（Action 的 NextFn 为 'static），
/// 保证两次 `compile` 产生的 Action 行为逐字节一致（可重放前提）。
fn compile(bp: &Bp) -> Action {
    match bp {
        Bp::Pure(v) => Action::Pure(v.clone()),
        Bp::Syscall {
            res,
            mode,
            data_len,
        } => Action::Syscall {
            op: mode.to_op(*res, *data_len),
            resources: vec![usage(Resource::Fd(*res), mode.access_mode())],
            next: Box::new(Action::Pure),
        },
        Bp::Seq(a, b) => {
            let b = (**b).clone();
            Action::Sequential {
                current: Box::new(compile(a)),
                next: Box::new(move |_| compile(&b)),
            }
        }
        Bp::Choose(c, t, e) => {
            let c = *c;
            Action::Choose {
                cond: Box::new(move |_| c),
                then_branch: Box::new(compile(t)),
                else_branch: Box::new(compile(e)),
            }
        }
        Bp::Fork { left, right } => Action::Fork {
            left: Box::new(compile(left)),
            right: Box::new(compile(right)),
            combine: Box::new(|l, r| Action::Pure(merge_values(l, r))),
        },
        Bp::Replace(t) => Action::Replace {
            target: Box::new(compile(t)),
        },
    }
}

/// Fork combine 的确定性合并（顶层 fn，无捕获 → 逐轮一致）。
fn merge_values(l: Value, r: Value) -> Value {
    match (l, r) {
        (Value::Unit, Value::Unit) => Value::Unit,
        (Value::U64(a), Value::U64(b)) => Value::U64(a.wrapping_add(b)),
        (Value::Bool(a), Value::Bool(b)) => Value::Bool(a && b),
        (Value::Str(a), Value::Str(b)) => Value::Str(format!("{a}{b}")),
        (Value::List(mut a), Value::List(b)) => {
            a.extend(b);
            Value::List(a)
        }
        _ => Value::Unit,
    }
}

// ── MockExecutor：记录 op / undo 序列，返回值由 op 确定性导出 ─────────────
// 与 execution_axioms.rs 同构（记录序、with_undo），但返回值不查表——
// 直接由 op 决定（Write→U64(fd)，Read→U64(len)，Close→Unit），保证随机
// 蓝图的 (Value, op 序列) 逐轮可复现。

#[derive(Default)]
struct MockExecutor {
    log: Arc<Mutex<Vec<String>>>,
    undo_log: Arc<Mutex<Vec<String>>>,
    with_undo: bool,
}

impl MockExecutor {
    fn new() -> Self {
        Self::default()
    }

    fn ops(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn undo_ops(&self) -> Vec<String> {
        self.undo_log.lock().unwrap().clone()
    }
}

/// op → 稳定描述串（断言按此比较调用序）。
fn describe(op: &DataOp) -> String {
    match op {
        DataOp::Write { fd, data } => format!("write:{fd}:{}", data.len()),
        DataOp::Read { fd, len } => format!("read:{fd}:{len}"),
        DataOp::Close { fd } => format!("close:{fd}"),
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
        let value = match op {
            DataOp::Write { fd, .. } => Value::U64(*fd),
            DataOp::Read { len, .. } => Value::U64(*len as u64),
            DataOp::Close { .. } => Value::Unit,
            _ => Value::Unit,
        };
        Box::pin(async move {
            self.log.lock().unwrap().push(desc.clone());
            let undo: Option<UndoOp> = if self.with_undo {
                let label = format!("undo({desc})");
                let undo_log = self.undo_log.clone();
                Some(Box::pin(
                    async move { undo_log.lock().unwrap().push(label) },
                ))
            } else {
                None
            };
            Ok((value, undo))
        })
    }
}

/// 单次执行的完整可观测轨迹（三属性断言的最小充分集合）。
#[derive(Debug, Clone, PartialEq)]
struct RunTrace {
    value: Result<Value, SysError>,
    ops: Vec<String>,
    undos: Vec<String>,
    undo_left: usize,
}

/// 全新 (Context, UndoStack, ResourceRegistry, MockExecutor) 上执行一次蓝图。
fn run_once(bp: &Bp) -> RunTrace {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let value = drive(interpret(
        compile(bp),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    RunTrace {
        value,
        ops: ex.ops(),
        undos: ex.undo_ops(),
        undo_left: undo.len(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// 属性 1（重放一致性，pdr.md §1.1 支柱二 + §十八 确定性）：
    /// 同一随机蓝图在全新 (registry/undo/context) 上执行两次，两次的
    /// (最终 Value, op 序列, undo 序列, 撤销栈深) 完全一致。
    /// Fork 当前实现为 D14 阶段 1 顺序执行（left→right），故全轨迹可断言。
    #[test]
    fn prop_replay_consistent(bp in arb_bp(4, 12)) {
        prop_assert!(
            count_nodes(&bp) <= 12,
            "复杂度控制：节点数 ≤ 12（实际 {}）",
            count_nodes(&bp)
        );
        let t1 = run_once(&bp);
        let t2 = run_once(&bp);
        prop_assert_eq!(
            t1, t2,
            "同一随机蓝图两次全新状态执行：值/op 序列/undo 序列/撤销栈深完全一致（确定性）"
        );
    }

    /// 属性 2（撤销往返，pdr.md §四 A6 w;w̄=1 + §1.1 可重放）：
    /// 执行 → recover（LIFO 逆序执行全部逆操作、栈清空）→ 全新状态重放同蓝图
    /// → 轨迹与第一次一致。撤销栈清空后的状态等价于「初始 + 可重放」。
    #[test]
    fn prop_undo_roundtrip(bp in arb_bp(4, 12)) {
        // run1：全新状态
        let mut ex1 = MockExecutor::new();
        ex1.with_undo = true;
        let mut ctx1 = Context::new();
        let mut undo1 = UndoStack::new();
        let mut reg1 = ResourceRegistry::new();
        let value1 = drive(interpret(
            compile(&bp),
            &mut ctx1,
            &mut undo1,
            &mut reg1,
            &mut ex1,
        ));
        let t1 = RunTrace {
            value: value1,
            ops: ex1.ops(),
            undos: ex1.undo_ops(),
            undo_left: undo1.len(),
        };

        // A6 撤销：最终 recover 批次按 LIFO 逆序执行**尚未撤销**的 op（执行中
        // Replace 已把此前批次按各自 LIFO 撤掉并记入 t1.undos，批间顺序 = Replace
        // 出现顺序；最终批次 = 结尾 pending 的 op 逆序）。两段拼接即完整撤销序列。
        let pending = &t1.ops[t1.ops.len() - t1.undo_left..];
        let final_batch: Vec<String> = pending
            .iter()
            .rev()
            .map(|d| format!("undo({d})"))
            .collect();
        let expected_undos: Vec<String> = t1
            .undos
            .iter()
            .cloned()
            .chain(final_batch)
            .collect();
        drive(undo1.recover());
        prop_assert!(undo1.is_empty(), "recover 后撤销栈清空");
        prop_assert_eq!(
            ex1.undo_ops(),
            expected_undos,
            "最终 recover 批次按 LIFO 逆序执行 pending 逆操作（w;w̄=1）"
        );

        // run2：全新状态重放 → 轨迹一致（撤销往返后初始态可重放）
        let t2 = run_once(&bp);
        prop_assert_eq!(t1, t2, "撤销往返后重放同蓝图，轨迹与第一次完全一致");
    }

    /// 属性 3（线性守恒，pdr.md §四 A4 + §11 recoverΓ 语义）：
    /// 构造首 op 必为 Write(Fd(r)) 的随机蓝图（主路径消费标记建立），执行后：
    /// 同资源第二次 Write 在同一 registry 被拒；**recover 之后仍被拒** ——
    /// recover 恢复的是「状态」（撤销副作用）而非「线性标记」（consumed 集
    /// 保留，当前实现如此，注释断言）；全新 registry 上重放 → 轨迹一致。
    #[test]
    fn prop_linearity_marker_survives_recover(
        bp in arb_bp(3, 11),
        r in 0u64..3,
    ) {
        let res = Resource::Fd(r + 1);
        let with_write = Bp::Seq(
            Box::new(Bp::Syscall {
                res: r + 1,
                mode: BpMode::Write,
                data_len: 1,
            }),
            Box::new(bp),
        );

        // run1：全新状态
        let mut ex1 = MockExecutor::new();
        ex1.with_undo = true;
        let mut ctx1 = Context::new();
        let mut undo1 = UndoStack::new();
        let mut reg1 = ResourceRegistry::new();
        let value1 = drive(interpret(
            compile(&with_write),
            &mut ctx1,
            &mut undo1,
            &mut reg1,
            &mut ex1,
        ));
        let ops1 = ex1.ops();
        assert_eq!(
            ops1.first(),
            Some(&format!("write:{}:1", r + 1)),
            "首 op 必为 Write(Fd(r))（主路径）"
        );

        // 同 registry 第二次 Write 被拒：A4 消费标记保留
        assert_eq!(
            reg1.check_linear(&usage(res.clone(), AccessMode::Write)),
            Err(SysError::InvalidInput),
            "含 Write 的蓝图执行后，同资源第二次 Write 在同一 registry 上被拒"
        );

        // recover 后仍被拒：recover 恢复「状态」而非「线性标记」
        drive(undo1.recover());
        assert!(undo1.is_empty(), "recover 后撤销栈清空");
        assert_eq!(
            reg1.check_linear(&usage(res.clone(), AccessMode::Write)),
            Err(SysError::InvalidInput),
            "recover 不恢复线性标记：consumed 集保留（§11 recoverΓ 只执行逆操作）"
        );

        // 全新 registry 重放同蓝图 → 与第一次轨迹一致（可重放性）
        let t2 = run_once(&with_write);
        prop_assert_eq!(
            t2.value,
            value1,
            "全新 registry 上重放：最终值一致"
        );
        prop_assert_eq!(t2.ops, ops1, "全新 registry 上重放：op 序列一致");
    }
}
