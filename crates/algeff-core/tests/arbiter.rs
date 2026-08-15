//! A3 批 3：`ResourceArbiter` 动态资源仲裁测试（pdr.md 公理 A7 的工程载体）。
//!
//! 目的：验证动态层「原子占坑 + 失败回滚 + 有限重试」原语的五项语义：
//! a. 全部可占 → 成功且 held；
//! b. 部分冲突 → 原子失败（冲突资源与已占资源都未留下坑 —— 回滚断言）；
//! c. Read-Read 共享、Read-Write 互斥（对齐 pdr.md §9.1 冲突矩阵）；
//! d. 释放后可重占；
//! e. 有限重试循环模拟（固定序列，最多 N 次后成功）不变量。
//!
//! 批 4 增补（arbiter 属性测试强化，A7 不变量）：
//! f. proptest：随机 (resource, mode) 集合 × 随机 claim/release 交错序列 →
//!    单调不减 / 失败原子性快照 / 无 panic·无泄漏（`is_clean`）三条不变量；
//! g. 互斥矩阵穷举（同资源 4×4 模式对，对齐 §9.1：Read-Read 可共享，其余互斥）；
//! h. `is_clean` 全生命周期测试（泄漏检测原语）。
//!
//! 不依赖 `interpret`（A2），不触碰 src/，仅用公共 API。

use std::collections::HashMap;

use algeff_core::{AccessMode, Resource, ResourceArbiter, ResourceUsage};
use proptest::prelude::*;

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

// a. 全部可占 → 成功且 held
#[test]
fn all_claimable_succeeds_and_held() {
    let mut arb = ResourceArbiter::new();
    let set = vec![
        usage(Resource::Fd(1), AccessMode::Read),
        usage(Resource::Fd(2), AccessMode::Write),
        usage(Resource::Path("/tmp/x".into()), AccessMode::Own),
    ];
    assert!(arb.try_claim(&set));
    for u in &set {
        assert!(arb.held(&u.resource), "占坑成功后 {:?} 应 held", u.resource);
    }
    // 未占坑资源不 held
    assert!(!arb.held(&Resource::Fd(99)));
}

// b. 部分冲突 → 原子失败（回滚断言：已占坑与未占坑资源都无残留）
#[test]
fn partial_conflict_atomic_failure_no_residue() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);
    let s = Resource::Fd(2);
    assert!(arb.try_claim(&vec![usage(r.clone(), AccessMode::Write)]));

    // set 内先出现的 Write(s) 本可占坑，但 Read(r) 与已占 Write(r) 冲突 →
    // 整体失败：s 不得留下坑（原子回滚），r 的坑保持原样。
    assert!(!arb.try_claim(&vec![
        usage(r.clone(), AccessMode::Read),
        usage(s.clone(), AccessMode::Write),
    ]));
    assert!(!arb.held(&s), "冲突失败后 s 不应留下坑（整体回滚）");
    assert!(arb.held(&r), "冲突失败后既有占坑 r 不受影响");

    // set 内部自冲突同样整体回滚：Write(s) 先成功、Write(t) 冲突，
    // 失败后 s 必须无残留。
    let t = Resource::Fd(3);
    assert!(arb.try_claim(&vec![usage(t.clone(), AccessMode::Write)]));
    assert!(!arb.try_claim(&vec![
        usage(s.clone(), AccessMode::Write),
        usage(t.clone(), AccessMode::Write),
    ]));
    assert!(!arb.held(&s), "set 内自冲突失败后 s 无残留");
    assert!(arb.held(&t), "set 内自冲突失败后既有占坑 t 不受影响");
}

// c. Read-Read 共享、Read-Write 互斥（pdr.md §9.1 矩阵）
#[test]
fn read_read_shared_read_write_exclusive() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);
    let read = vec![usage(r.clone(), AccessMode::Read)];
    let write = vec![usage(r.clone(), AccessMode::Write)];

    // Read-Read 共享：两次 Read 占坑都成功
    assert!(arb.try_claim(&read));
    assert!(arb.try_claim(&read));
    assert!(arb.held(&r));
    // 已有 Read 时 Write 被拒（互斥）
    assert!(!arb.try_claim(&write));
    // 释放一个 Read 后仍 held（还剩一个占坑）
    arb.release(&read);
    assert!(arb.held(&r));
    arb.release(&read);
    assert!(!arb.held(&r));

    // Read-Write 互斥（反向）：已有 Write 时 Read/Write 均被拒
    assert!(arb.try_claim(&write));
    assert!(arb.held(&r));
    assert!(!arb.try_claim(&read));
    assert!(!arb.try_claim(&write));
    arb.release(&write);
    assert!(!arb.held(&r));
    // 释放后可重占（衔接 d）
    assert!(arb.try_claim(&read));
    assert!(arb.held(&r));
}

// d. 释放后可重占（Write 与 Read 各验一次；release 幂等）
#[test]
fn release_allows_reclaim() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);

    let write = vec![usage(r.clone(), AccessMode::Write)];
    assert!(arb.try_claim(&write));
    assert!(arb.held(&r));
    arb.release(&write);
    assert!(!arb.held(&r));
    assert!(arb.try_claim(&write), "释放后可重占 Write");
    arb.release(&write);

    let read = vec![usage(r.clone(), AccessMode::Read)];
    assert!(arb.try_claim(&read));
    arb.release(&read);
    assert!(!arb.held(&r));
    // release 幂等：未占坑时释放无副作用、不 panic
    arb.release(&read);
    assert!(!arb.held(&r));
}

// e. 有限重试循环模拟（A7「失败回滚 + 有限重试」）：固定序列下最多 N 次后成功，
//    且每次失败后无残留坑（原子回滚不变量）。仲裁表语义：`claims` 记录所有
//    当前占坑者，重试目标与表中冲突即失败，直到持锁方释放。
#[test]
fn finite_retry_eventually_succeeds() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);
    let s = Resource::Fd(2);
    // 模拟「锁被另一任务持有」：仲裁表上有 Write(r) 占坑
    let held_lock = vec![usage(r.clone(), AccessMode::Write)];
    assert!(arb.try_claim(&held_lock));

    // 重试目标：Read(r)（与持锁冲突）+ Read(s)（无冲突，用于验证回滚无残留）
    let claim = vec![
        usage(r.clone(), AccessMode::Read),
        usage(s.clone(), AccessMode::Read),
    ];

    const MAX_RETRIES: usize = 5;
    let mut attempts = 0usize;
    let mut success = false;
    for _ in 0..MAX_RETRIES {
        attempts += 1;
        if arb.try_claim(&claim) {
            success = true;
            break;
        }
        // 不变量：失败后无残留坑（原子回滚）——s 本可占坑但被整体回滚
        assert!(!arb.held(&s), "第 {attempts} 次失败后 s 不应残留占坑");
        // 既有占坑（持锁方）不受重试影响
        assert!(arb.held(&r));
        // 固定序列：第 3 次重试前持锁方释放（模拟互斥让出）
        if attempts == 3 {
            arb.release(&held_lock);
        }
    }
    assert!(success, "固定序列下应在前几次尝试后成功");
    assert_eq!(attempts, 4, "第 3 次尝试后持锁方释放，第 4 次应成功");
    assert!(arb.held(&r));
    assert!(arb.held(&s));
    // 成功后释放，状态复位
    arb.release(&claim);
    assert!(!arb.held(&s));
    assert!(!arb.held(&r));
}

// ── 批 4：arbiter 属性测试强化（A7 不变量）──────────────────────────────

/// 随机操作：claim（原子占坑尝试）或 release（幂等释放）。
#[derive(Debug, Clone)]
enum ArbOp {
    Claim(Vec<ResourceUsage>),
    Release(Vec<ResourceUsage>),
}

/// 与 `arb_resource` 支撑空间一致的全部资源池（Fd 0..8、Pid 0..4、Signal、Foreign 0..4）。
/// 断言只用池内资源即可覆盖所有可能被占坑的键。
fn arbiter_pool() -> Vec<Resource> {
    let mut v = Vec::new();
    for i in 0..8u64 {
        v.push(Resource::Fd(i));
    }
    for i in 0..4u32 {
        v.push(Resource::Pid(i));
    }
    v.push(Resource::Signal);
    for i in 0..4u64 {
        v.push(Resource::Foreign(i));
    }
    v
}

fn arb_resource() -> impl Strategy<Value = Resource> {
    prop_oneof![
        (0u64..8).prop_map(Resource::Fd),
        (0u32..4).prop_map(Resource::Pid),
        Just(Resource::Signal),
        (0u64..4).prop_map(Resource::Foreign),
    ]
}

fn arb_usage() -> impl Strategy<Value = ResourceUsage> {
    let mode = (0u8..4).prop_map(|m| match m {
        0 => AccessMode::Read,
        1 => AccessMode::Write,
        2 => AccessMode::Append,
        _ => AccessMode::Own,
    });
    (arb_resource(), mode).prop_map(|(resource, mode)| ResourceUsage { resource, mode })
}

fn arb_set() -> impl Strategy<Value = Vec<ResourceUsage>> {
    prop::collection::vec(arb_usage(), 0..=3)
}

fn arb_op() -> impl Strategy<Value = ArbOp> {
    prop_oneof![
        arb_set().prop_map(ArbOp::Claim),
        arb_set().prop_map(ArbOp::Release),
    ]
}

/// 参照模型：与实现同编码（Read 计数累加 / `usize::MAX` 独占标记）。
/// 作为独立 oracle：判定 claim 成败、追踪精确计数，供最终泄漏检测使用。
const EXCLUSIVE: usize = usize::MAX;

fn model_try_claim(model: &mut HashMap<Resource, usize>, set: &[ResourceUsage]) -> bool {
    let mut trial = model.clone();
    for u in set {
        let count = trial.entry(u.resource.clone()).or_insert(0);
        match u.mode {
            AccessMode::Read => {
                if *count == EXCLUSIVE {
                    return false;
                }
                *count += 1;
            }
            AccessMode::Write | AccessMode::Own | AccessMode::Append => {
                if *count != 0 {
                    return false;
                }
                *count = EXCLUSIVE;
            }
        }
    }
    *model = trial;
    true
}

fn model_release(model: &mut HashMap<Resource, usize>, set: &[ResourceUsage]) {
    for u in set {
        match model.get(&u.resource).copied() {
            Some(EXCLUSIVE) => {
                model.remove(&u.resource);
            }
            Some(n) if n > 0 => {
                if n == 1 {
                    model.remove(&u.resource);
                } else {
                    model.insert(u.resource.clone(), n - 1);
                }
            }
            _ => {}
        }
    }
}

/// 布尔级 held 快照：对资源池内每个资源断言 held 与否。
fn arbiter_bool_state(arb: &ResourceArbiter) -> Vec<bool> {
    arbiter_pool().iter().map(|r| arb.held(r)).collect()
}

// f. proptest：随机 (resource, mode) 集合 × 随机 claim/release 交错序列 →
//    A7 三条不变量：
//    1. 单调不减：`try_claim` 成功 → held 集 = 本次集合 ∪ 之前持有（直到 release）；
//    2. 原子性快照：`try_claim` 失败 → 状态与调用前完全一致（无部分占坑残留）；
//    3. 无 panic / 无泄漏：随机序列全程不 panic，按模型释放全部后 `is_clean()`。
//    另：每步与独立参照模型交叉校验布尔 held 状态一致。
proptest! {
    #![proptest_config(ProptestConfig {
        // 集成测试目录无 lib.rs/main.rs，禁用失败用例持久化避免 SourceParallel 噪音
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn random_claim_release_keeps_invariants(
        ops in prop::collection::vec(arb_op(), 0..=60),
    ) {
        let mut arb = ResourceArbiter::new();
        let mut model: HashMap<Resource, usize> = HashMap::new();
        let pool = arbiter_pool();

        for op in ops {
            match op {
                ArbOp::Claim(set) => {
                    let before = arbiter_bool_state(&arb);
                    let ok = arb.try_claim(&set);
                    let after = arbiter_bool_state(&arb);
                    if ok {
                        // 单调不减：本次集合全部 held，且此前 held 的资源保持 held
                        for u in &set {
                            assert!(
                                arb.held(&u.resource),
                                "claim 成功后 {:?} 必须 held",
                                u
                            );
                        }
                        for (i, r) in pool.iter().enumerate() {
                            assert!(
                                !before[i] || after[i],
                                "claim 成功不应释放任何资源（单调不减）：{:?} 成功前 held={}，成功后 held={}",
                                r,
                                before[i],
                                after[i]
                            );
                        }
                        assert!(model_try_claim(&mut model, &set), "模型判定应一致：成功");
                    } else {
                        // 原子性快照断言：失败后状态与调用前完全一致（整体回滚）
                        assert_eq!(
                            before,
                            after,
                            "try_claim 失败后状态必须与调用前完全一致（原子回滚）"
                        );
                        assert!(
                            !model_try_claim(&mut model, &set),
                            "模型判定应一致：失败"
                        );
                    }
                    // 交叉校验：arbiter 布尔状态 == 模型布尔状态
                    for r in &pool {
                        assert_eq!(
                            arb.held(r),
                            model.contains_key(r),
                            "claim 后 arbiter 与模型不一致：{:?}",
                            r
                        );
                    }
                }
                ArbOp::Release(set) => {
                    let before = arbiter_bool_state(&arb);
                    arb.release(&set);
                    let after = arbiter_bool_state(&arb);
                    // release 只释放、不新增占坑：此前未 held 的资源不得变为 held
                    for (i, r) in pool.iter().enumerate() {
                        assert!(
                            before[i] || !after[i],
                            "release 不应新增占坑：{:?} 释放前 held={}，释放后 held={}",
                            r,
                            before[i],
                            after[i]
                        );
                    }
                    model_release(&mut model, &set);
                    for r in &pool {
                        assert_eq!(
                            arb.held(r),
                            model.contains_key(r),
                            "release 后 arbiter 与模型不一致：{:?}",
                            r
                        );
                    }
                }
            }
        }

        // 泄漏检测：按参照模型释放全部占坑，arbiter 必须干净
        for (r, n) in &model {
            let set: Vec<ResourceUsage> = match *n {
                EXCLUSIVE => vec![ResourceUsage {
                    resource: r.clone(),
                    mode: AccessMode::Write,
                }],
                k => (0..k)
                    .map(|_| ResourceUsage {
                        resource: r.clone(),
                        mode: AccessMode::Read,
                    })
                    .collect(),
            };
            arb.release(&set);
        }
        model.clear(); // 模型记录的占坑已全部从 arbiter 释放，同步清空
        assert!(arb.is_clean(), "全部释放后 claims 表必须为空（无泄漏）");
        assert!(model.is_empty(), "参照模型应同步为空");
        assert!(!arb.held(&Resource::Fd(999)), "池外资源不应被占坑");
    }
}

// g. 互斥矩阵穷举（arbiter 动态层，对齐 pdr.md §9.1）：同资源下第一方任意模式
//    占坑成功；第二方仅 (Read, Read) 可共享，其余（Read×Write、Write×Write、
//    Own×任意、Append×Append 保守）一律互斥失败。
#[test]
fn arbiter_mutex_matrix_exhaustive_4x4() {
    let modes = [
        AccessMode::Read,
        AccessMode::Write,
        AccessMode::Append,
        AccessMode::Own,
    ];
    let r = Resource::Fd(1);
    for &m1 in &modes {
        for &m2 in &modes {
            let mut arb = ResourceArbiter::new();
            let first = vec![usage(r.clone(), m1)];
            let second = vec![usage(r.clone(), m2)];
            assert!(arb.try_claim(&first), "首占 {m1:?} 应成功");
            let expected = matches!((m1, m2), (AccessMode::Read, AccessMode::Read));
            assert_eq!(
                arb.try_claim(&second),
                expected,
                "同资源 {m1:?} 之后 {m2:?}：Read-Read 可共享，其余互斥"
            );
            // 释放全部占坑后状态复位（无残留）
            arb.release(&first);
            if expected {
                // Read-Read：还剩一个 Read 占坑，需再释放 second
                arb.release(&second);
            }
            assert!(!arb.held(&r));
            assert!(arb.is_clean(), "释放全部后仲裁表应干净");
        }
    }
}

// h. is_clean：泄漏检测原语——初始干净、持有时不干净、失败 claim 不改干净度、
//    全部释放后恢复干净。
#[test]
fn is_clean_tracks_full_lifecycle() {
    let mut arb = ResourceArbiter::new();
    assert!(arb.is_clean(), "新建仲裁器应干净");

    let r = Resource::Fd(1);
    let read = vec![usage(r.clone(), AccessMode::Read)];
    let write = vec![usage(Resource::Fd(2), AccessMode::Write)];
    assert!(arb.try_claim(&read));
    assert!(!arb.is_clean(), "持有占坑时不应干净");
    assert!(arb.try_claim(&write));
    assert!(!arb.is_clean());

    // 失败的 claim 不改变干净度（原子回滚，不产生新坑）
    assert!(!arb.try_claim(&vec![usage(r.clone(), AccessMode::Write)]));
    assert!(!arb.is_clean());

    arb.release(&read);
    assert!(!arb.is_clean(), "Read 坑未释放完前仍不干净");
    arb.release(&write);
    assert!(arb.is_clean(), "全部释放后应恢复干净（无泄漏）");
}
