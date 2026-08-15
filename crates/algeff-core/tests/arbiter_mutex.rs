//! A3 批 7：`ResourceArbiter` × `MutexLock` 语义组合测试
//! （spec/resource-notes.md §2 强制记录的 core 侧验证，G4 残余 R-1 待办①配套）。
//!
//! 背景（spec/g4-closure.md §4 R-1）：Fork 分支内使用 `DataOp::MutexLock { id }` 的
//! 蓝图**必须**在资源声明中包含对应 `Resource::Fd(id)`（或等价声明），否则静态冲突
//! 检测不可见 → D17 并行 Fork 路径下两分支在共享执行器上对同一互斥锁阻塞
//! `lock_owned`，存在死锁可达窗口。本文件在 core 侧组合验证动态层原语
//! `ResourceArbiter` 与物理互斥载体 `tokio::sync::Mutex`
//! （`ResourceHandle::Mutex` / `DataOp::MutexLock { id }` 的执行语义）的对应关系：
//!
//! a. `claim_then_release_cycle`：try_claim(Write) → held → release → 可重占；
//!    幂等 release 二次调用无害（is_clean 断言）——MutexLock→MutexUnlock→
//!    再次 MutexLock 的占坑生命周期；
//! b. `exclusive_vs_shared_mutex_semantics`：Read claim 可共享（两个任务各 claim
//!    Read 同一资源均成功）vs Write claim 互斥——与 `tokio::sync::Mutex` 的 lock
//!    语义对应：一个任务持有 tokio Mutex（物理互斥）+ arbiter Write 占坑（逻辑
//!    互斥）时，另一任务 claim Write 失败（返回 false = WouldBlock）；释放后成功；
//! c. `retry_upper_bound`：有限重试循环（≤8 次）在竞争者持续持有时终止于 WouldBlock
//!    语义（返回 bool false）且无残留（释放竞争者占坑后 is_clean true）——A5 批 5
//!    将 `op_mutex_lock` 改为 arbiter try_claim + 有限重试后的缓解路径的占坑侧预演。
//!
//! 工程约束：纯 core 无 IO（`tokio::sync::Mutex` 为内存原语，非文件/网络）；仅用
//! 公共 API（lib.rs re-export）；不依赖 interpret（A2）；不触碰 src/。

use std::sync::Arc;

use algeff_core::{AccessMode, Resource, ResourceArbiter, ResourceUsage};

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

// a. 占坑-释放完整周期：try_claim(Write) → held → release → 可重占；幂等 release
//    二次调用无害（is_clean 断言）——对应 MutexLock→MutexUnlock→再次 MutexLock。
#[test]
fn claim_then_release_cycle() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);
    let write = vec![usage(r.clone(), AccessMode::Write)];

    // MutexLock 语义：arbiter 占坑成功后才进入临界区
    assert!(arb.try_claim(&write), "try_claim(Write) 应成功");
    assert!(arb.held(&r), "占坑成功后资源 held");

    // 释放：MutexUnlock / undo（recover 释放锁）
    arb.release(&write);
    assert!(!arb.held(&r), "释放后资源不再 held");
    assert!(arb.is_clean(), "单次完整占坑-释放周期后仲裁表干净");

    // 释放后可重占（再次 MutexLock 同一资源）
    assert!(arb.try_claim(&write), "释放后同一资源可再次占坑");
    arb.release(&write);

    // 幂等 release：未占坑时二次释放无副作用、不 panic
    arb.release(&write);
    assert!(!arb.held(&r), "幂等二次释放后资源仍不 held");
    assert!(
        arb.is_clean(),
        "幂等二次释放后仲裁表仍干净（无残留/无计数 underflow）"
    );
}

// b. Read 共享 vs Write 独占 × tokio::sync::Mutex 语义对应：
//    - 两个任务各 claim Read 同一资源均成功（Read 计数累加，静态层 Read×Read 并行
//      在动态层的对应）；
//    - 一个任务持有 tokio Mutex（物理互斥）+ arbiter Write 占坑（逻辑互斥）时，
//      另一任务 claim Write 失败（bool false = WouldBlock 语义）；释放后成功。
#[tokio::test]
async fn exclusive_vs_shared_mutex_semantics() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);
    let read = vec![usage(r.clone(), AccessMode::Read)];
    let write = vec![usage(r.clone(), AccessMode::Write)];

    // ── Read 可共享 ──
    assert!(arb.try_claim(&read), "任务 1 claim Read 成功");
    assert!(arb.try_claim(&read), "任务 2 共享 claim 同一资源 Read 成功");
    assert!(arb.held(&r));
    arb.release(&read);
    assert!(
        arb.held(&r),
        "仅释放任务 1 的 Read 后仍 held（任务 2 的坑还在）"
    );
    arb.release(&read);
    assert!(!arb.held(&r), "两个 Read 全部释放后不再 held");

    // ── Write 独占 × tokio::sync::Mutex 物理互斥 ──
    // MutexLock 协议：逻辑层先占坑（try_claim），成功后再取物理锁。
    let m = Arc::new(tokio::sync::Mutex::new(())); // 模拟 ResourceHandle::Mutex（Fd(1)）
    assert!(arb.try_claim(&write), "任务 A 占坑 Write 成功");
    let _guard = m.lock().await; // 任务 A 持有物理互斥锁（进入临界区）

    // 任务 B：逻辑层 claim Write 失败（与 A 占坑冲突 → false = WouldBlock）；
    // 物理层同样不可获得（try_lock 失败）——两层语义一致。
    assert!(
        !arb.try_claim(&write),
        "A 持锁期间 B claim Write 失败（WouldBlock 语义）"
    );
    assert!(m.try_lock().is_err(), "A 持锁期间 B 物理 try_lock 失败");

    // 任务 A 释放：undo（recover 路径）释放物理锁 + 释放 arbiter 占坑
    drop(_guard);
    arb.release(&write);
    assert!(!arb.held(&r), "A 释放后资源不再 held");

    // 释放后任务 B 可成功占坑（对应有限重试窗口内竞争者让出后的成功路径）
    assert!(arb.try_claim(&write), "A 释放后 B claim Write 成功");
    assert!(arb.held(&r));
    arb.release(&write);
    assert!(arb.is_clean(), "全部释放后仲裁表干净（无泄漏）");
}

// c. 有限重试上界：竞争者持续持有时，重试循环（≤8 次）终止于 WouldBlock 语义
//    （try_claim 返回 bool false），且重试无残留（释放竞争者占坑后 is_clean true）。
//    ——A5 批 5 接入 arbiter 后 op_mutex_lock「try_claim + 有限重试 → 返回
//    WouldBlock 而非死锁」的缓解路径的占坑侧预演。
#[test]
fn retry_upper_bound() {
    let mut arb = ResourceArbiter::new();
    let r = Resource::Fd(1);
    let write = vec![usage(r.clone(), AccessMode::Write)];

    // 竞争者持续持有 Write 占坑（模拟持锁任务始终不释放）
    assert!(arb.try_claim(&write), "竞争者占坑成功");
    assert!(arb.held(&r));

    // 有限重试 ≤8 次：竞争者不释放 → 全部失败，返回 false（WouldBlock 语义）
    const MAX_RETRIES: usize = 8;
    let mut attempts = 0usize;
    let mut acquired = false;
    for _ in 0..MAX_RETRIES {
        attempts += 1;
        if arb.try_claim(&write) {
            acquired = true;
            break;
        }
        // 失败不变量：每次失败后既有占坑不受影响（原子回滚，无残留）
        assert!(arb.held(&r), "第 {attempts} 次失败后竞争者占坑仍 held");
    }
    assert!(!acquired, "竞争者持续持有时 ≤8 次重试应全部失败");
    assert_eq!(
        attempts, MAX_RETRIES,
        "恰好消耗全部重试配额（有界，不无限循环）"
    );

    // 无残留：重试循环未在仲裁表留下任何新坑 —— 释放竞争者的单个占坑后
    // is_clean true（若任一次失败重试残留了占坑，单次 release 无法清空）。
    arb.release(&write);
    assert!(!arb.held(&r), "释放竞争者占坑后资源不再 held");
    assert!(arb.is_clean(), "释放竞争者占坑后仲裁表干净（重试无残留）");
}
