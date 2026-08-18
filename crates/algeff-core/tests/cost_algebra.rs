//! 效应开销代数（cost.rs）定律驱动测试。
//!
//! 目标：对 `cost.rs` 中**每一个数学定义/定律**给出可执行判定（不止行为测试）。
//! 覆盖 task 规定的 8 大类：
//!   1. `Grade.plus` 区间加法 monoid（结合/单位元/交换/saturating 溢出）
//!   2. `Grade.peak` 格 join（结合/幂等/单位元/交换）
//!   3. `Grade.with_max` 单调性
//!   4. `EffectCost.plus` 三元组积 monoid
//!   5. 量纲隔离（read/write/occupy 不可跨维）
//!   6. `CostBudget.exceeded_by`（仅 max 越界 / UNBOUNDED / 边界 ==）
//!   7. `DataOp::for_op` 静态派生正确性（逐 op 落维 + 纯函数性 + 度量语义）
//!   8. 幂等塌缩 C1 的代数层确认（for_op 纯函数 + 运行时唯一记账落点）
//!
//! 框架：复用仓库已存在的 `proptest`（dev-dependency）；定律用随机生成 + 边界
//! 枚举双重覆盖。不引入新依赖（YAGNI，符合约束）。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;

use algeff_core::action::DataOp;
use algeff_core::cost::{CostBudget, EffectCost, Grade};

use proptest::prelude::*;

// ───────────────────────────── 构造辅助 ─────────────────────────────

fn g(min: u64, max: u64) -> Grade {
    Grade { min, max }
}

/// 三维开销构造（read/write/occupy 各为闭区间）。
fn ec(read: Grade, write: Grade, occupy: Grade) -> EffectCost {
    EffectCost {
        read,
        write,
        occupy,
    }
}

fn p(n: u64) -> Grade {
    Grade::point(n)
}

// ───────────────────────────── 1. Grade.plus monoid ─────────────────────────────

proptest! {
    #[test]
    fn grade_plus_associative(a in arb_grade(), b in arb_grade(), c in arb_grade()) {
        // (a + b) + c == a + (b + c)
        let left = a.plus(b).plus(c);
        let right = a.plus(b.plus(c));
        prop_assert_eq!(left, right);
    }

    #[test]
    fn grade_plus_commutative(a in arb_grade(), b in arb_grade()) {
        prop_assert_eq!(a.plus(b), b.plus(a));
    }
}

#[test]
fn grade_plus_left_unit() {
    // ZERO + a == a
    let a = g(3, 7);
    assert_eq!(Grade::ZERO.plus(a), a);
    assert_eq!(Grade::ZERO.plus(Grade::ZERO), Grade::ZERO);
}

#[test]
fn grade_plus_right_unit() {
    // a + ZERO == a
    let a = g(3, 7);
    assert_eq!(a.plus(Grade::ZERO), a);
}

#[test]
fn grade_plus_saturating_overflow() {
    // min/max 各自 saturating_add；u64::MAX + 1 饱和到 u64::MAX。
    assert_eq!(
        Grade::point(u64::MAX).plus(Grade::point(1)),
        Grade::point(u64::MAX)
    );
    assert_eq!(g(u64::MAX, u64::MAX).plus(g(5, 5)), g(u64::MAX, u64::MAX));
    // 跨分量互不干扰：min 饱和但 max 不，反之亦然。
    assert_eq!(g(1, u64::MAX).plus(g(u64::MAX, 1)), g(u64::MAX, u64::MAX));
    // 精确边界：MAX/2 + MAX/2 = MAX（无饱和）。
    let half = u64::MAX / 2;
    assert_eq!(
        g(half, half).plus(g(half, half)),
        g(u64::MAX - 1, u64::MAX - 1)
    );
    // 零元不溢出。
    assert_eq!(g(0, 0).plus(g(0, 0)), g(0, 0));
}

// ───────────────────────────── 2. Grade.peak 格 join ─────────────────────────────

proptest! {
    #[test]
    fn grade_peak_associative(a in arb_grade(), b in arb_grade(), c in arb_grade()) {
        prop_assert_eq!(a.peak(b).peak(c), a.peak(b.peak(c)));
    }

    #[test]
    fn grade_peak_commutative(a in arb_grade(), b in arb_grade()) {
        prop_assert_eq!(a.peak(b), b.peak(a));
    }
}

#[test]
fn grade_peak_idempotent() {
    // peak(a, a) == a
    let a = g(2, 9);
    assert_eq!(a.peak(a), a);
}

#[test]
fn grade_peak_left_unit_zero() {
    // ZERO 是 peak 单位元：max(0, x) == x。
    let a = g(4, 8);
    assert_eq!(Grade::ZERO.peak(a), a);
}

#[test]
fn grade_peak_right_unit_zero() {
    let a = g(4, 8);
    assert_eq!(a.peak(Grade::ZERO), a);
}

// ───────────────────────────── 3. Grade.with_max 单调性 ─────────────────────────────

proptest! {
    #[test]
    fn with_max_keeps_min(a in arb_grade(), m in any::<u64>()) {
        // min 分量不变。
        prop_assert_eq!(a.with_max(m).min, a.min);
    }

    #[test]
    fn with_max_takes_max(a in arb_grade(), m in any::<u64>()) {
        // max 分量 = old_max.max(m)。
        prop_assert_eq!(a.with_max(m).max, a.max.max(m));
    }

    #[test]
    fn with_max_monotone_in_m(m1 in any::<u64>(), m2 in any::<u64>()) {
        // 若 m1 <= m2，则 with_max(m1).max <= with_max(m2).max。
        let a = g(3, 5);
        prop_assume!(m1 <= m2);
        prop_assert!(a.with_max(m1).max <= a.with_max(m2).max);
    }
}

#[test]
fn with_max_idempotent() {
    // 相同 m 重复应用不改变结果。
    let a = g(2, 4);
    assert_eq!(a.with_max(7), a.with_max(7).with_max(7));
    // m <= 当前 max 时不抬高。
    assert_eq!(g(2, 10).with_max(3), g(2, 10));
}

// ───────────────────────────── 4. EffectCost.plus 三元组积 monoid ─────────────────────────────

proptest! {
    #[test]
    fn effect_cost_plus_associative(a in arb_cost(), b in arb_cost(), c in arb_cost()) {
        prop_assert_eq!(a.plus(&b).plus(&c), a.plus(&(b.plus(&c))));
    }

    #[test]
    fn effect_cost_plus_commutative(a in arb_cost(), b in arb_cost()) {
        prop_assert_eq!(a.plus(&b), b.plus(&a));
    }
}

#[test]
fn effect_cost_plus_left_unit() {
    let c = ec(p(2), p(3), p(4));
    assert_eq!(EffectCost::ZERO.plus(&c), c);
}

#[test]
fn effect_cost_plus_right_unit() {
    let c = ec(p(2), p(3), p(4));
    assert_eq!(c.plus(&EffectCost::ZERO), c);
}

// ───────────────────────────── 5. 量纲隔离 ─────────────────────────────

#[test]
fn plus_keeps_dimensions_separate() {
    // read 维度的值不得泄漏进 write/occupy，反之亦然。
    let a = ec(p(10), Grade::ZERO, Grade::ZERO);
    let b = ec(Grade::ZERO, p(20), Grade::ZERO);
    let c = ec(Grade::ZERO, Grade::ZERO, p(30));
    let s = a.plus(&b).plus(&c);
    assert_eq!(s.read, p(10));
    assert_eq!(s.write, p(20));
    assert_eq!(s.occupy, p(30));
}

#[test]
fn occupy_net_peak_only_occupy_dimension() {
    // occupy_net / occupy_peak 只反映 occupy 维，不得跨维求和 read/write。
    let c = ec(p(100), p(200), p(7));
    assert_eq!(c.occupy_net(), 7);
    assert_eq!(c.occupy_peak(), 7);
    // read/write 维的很大值不能污染占用判定。
    assert!(c.occupy_net() < 100);
}

#[test]
fn occupy_net_eq_occupy_peak_current_impl() {
    // 当前实现两方法均返回 occupy.max（见 cost.rs）；记录为 doc 一致性依据。
    let c = ec(Grade::ZERO, Grade::ZERO, g(3, 9));
    assert_eq!(c.occupy_net(), 9);
    assert_eq!(c.occupy_peak(), 9);
}

// ───────────────────────────── 6. CostBudget.exceeded_by ─────────────────────────────

#[test]
fn unbounded_never_exceeded() {
    let b = CostBudget::UNBOUNDED;
    // 任意有限 cost 均不超过（UNBOUNDED 阈值 = u64::MAX）。
    let c = ec(p(u64::MAX / 2), p(u64::MAX / 2), p(u64::MAX / 2));
    assert!(!b.exceeded_by(&c));
    assert!(!b.exceeded_by(&EffectCost::ZERO));
    assert!(!b.exceeded_by(&ec(p(u64::MAX), p(u64::MAX), p(u64::MAX))));
}

#[test]
fn boundary_equal_not_exceeded() {
    // == 阈值不算超。
    let b = CostBudget {
        read: 5,
        write: 5,
        occupy: 5,
    };
    assert!(!b.exceeded_by(&ec(p(5), p(5), p(5))));
}

#[test]
fn only_max_component_triggers() {
    // 仅 max 分量越界即超；min 再大也不算（保守上界语义）。
    let b = CostBudget {
        read: 4,
        write: 4,
        occupy: 4,
    };
    // max 越界 → 超。
    assert!(b.exceeded_by(&ec(g(0, 5), g(0, 0), g(0, 0))));
    // 仅 min 越界、max 在内 → 不超。
    assert!(!b.exceeded_by(&ec(g(5, 3), g(0, 0), g(0, 0))));
}

#[test]
fn per_dimension_exceeds() {
    let b = CostBudget {
        read: 4,
        write: 4,
        occupy: 4,
    };
    assert!(b.exceeded_by(&ec(g(0, 5), g(0, 0), g(0, 0)))); // read 超
    assert!(b.exceeded_by(&ec(g(0, 0), g(0, 5), g(0, 0)))); // write 超
    assert!(b.exceeded_by(&ec(g(0, 0), g(0, 0), g(0, 5)))); // occupy 超
}

#[test]
fn budget_check_not_subadditive() {
    // 反例：两个各自在预算内的 cost，累加后越界。
    // 说明预算检查不可按单 op 判定，必须在累计 EffectCost 上判定（运行时会
    // 对 accrued 整体检查）。这不是缺陷，而是 max-only 保守语义的固有性质。
    let b = CostBudget {
        read: 5,
        write: u64::MAX,
        occupy: u64::MAX,
    };
    let c1 = ec(p(3), Grade::ZERO, Grade::ZERO);
    let c2 = ec(p(3), Grade::ZERO, Grade::ZERO);
    assert!(!b.exceeded_by(&c1));
    assert!(!b.exceeded_by(&c2));
    assert!(b.exceeded_by(&c1.plus(&c2))); // 3+3=6 > 5
}

// ───────────────────────────── 7. DataOp::for_op 静态派生 ─────────────────────────────

/// 逐 op 验证落对维度 + 度量语义（文档 B1："3"= 各原语语义计数）。
#[test]
fn for_op_classification_table() {
    let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let path = PathBuf::from("/x");

    // read 维（无 occupy）：
    assert_eq!(
        EffectCost::for_op(&DataOp::Stat { path: path.clone() }),
        ec(p(1), Grade::ZERO, Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::ReadDir { path: path.clone() }),
        ec(p(1), Grade::ZERO, Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::GetTime),
        ec(p(1), Grade::ZERO, Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::TcpAccept { listener: 1 }),
        ec(p(1), Grade::ZERO, Grade::ZERO)
    );
    // 带长度读：max 绑定 len。
    assert_eq!(
        EffectCost::for_op(&DataOp::Read { fd: 1, len: 0 }),
        ec(p(1), Grade::ZERO, Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::Read { fd: 1, len: 5 }),
        ec(g(1, 5), Grade::ZERO, Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::UdpRecvFrom { fd: 1, len: 5 }),
        ec(g(1, 5), Grade::ZERO, Grade::ZERO)
    );
    // TcpRead 与 Read/UdpRecvFrom 一致：带长度读以字节量为 max。
    assert_eq!(
        EffectCost::for_op(&DataOp::TcpRead { fd: 1, len: 5 }),
        ec(g(1, 5), Grade::ZERO, Grade::ZERO)
    );

    // write 维（无 occupy），带长度用 max：
    assert_eq!(
        EffectCost::for_op(&DataOp::Write {
            fd: 1,
            data: vec![]
        }),
        ec(Grade::ZERO, p(1), Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::Write {
            fd: 1,
            data: vec![0u8; 5]
        }),
        ec(Grade::ZERO, g(1, 5), Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::TcpWrite {
            fd: 1,
            data: vec![0u8; 5]
        }),
        ec(Grade::ZERO, g(1, 5), Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::UdpSendTo {
            fd: 1,
            data: vec![0u8; 5],
            addr
        }),
        ec(Grade::ZERO, g(1, 5), Grade::ZERO)
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::TcpShutdown {
            fd: 1,
            how: std::net::Shutdown::Both
        }),
        ec(Grade::ZERO, p(1), Grade::ZERO)
    );
    // 元数据/游标修改类（write{1}）：
    for op in [
        DataOp::Seek {
            fd: 1,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        DataOp::Truncate {
            path: path.clone(),
            len: 0,
        },
        DataOp::Rename {
            from: path.clone(),
            to: path.clone(),
        },
        DataOp::Mkdir {
            path: path.clone(),
            mode: 0,
        },
        DataOp::Chmod {
            path: path.clone(),
            mode: 0,
        },
        DataOp::Chown {
            path: path.clone(),
            uid: 0,
            gid: 0,
        },
        DataOp::SendFile {
            out: 1,
            input: 2,
            offset: 0,
            len: 0,
        },
    ] {
        assert_eq!(EffectCost::for_op(&op), ec(Grade::ZERO, p(1), Grade::ZERO));
    }

    // 释放/删除类（write{1}，无 occupy）：
    for op in [
        DataOp::Close { fd: 1 },
        DataOp::Dup { fd: 1 },
        DataOp::Dup2 {
            old_fd: 1,
            new_fd: 2,
        },
        DataOp::Rmdir { path: path.clone() },
        DataOp::Unlink { path: path.clone() },
    ] {
        assert_eq!(EffectCost::for_op(&op), ec(Grade::ZERO, p(1), Grade::ZERO));
    }

    // 不可逆投递/信号/锁（write{1}，无 occupy）：
    for op in [
        DataOp::Kill { pid: 1, signal: 9 },
        DataOp::Wait { pid: 1 },
        DataOp::SendSignal { signal: 9, pid: 1 },
        DataOp::MutexLock { id: 1 },
        DataOp::MutexUnlock { id: 1 },
    ] {
        assert_eq!(EffectCost::for_op(&op), ec(Grade::ZERO, p(1), Grade::ZERO));
    }

    // 创建句柄（write{1} + occupy{1}）：
    for op in [
        DataOp::Open {
            path: path.clone(),
            flags: Default::default(),
        },
        DataOp::TcpBind { addr },
        DataOp::TcpConnect { addr },
        DataOp::UdpBind { addr },
        DataOp::PipeOpen {
            flags: Default::default(),
        },
        DataOp::Spawn {
            cmd: Command::new("true"),
        },
    ] {
        assert_eq!(EffectCost::for_op(&op), ec(Grade::ZERO, p(1), p(1)));
    }

    // 内存映射：read + occupy{len} / write + occupy{len}：
    assert_eq!(
        EffectCost::for_op(&DataOp::Mmap {
            path: path.clone(),
            len: 0,
            prot: Default::default()
        }),
        ec(p(1), Grade::ZERO, p(0))
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::Mmap {
            path: path.clone(),
            len: 5,
            prot: Default::default()
        }),
        ec(p(1), Grade::ZERO, p(5))
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::Munmap { addr: 0, len: 0 }),
        ec(Grade::ZERO, p(1), p(0))
    );
    assert_eq!(
        EffectCost::for_op(&DataOp::Munmap { addr: 0, len: 5 }),
        ec(Grade::ZERO, p(1), p(5))
    );
}

#[test]
fn for_op_is_pure_and_deterministic() {
    // 同 op 两次派生一致（无副作用、确定性）。
    let ops = [
        DataOp::Open {
            path: PathBuf::from("/a"),
            flags: Default::default(),
        },
        DataOp::Write {
            fd: 1,
            data: vec![1, 2, 3],
        },
        DataOp::Mmap {
            path: PathBuf::from("/m"),
            len: 42,
            prot: Default::default(),
        },
        DataOp::Read { fd: 1, len: 7 },
    ];
    for op in ops {
        assert_eq!(EffectCost::for_op(&op), EffectCost::for_op(&op));
    }
    // 不同 len 应产生不同 max（度量语义敏感）。
    assert_ne!(
        EffectCost::for_op(&DataOp::Read { fd: 1, len: 1 }),
        EffectCost::for_op(&DataOp::Read { fd: 1, len: 9 })
    );
}

// ───────────────────────────── 8. 幂等塌缩 C1 代数确认 ─────────────────────────────

/// 确认 `for_op` 纯函数 + 运行时累计唯一落点（`UndoStack::add_cost` 仅在
/// `exec_via` 成功路径调用一次，runtime.rs:1520）。本测试在代数/运行时边界上
/// 复验：相同幂等键重试命中缓存 → accrued 累计值不变（不翻倍）。
///
/// 与 cost_audit.rs 的 `idempotent_cache_hit_collapses_cost` 互为补充：那里是
/// 行为层；这里是"累计落点单一性 + 纯函数"的代数层确认。
#[test]
fn idempotent_cache_hit_cost_not_accumulated() {
    use algeff_core::{
        Action, BoxFuture, ResourceRegistry, Runtime, SysError, SyscallExecutor, UndoCapability,
        UndoStack, Value,
    };

    struct RecordingExecutor;
    impl SyscallExecutor for RecordingExecutor {
        fn execute<'a>(
            &'a mut self,
            _op: &'a DataOp,
            _registry: &'a mut ResourceRegistry,
        ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
            Box::pin(async {
                Ok((
                    Value::Unit,
                    UndoCapability::Invertible(Box::pin(async { Ok(()) })),
                ))
            })
        }
    }

    fn open(next: impl FnOnce(Value) -> Action + Send + 'static) -> Action {
        Action::Syscall {
            op: DataOp::Open {
                path: "/x".into(),
                flags: Default::default(),
            },
            resources: Vec::new(),
            next: Box::new(next),
        }
    }
    fn idem(key: &str, inner: Action) -> Action {
        Action::Idempotent {
            key: key.to_string(),
            inner: Box::new(inner),
            next: Box::new(Action::Pure),
        }
    }

    let mut rt = Runtime::new(Box::new(RecordingExecutor));
    let make = || idem("cost:idem:alg", open(Action::Pure));
    rt.run_blocking(make()).unwrap();
    let first = rt.undo_stack().accrued_cost();
    // 第一次执行：Open → write{1}+occupy{1}。
    assert_eq!(first.write.max, 1);
    assert_eq!(first.occupy.max, 1);
    // 重试命中缓存，inner 不执行 → 累计值保持。
    rt.run_blocking(make()).unwrap();
    let second = rt.undo_stack().accrued_cost();
    assert_eq!(
        second.write.max, 1,
        "幂等命中不应重复累计开销（C1 塌缩，代数层确认）"
    );
    assert_eq!(
        second.occupy.max, 1,
        "幂等命中不应重复累计占用（C1 塌缩，代数层确认）"
    );
    // 与空栈零开销可比性（纯计算零成本）。
    assert_eq!(UndoStack::new().accrued_cost(), EffectCost::ZERO);
}

// ───────────────────────────── proptest 策略 ─────────────────────────────

fn arb_grade() -> impl Strategy<Value = Grade> {
    any::<u64>().prop_flat_map(|lo| {
        (Just(lo), any::<u64>()).prop_map(move |(_, hi)| Grade {
            min: lo,
            max: lo.max(hi),
        })
    })
}

fn arb_cost() -> impl Strategy<Value = EffectCost> {
    (arb_grade(), arb_grade(), arb_grade()).prop_map(|(r, w, o)| EffectCost {
        read: r,
        write: w,
        occupy: o,
    })
}
