//! R4a 对抗审计（第 4 轮，分块 A）：Registry API 对抗 —— 绕过解释器，
//! 直接用 `ResourceRegistry` 做 allocate/take/merge/clear 组合序列
//! （模拟解释器外部使用 / 宿主进程直接驱动注册表），断言契约级不变量。
//!
//! R1-R3 已覆盖（不重复）：`registry_integration.rs`（open/write/close 生命周期、
//! D10 Replace → clear、D13 clone-merge 迁移、A4 随机序列）与 resource.rs 单测
//! （merge fd 身份/consumed 并集/next_fd max、clear 复位）。本文件攻击它们的
//! **组合序列边界**：
//!
//! 1. `reg_external_allocate_take_clear_d1_monotonic`：allocate ×5 → take(中段)
//!    → 再 allocate（D1：不复用被 take 的 fd）→ A4 消费 → clear → 再 allocate
//!    （D1：next_fd 保留、整体单调）+ A4 复位可再写；take 幂等（二次 take=None）。
//! 2. `reg_merge_d13_semantics_combos`：merge 三连——(a) 子 offset 高位区间但
//!    未实际分配 → 合并后游标收敛回基线（父 next_fd 不被大常数抬高）；
//!    (b) 子 offset + 实际分配 → fd 身份保留（原 fd 可见）+ consumed 并集 +
//!    next_fd=max 归一化；(c) 两子注册表合并 → 句柄并集、父原句柄保留、无重复 fd。
//! 3. `reg_external_mixed_sequence_d1_d13_d10`：宿主进程风格的长序列
//!    allocate→check_linear→take→remove→clone→子 allocate→merge→clear→allocate，
//!    每步断言 D1 单调 / D13 合并可见性 / D10 clear 复位且 next_fd 保留。

use std::sync::Arc;

use algeff_core::{
    AccessMode, Resource, ResourceHandle, ResourceRegistry, ResourceUsage, SysError,
};

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

fn mutex_handle() -> ResourceHandle {
    // 简单物理句柄：Arc 共享 tokio Mutex（无需 async 上下文、无临时文件）。
    ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(())))
}

/// allocate → take → A4 消费 → clear 组合序列下的 D1 单调不变量：
/// take 移除的 fd 不复用、clear 后 next_fd 保留、A4 线性标记随 clear 复位。
#[test]
fn reg_external_allocate_take_clear_d1_monotonic() {
    let mut reg = ResourceRegistry::new();

    // allocate ×5：单调 0..=4。
    let mut fds = Vec::new();
    for _ in 0..5 {
        fds.push(reg.allocate(mutex_handle()));
    }
    for i in 1..5 {
        assert!(fds[i] > fds[i - 1], "allocate 单调递增（D1）");
    }

    // take 中段句柄：句柄取出、lookup 消失、二次 take 为 None（幂等）。
    let taken = reg.take(fds[2]).expect("take 应取出句柄");
    assert!(matches!(taken, ResourceHandle::Mutex(_)));
    assert!(reg.lookup(fds[2]).is_none(), "take 后句柄不可见");
    assert!(
        reg.take(fds[2]).is_none(),
        "二次 take 为 None（无残留句柄）"
    );

    // 再 allocate：不复用被 take 的 fd（D1 全局唯一、永不复用）。
    let n1 = reg.allocate(mutex_handle());
    assert_eq!(n1, 5, "next_fd 跳过已分配区间（{n1} != 2）");
    assert!(fds.iter().all(|f| *f != n1), "新 fd 不复用任何历史 fd");

    // A4 use 语义：Write 不限次数（D-0xx 拆分，运行时维护独立 undo）。
    let r = Resource::Fd(fds[0]);
    assert!(reg
        .check_linear(&usage(r.clone(), AccessMode::Write))
        .is_ok());
    assert!(
        reg.check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok(),
        "同一资源二次 Write 允许（use 语义）"
    );

    // clear（D10 复位）：句柄与线性标记全清，next_fd 保留（D1）。
    reg.clear();
    for fd in &fds {
        assert!(reg.lookup(*fd).is_none(), "clear 后句柄全部释放");
    }
    assert!(reg.lookup(n1).is_none());
    assert!(
        reg.check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok(),
        "clear 复位 A4：同资源可再 Write"
    );

    // next_fd 保留：新 allocate 继续单调（n2 = 6，不复用 0..=5）。
    let n2 = reg.allocate(mutex_handle());
    assert_eq!(n2, 6, "clear 后 next_fd 保留（{n2}），整体单调不复用");
    assert!(n2 > n1 && n2 > fds[4], "新 fd 大于 clear 前全部 fd");
}

/// merge（D13）语义组合：(a) 子注册表 offset 高位区间但未实际分配 → 合并后
/// 游标收敛回基线（父 next_fd 不被大常数永久抬高）；(b) 子 offset + 实际分配
/// → fd 身份保留（原 fd 并入）、consumed 并集、next_fd = max 归一化；(c) 两子
/// 注册表连续 merge → 句柄并集、父原句柄保留、全部 fd 互不重复。
#[test]
fn reg_merge_d13_semantics_combos() {
    let mut parent = ResourceRegistry::new();
    let p1 = parent.allocate(mutex_handle());
    let p2 = parent.allocate(mutex_handle());
    assert_eq!((p1, p2), (0, 1), "父初始 fd 0..=1");

    // (a) offset 高位区间 + 未实际分配 → merge 收敛回基线。
    let mut child_a = parent.clone();
    child_a.offset_next_fd(1 << 48);
    parent.merge(child_a);
    let n_conv = parent.allocate(mutex_handle());
    assert_eq!(
        n_conv, 2,
        "offset 未分配 → 合并后游标收敛回基线（next_fd 不被抬高）"
    );

    // (b) offset + 实际分配 → fd 身份保留 + consumed 并集 + next_fd = max。
    let mut child_b = parent.clone();
    child_b.offset_next_fd(2 << 48);
    let c1 = child_b.allocate(mutex_handle());
    assert!(c1 > (2 << 48), "子分配落入全局唯一高位区间（F1）");
    assert!(child_b
        .check_linear(&usage(Resource::Fd(c1), AccessMode::Write))
        .is_ok());
    assert!(child_b
        .check_linear(&usage(Resource::Fd(c1), AccessMode::Own))
        .is_ok());
    assert!(parent.lookup(c1).is_none(), "合并前父不可见子句柄");

    parent.merge(child_b);
    assert!(
        parent.lookup(c1).is_some(),
        "合并后父以原 fd 可见子句柄（D13 fd 身份保留）"
    );
    assert_eq!(
        parent.check_linear(&usage(Resource::Fd(c1), AccessMode::Read)),
        Err(SysError::InvalidInput),
        "子路径 Write+Own 消费并入父（consumed 并集）"
    );
    let n_after = parent.allocate(mutex_handle());
    assert!(n_after > c1, "merge 后 next_fd = max，新分配不冲突（D1）");

    // (c) 两子注册表连续 merge → 句柄并集、父原句柄保留、无重复 fd。
    // 注意：两子克隆自同一父（同 next_fd），若都不偏移会分配到相同 fd——
    // 恰是 F1 修复的场景，故第二个子模拟右分支 offset 区间预分割。
    let mut child_c1 = parent.clone();
    let c2 = child_c1.allocate(mutex_handle());
    let mut child_c2 = parent.clone();
    child_c2.offset_next_fd(3 << 48);
    let c3 = child_c2.allocate(mutex_handle());
    assert_ne!(c2, c3, "子注册表 fd 区间互斥（F1 预分割）");
    parent.merge(child_c1);
    parent.merge(child_c2);
    assert!(
        parent.lookup(p1).is_some() && parent.lookup(p2).is_some(),
        "父原句柄保留"
    );
    assert!(parent.lookup(c1).is_some(), "第一子句柄保留");
    assert!(
        parent.lookup(c2).is_some() && parent.lookup(c3).is_some(),
        "两子新句柄并入"
    );

    let mut seen = std::collections::HashSet::new();
    for fd in [p1, p2, c1, c2, c3] {
        assert!(seen.insert(fd), "合并后出现重复 fd {fd}");
    }
}

/// 宿主进程风格混合长序列：allocate → check_linear → take → remove → clone
/// → 子 allocate（D13 隔离）→ merge → clear → allocate，逐步断言
/// D1 单调、D13 合并可见性与消费并集、D10 clear 复位且 next_fd 保留。
#[test]
fn reg_external_mixed_sequence_d1_d13_d10() {
    let mut reg = ResourceRegistry::new();

    // 阶段 1：allocate ×3。
    let a0 = reg.allocate(mutex_handle());
    let a1 = reg.allocate(mutex_handle());
    let a2 = reg.allocate(mutex_handle());
    assert_eq!([a0, a1, a2], [0, 1, 2], "初始分配 0..=2");

    // 阶段 2：A4 消费 + take + remove（外部使用：Own 关闭 + 显式移除）。
    assert!(reg
        .check_linear(&usage(Resource::Fd(a1), AccessMode::Write))
        .is_ok());
    assert!(reg.take(a1).is_some(), "take 取出句柄");
    assert!(reg.lookup(a1).is_none());
    reg.remove(a2);
    assert!(reg.lookup(a2).is_none(), "remove 移除句柄");

    // 阶段 3：D1 —— 新分配继续单调，不复用 a1/a2。
    let a3 = reg.allocate(mutex_handle());
    assert_eq!(a3, 3, "take/remove 后 next_fd 不受影响（D1）");

    // 阶段 4：D13 —— clone 隔离子任务，子分配 + 消费线性。
    let mut child = reg.clone();
    let c1 = child.allocate(mutex_handle());
    let c2 = child.allocate(mutex_handle());
    assert!(child
        .check_linear(&usage(Resource::Fd(c1), AccessMode::Write))
        .is_ok());
    assert!(reg.lookup(c1).is_none(), "合并前父不可见子句柄");

    // 阶段 5：merge 回父 —— fd 身份保留、consumed 并集、next_fd = max。
    reg.merge(child);
    assert!(
        reg.lookup(c1).is_some() && reg.lookup(c2).is_some(),
        "合并后子句柄可见"
    );
    assert!(reg.lookup(a0).is_some(), "父原句柄保留");
    // 子路径 Write（use 语义）并入父：允许重复（运行时维护独立 undo）。
    assert!(
        reg.check_linear(&usage(Resource::Fd(c1), AccessMode::Write))
            .is_ok(),
        "merge 后 Write 仍允许（use 语义，不消费）"
    );

    // 阶段 6：D10 —— clear 复位句柄与线性标记，next_fd 保留（D1）。
    reg.clear();
    for fd in [a0, a1, a2, a3, c1, c2] {
        assert!(reg.lookup(fd).is_none(), "clear 后 fd {fd} 句柄释放");
    }
    assert!(
        reg.check_linear(&usage(Resource::Fd(c1), AccessMode::Write))
            .is_ok(),
        "clear 复位 A4 线性标记"
    );

    // 阶段 7：clear 后 next_fd 保留 —— 新分配继续单调且不复用任何历史 fd。
    let a4 = reg.allocate(mutex_handle());
    assert!(
        a4 > c2 && a4 > a3,
        "clear 后 next_fd 保留（D1），新分配不复用"
    );
}
