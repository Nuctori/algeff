//! 公理属性测试（A6 Verification，pdr.md §19.1 / §19.4）—— `crates/algeff-core/tests/axioms.rs`。
//!
//! 范围：本阶段只测已冻结的静态实现（pdr.md §七 工程映射表）：
//!   - A2 单位元：空资源集 `can_parallel` 恒真；UndoStack 空 `recover` 无副作用；
//!   - A3 交换律/冲突矩阵（pdr.md §9.1）：穷举 4×4 AccessMode ×（同资源/异资源）；
//!   - A3 对称性（A7 静态部分）：proptest 随机 ResourceSet 对；
//!   - A4 资源线性：Write/Own 恰好消费一次，重复 → `InvalidInput`；Read/Append 可重复；
//!   - A6 撤销双态：UndoStack LIFO 逆序执行 + 可观测状态恢复（w;w̄=1）；
//!   - 错误映射：`SysError::from_errno` 往返 + `From<io::Error>`（pdr.md §10）。
//!
//! 注意：`interpret`/`Runtime::run` 仍为 A2 的 `todo!()`（另一分支实现），
//! 本文件禁止调用；阶段 2 执行级公理测试清单见文件末尾注释。

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use algeff_core::error::SysError;
use algeff_core::resource::{
    AccessMode, AppendOnly, Owned, ReadOnly, Resource, ResourceHandle, ResourceInner,
    ResourceRegistry, ResourceSet, ResourceUsage, TypedResource, WriteOnly,
};
use algeff_core::runtime::UndoStack;
use proptest::prelude::*;

const MODES: [AccessMode; 4] = [
    AccessMode::Read,
    AccessMode::Write,
    AccessMode::Append,
    AccessMode::Own,
];

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage { resource: r, mode: m }
}

fn mutex_handle() -> ResourceHandle {
    ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(())))
}

/// 小宇宙资源生成器：提高资源碰撞概率，让 `can_parallel` 冲突分支被充分覆盖。
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

// ── A2 单位元（pdr.md §四 A2）───────────────────────────────────────────

#[test]
fn a2_empty_resource_set_parallel_always() {
    let reg = ResourceRegistry::new();
    assert!(reg.can_parallel(&vec![], &vec![]), "空集 × 空集恒真");
    let w = usage(Resource::Fd(1), AccessMode::Write);
    assert!(reg.can_parallel(&vec![], &vec![w.clone()]), "空集 × 非空恒真");
    assert!(reg.can_parallel(&vec![w], &vec![]), "非空 × 空集恒真");
}

proptest! {
    /// 空资源集是 `can_parallel` 的单位元：与任意 ResourceSet 组合恒为真（不相交 ⇒ 并行）。
    #[test]
    fn a2_empty_set_is_parallel_identity(ref a in arb_resource_set()) {
        let reg = ResourceRegistry::new();
        prop_assert!(reg.can_parallel(&vec![], a));
        prop_assert!(reg.can_parallel(a, &vec![]));
    }
}

#[tokio::test]
async fn a2_empty_undo_stack_recover_noop() {
    let mut stack = UndoStack::new();
    assert!(stack.is_empty());
    assert_eq!(stack.len(), 0);
    stack.recover().await; // 空栈 recover 直接返回，无副作用
    assert!(stack.is_empty());
    assert_eq!(stack.len(), 0);
}

// ── A3 交换律 / 冲突矩阵（pdr.md §9.1，决策 D6）───────────────────────

#[test]
fn a3_conflict_matrix_exhaustive_same_resource() {
    let reg = ResourceRegistry::new();
    let r = Resource::Fd(1);
    for &m1 in &MODES {
        for &m2 in &MODES {
            let a = vec![usage(r.clone(), m1)];
            let b = vec![usage(r.clone(), m2)];
            // §9.1 表：同资源下仅 Read×Read 并行（Append×Append 默认串行，决策 D6）
            let expected = matches!((m1, m2), (AccessMode::Read, AccessMode::Read));
            assert_eq!(
                reg.can_parallel(&a, &b),
                expected,
                "同资源 {m1:?}×{m2:?} 期望并行={expected}"
            );
            // A3 交换律：can_parallel 对称
            assert_eq!(reg.can_parallel(&b, &a), expected);
        }
    }
}

#[test]
fn a3_conflict_matrix_exhaustive_disjoint_resources() {
    let reg = ResourceRegistry::new();
    let r1 = Resource::Fd(1);
    let r2 = Resource::Fd(2);
    for &m1 in &MODES {
        for &m2 in &MODES {
            let a = vec![usage(r1.clone(), m1)];
            let b = vec![usage(r2.clone(), m2)];
            assert!(
                reg.can_parallel(&a, &b),
                "异资源 {m1:?}×{m2:?} 应恒并行（资源不相交 ⇒ 可并行）"
            );
        }
    }
}

#[test]
fn a3_append_append_requires_opt_in() {
    let reg = ResourceRegistry::new();
    let r = Resource::Fd(1);
    let a = vec![usage(r.clone(), AccessMode::Append)];
    let b = vec![usage(r.clone(), AccessMode::Append)];
    assert!(!reg.can_parallel(&a, &b), "默认 Append∥Append 串行（决策 D6）");
    assert!(reg.can_parallel_with(&a, &b, true), "显式声明顺序无关 → 并行");
    // opt-in 只放宽 Append∥Append，其余冲突仍然拒绝
    let w = vec![usage(r, AccessMode::Write)];
    assert!(!reg.can_parallel_with(&a, &w, true));
}

#[test]
fn a3_conflict_matrix_with_append_opt_in() {
    let reg = ResourceRegistry::new();
    let r = Resource::Fd(1);
    for &m1 in &MODES {
        for &m2 in &MODES {
            let a = vec![usage(r.clone(), m1)];
            let b = vec![usage(r.clone(), m2)];
            // opt-in 后：Read×Read 与 Append×Append 并行，其余仍串行
            let expected = matches!(
                (m1, m2),
                (AccessMode::Read, AccessMode::Read)
                    | (AccessMode::Append, AccessMode::Append)
            );
            assert_eq!(reg.can_parallel_with(&a, &b, true), expected);
        }
    }
}

proptest! {
    /// A3 交换律对称性（A7 静态部分）：随机 ResourceSet 对下 `can_parallel`
    /// 与 `can_parallel_with`（opt-in 变体）均交换不变。
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

// ── A4 资源线性（pdr.md §四 A4）────────────────────────────────────────

#[test]
fn a4_write_duplicate_rejected() {
    let mut reg = ResourceRegistry::new();
    let u = usage(Resource::Fd(1), AccessMode::Write);
    assert!(reg.check_linear(&u).is_ok(), "首次 Write 消费应 Ok");
    assert_eq!(
        reg.check_linear(&u),
        Err(SysError::InvalidInput),
        "重复 Write 应拒绝（恰好消费一次）"
    );
}

#[test]
fn a4_read_repeatable() {
    let mut reg = ResourceRegistry::new();
    let u = usage(Resource::Fd(1), AccessMode::Read);
    assert!(reg.check_linear(&u).is_ok());
    assert!(reg.check_linear(&u).is_ok(), "Read 不消费，可重复");
}

#[test]
fn a4_own_is_linear_too() {
    let mut reg = ResourceRegistry::new();
    let u = usage(Resource::Fd(1), AccessMode::Own);
    assert!(reg.check_linear(&u).is_ok());
    assert_eq!(reg.check_linear(&u), Err(SysError::InvalidInput));
}

proptest! {
    /// 异资源 Write 各自恰好消费一次均 Ok（随机序列）；去重后同一资源重复 Write 恒拒绝。
    #[test]
    fn a4_disjoint_writes_linear_sequence(
        ref resources in proptest::collection::vec(arb_resource(), 0..8),
    ) {
        let mut reg = ResourceRegistry::new();
        let mut seen: HashSet<Resource> = HashSet::new();
        for r in resources {
            if !seen.insert(r.clone()) {
                continue; // 同一资源的重复消费由 a4_write_duplicate_rejected 覆盖
            }
            let u = usage(r.clone(), AccessMode::Write);
            prop_assert!(reg.check_linear(&u).is_ok(), "异资源首次 Write 应 Ok: {:?}", r);
        }
    }

    /// 随机混合序列：Read/Append 可重复；Write/Own 恰好一次，重复 → InvalidInput。
    #[test]
    fn a4_random_read_write_sequence(
        ref seq in proptest::collection::vec((arb_resource(), arb_mode()), 0..16),
    ) {
        let mut reg = ResourceRegistry::new();
        let mut written: HashSet<Resource> = HashSet::new();
        for (r, m) in seq {
            let u = usage(r.clone(), *m);
            match m {
                AccessMode::Read | AccessMode::Append => {
                    prop_assert!(reg.check_linear(&u).is_ok(), "Read/Append 可重复: {:?}", u);
                }
                AccessMode::Write | AccessMode::Own => {
                    if written.contains(r) {
                        prop_assert_eq!(
                            reg.check_linear(&u),
                            Err(SysError::InvalidInput),
                            "重复消费应拒绝: {:?}",
                            u
                        );
                    } else {
                        prop_assert!(reg.check_linear(&u).is_ok(), "首次消费应 Ok: {:?}", u);
                        written.insert(r.clone());
                    }
                }
            }
        }
    }
}

// ── A6 撤销双态（pdr.md §四 A6 / §5.1 / §11：w;w̄=1）───────────────────

#[tokio::test]
async fn a6_undo_lifo_order() {
    // 两个 undo 各向日志推入自己的标记；recover 后执行顺序必须为逆序（LIFO）
    let log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let mut stack = UndoStack::new();
    for id in [1u8, 2u8] {
        let log_ref = log.clone();
        stack.push(Box::pin(async move {
            log_ref.lock().unwrap().push(id);
        }));
    }
    assert_eq!(stack.len(), 2);
    stack.recover().await;
    assert_eq!(*log.lock().unwrap(), vec![2u8, 1u8], "后压入的先执行（LIFO 逆序）");
    assert!(stack.is_empty(), "recover 后栈清空");
}

#[tokio::test]
async fn a6_undo_restores_observable_state() {
    // 模拟 w;w̄=1：状态 +2（写副作用），逆操作 -2（撤销），recover 后回到原值
    let state = Arc::new(AtomicI64::new(0));
    state.fetch_add(2, Ordering::SeqCst);
    let undo_state = state.clone();
    let mut stack = UndoStack::new();
    stack.push(Box::pin(async move {
        undo_state.fetch_sub(2, Ordering::SeqCst);
    }));
    assert_eq!(state.load(Ordering::SeqCst), 2);
    stack.recover().await;
    assert_eq!(state.load(Ordering::SeqCst), 0, "状态应恢复为初始值（w;w̄=1）");
}

#[tokio::test]
async fn a6_undo_multiple_restores_full_state() {
    // 多步写入（+1,+10）→ 逆序撤销 → 恢复 0；且每个逆操作都被执行
    let state = Arc::new(AtomicI64::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let mut stack = UndoStack::new();
    for delta in [1i64, 10] {
        state.fetch_add(delta, Ordering::SeqCst);
        let undo_state = state.clone();
        let undo_executed = executed.clone();
        stack.push(Box::pin(async move {
            undo_state.fetch_sub(delta, Ordering::SeqCst);
            undo_executed.fetch_add(1, Ordering::SeqCst);
        }));
    }
    assert_eq!(state.load(Ordering::SeqCst), 11);
    stack.recover().await;
    assert_eq!(state.load(Ordering::SeqCst), 0);
    assert_eq!(executed.load(Ordering::SeqCst), 2);
}

// ── 错误映射（pdr.md §10）──────────────────────────────────────────────

#[test]
fn error_from_errno_roundtrip_all_variants() {
    let known: [(i32, SysError); 14] = [
        (2, SysError::NotFound),
        (13, SysError::PermissionDenied),
        (11, SysError::WouldBlock),
        (4, SysError::Interrupted),
        (110, SysError::TimedOut),
        (104, SysError::ConnectionReset),
        (111, SysError::ConnectionRefused),
        (32, SysError::BrokenPipe),
        (28, SysError::StorageFull),
        (22, SysError::InvalidInput),
        (17, SysError::AlreadyExists),
        (20, SysError::NotADirectory),
        (21, SysError::IsADirectory),
        (18, SysError::CrossDevice),
    ];
    for (errno, expected) in known {
        assert_eq!(SysError::from_errno(errno), expected, "errno {errno} 映射");
        assert_eq!(expected.code(), errno, "code 往返 errno {errno}");
    }
    // 未知 errno → Other 兜底（保留原始码，不参与穷尽性检查）
    assert_eq!(SysError::from_errno(0), SysError::Other(0));
    assert_eq!(SysError::from_errno(999), SysError::Other(999));
    assert_eq!(SysError::Other(7).code(), 7);
}

proptest! {
    /// from_errno ∘ code = id（任意 errno，含 Other 兜底）
    #[test]
    fn error_errno_roundtrip_proptest(errno in any::<i32>()) {
        let e = SysError::from_errno(errno);
        prop_assert_eq!(e.code(), errno);
        prop_assert_eq!(SysError::from_errno(e.code()), e);
    }
}

#[test]
fn error_from_io_error() {
    // 有原始 errno → 映射为对应变体
    let e = std::io::Error::from_raw_os_error(13);
    assert_eq!(SysError::from(e), SysError::PermissionDenied);
    // 无原始 errno → Other(0)
    let e2 = std::io::Error::other("no errno");
    assert_eq!(SysError::from(e2), SysError::Other(0));
}

// ── typestate 与注册表（contracts.md §2 冻结类型）──────────────────────

#[test]
fn typestate_usage_mode_matches() {
    let u = TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(1)).into_usage();
    assert_eq!(u.mode, AccessMode::Read);
    let u = TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(1)).into_usage();
    assert_eq!(u.mode, AccessMode::Write);
    let u = TypedResource::<AppendOnly>::new_append(ResourceInner::Fd(1)).into_usage();
    assert_eq!(u.mode, AccessMode::Append);
    let u = TypedResource::<Owned>::new_owned(ResourceInner::Fd(1)).into_usage();
    assert_eq!(u.mode, AccessMode::Own);
}

#[test]
fn typestate_transitions_are_valid() {
    let r = TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(1));
    let w: TypedResource<WriteOnly> = r.into_write();
    let o: TypedResource<Owned> = w.into_owned();
    assert_eq!(o.into_usage().mode, AccessMode::Own);
    // ReadOnly → AppendOnly；WriteOnly 不可直接降级为 Append（构造器决定合法迁移）
    let r2 = TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(2));
    let a: TypedResource<AppendOnly> = r2.into_append();
    assert_eq!(a.into_usage().mode, AccessMode::Append);
}

#[test]
fn registry_fd_monotonic_unique_never_reused() {
    let mut reg = ResourceRegistry::new();
    let f1 = reg.allocate(mutex_handle());
    let f2 = reg.allocate(mutex_handle());
    assert!(f1 < f2, "Fd 单调递增（决策 D1）");
    reg.remove(f1);
    assert!(reg.lookup(f1).is_none(), "remove 后 lookup 不可见");
    let f3 = reg.allocate(mutex_handle());
    assert!(f3 > f2, "Fd 永不复用（决策 D1）");
}

// ── 阶段 2 执行级公理测试清单（interpret / Runtime::run 就绪后补充）────
//
// 本文件只覆盖已冻结静态实现；A2 的 interpret 合并后需追加执行级属性测试：
//   1. A2 执行级单位元：run(Pure(v)) == Ok(v)；run(Pure(()); a) 与 run(a) 值等价；
//   2. A1 结合律：(a;b);c 与 a;(b;c) 对任意可执行蓝图结果一致（属性测试）；
//   3. A3 动态冲突调度：Fork 冲突资源 → 串行降级，结果与顺序执行一致；
//   4. A4 动态线性：interpret 内对 Write/Own 调 check_linear，重复消费报 InvalidInput；
//   5. A5 分支隔离：Choose 左分支 Write 不影响右分支 Read（COW/隔离）；
//   6. A6 执行级撤销往返：algeff-std TokioExecutor Full 策略写文件 → recover 后内容还原；
//   7. A7 动态无死锁：Runtime::run 多任务并发调度压力测试（loom 或 tokio 并发）。
