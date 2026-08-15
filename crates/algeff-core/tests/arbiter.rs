//! A3 批 3：`ResourceArbiter` 动态资源仲裁测试（pdr.md 公理 A7 的工程载体）。
//!
//! 目的：验证动态层「原子占坑 + 失败回滚 + 有限重试」原语的五项语义：
//! a. 全部可占 → 成功且 held；
//! b. 部分冲突 → 原子失败（冲突资源与已占资源都未留下坑 —— 回滚断言）；
//! c. Read-Read 共享、Read-Write 互斥（对齐 pdr.md §9.1 冲突矩阵）；
//! d. 释放后可重占；
//! e. 有限重试循环模拟（固定序列，最多 N 次后成功）不变量。
//!
//! 不依赖 `interpret`（A2），不触碰 src/，仅用公共 API。

use algeff_core::{AccessMode, Resource, ResourceArbiter, ResourceUsage};

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
