//! R4c 对抗审计（分块 C，第 1 部分）：规模/栈深边界 —— 长链执行（trampoline）。
//!
//! 攻击方法论：与 R3b core 侧相同——真实 `Runtime`（`run_blocking` →
//! `interpret_impl` 全链路）驱动；本文件不含任何 Syscall，`NoopExecutor`
//! 仅用于构造 `Runtime`，所有断言不依赖执行器返回值（解释器语义面）。
//!
//! R1-R3 已覆盖（不重复）：R1 值流 and_then 仅 5 层、Scope 3 层、Alloc 1MB；
//! R2 Fork 树深度 5/6（8 叶）；R3b Alloc 内存面。**本文件攻击 R1-R3 未触及
//! 的栈深压力面**：
//!
//! 1. **1000 节点 Sequential 链（纯值传递）**：急切构造 1000 层嵌套 CPS 树，
//!    每个节点 `current` 为 Pure、`next` 校验收到的值后给出下一节点。解释器
//!    主循环为 trampoline（runtime.rs `interpret_impl`）——1000 次循环迭代、
//!    O(1) 调用栈；若解释器按树深递归，本树会直接吃穿栈（攻击点）。
//! 2. **500 层嵌套 Choose 常量链**：500 层 Choose 树、cond 恒真取 then 分支。
//!    Choose 在主循环内选择分支（不递归），500 层深树按 500 次迭代执行。
//! 3. **嵌套 Sequential（递归子 future 路径栈深探针）**：`current` 本身
//!    是下一层 Sequential——`run_sub_impl` 每层 Box::pin 递归。**实测发现
//!    HIGH：Windows debug 默认 2MB 线程栈下深度 ~110-120 即栈溢出（进程
//!    abort），release 深度 1000 同样溢出**；测试在安全深度 64 固定回归，
//!    崩溃边界作为 finding 上报（修复在 runtime.rs，审计范围外）。
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 与 `run_blocking` 在 tokio 上下文之外调用。

use algeff_core::{
    Action, BoxFuture, ResourceRegistry, Runtime, SysError, SyscallExecutor, UndoCapability, Value,
};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R3b core 侧相同约定）──────────────

/// 最小执行器：仅用于构造 Runtime。本文件的全部断言均不依赖执行器。
struct NoopExecutor;

impl SyscallExecutor for NoopExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a algeff_core::DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        Box::pin(async { Ok((Value::Unit, UndoCapability::Identity)) })
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1a：1000 节点 Sequential 链（纯值传递）。
//
// 急切构造 1000 层嵌套 CPS 树：`Sequential { current: Pure(U64(i)), next }`，
// next 断言收到第 i 个节点的值（值流保真）后返回下一节点。整棵树 1000 层深、
// 1000 个节点全部执行 —— 主循环为 trampoline，每节点一次迭代、栈 O(1)。
// 若解释器改为按树深递归，本树将栈溢出（fail-fast，无需栈上限探测）。
// ══════════════════════════════════════════════════════════════════════

/// 节点 i：current 产出 U64(i)，next 校验值并给出节点 i+1；i == n 时返回
/// Pure(U64(n))。急切递归构造（执行时整树已 1000 层深）。
fn seq_chain(i: u64, n: u64) -> Action {
    if i == n {
        return Action::Pure(Value::U64(n));
    }
    Action::Sequential {
        current: Box::new(Action::Pure(Value::U64(i))),
        next: Box::new(move |v| {
            assert_eq!(v, Value::U64(i), "第 {i} 节点值流保真（收到 {v:?}）");
            seq_chain(i + 1, n)
        }),
    }
}

#[test]
fn trampoline_1000_node_sequential_chain_values_correct() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let v = rt.run_blocking(seq_chain(0, 1000)).unwrap();
    assert_eq!(
        v,
        Value::U64(1000),
        "1000 节点链最终值正确（trampoline 迭代执行，无递归栈溢出）"
    );
    assert!(rt.undo_stack().is_empty(), "纯值链不产生 undo");
    // 纯值链不经执行器、不分配资源：registry 无任何句柄（lookup 不可见）。
    assert!(
        rt.registry().lookup(0).is_none() && rt.registry().lookup(u64::MAX).is_none(),
        "纯值链不分配资源（registry 空）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1b：500 层嵌套 Choose 常量链。
//
// Choose 在主循环内选择分支（不递归）；500 层深树按 500 次迭代执行。
// cond 恒真 → 恒取 then 分支（下一层 Choose），else 分支为哨兵值（若被误选
// 立即暴露）。攻击点：常量 cond 链若被解释器当作递归处理，500 层深树会栈
// 溢出；trampoline 下为迭代。
// ══════════════════════════════════════════════════════════════════════

/// 500 层 Choose：cond 恒真取 then（下一层 Choose）；叶子返回 U64(500)。
fn choose_chain(i: u64, n: u64) -> Action {
    if i == n {
        return Action::Pure(Value::U64(n));
    }
    Action::Choose {
        cond: Box::new(|_| true),
        then_branch: Box::new(choose_chain(i + 1, n)),
        else_branch: Box::new(Action::Pure(Value::U64(u64::MAX))),
    }
}

#[test]
fn trampoline_500_nested_choose_constant_chain() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let v = rt.run_blocking(choose_chain(0, 500)).unwrap();
    assert_eq!(
        v,
        Value::U64(500),
        "500 层常量 Choose 链按迭代执行，最终值正确（未误走 else 哨兵）"
    );
    assert!(rt.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1c：嵌套 Sequential（递归子 future 路径栈深探针）——**实测发现**。
//
// 与 1a/1b 的 trampoline 平链相反：每个节点的 `current` 是下一层
// Sequential，`run_sub_impl` 每层 `Box::pin(async move { interpret_impl(..) })`
// 递归。**实测（本块调试过程，Windows + debug + 默认 2MB 测试线程栈）：
// 深度 100 通过、120 即 STATUS_STACK_OVERFLOW（进程级 abort）**——每层栈
// 消耗 ≈ 13~20KB（debug 未优化 async 状态机 + Action/DataOp 大枚举栈槽）。
// release 构建在深度 1000 同样溢出。即：解释器对嵌套型蓝图**无递归深度
// 上限**，约百层即可崩溃（拒绝服务）；Fork 顺序路径（run_sub_impl 同一
// 递归）与 Scope/Catch/Timeout 嵌套同样受影响。修复需堆上延续或深度限制
// （runtime.rs，本审计范围外，由 CTO 裁决；本测试在安全深度 64 固定回归）。
// ══════════════════════════════════════════════════════════════════════

/// 深度 depth 的嵌套 Sequential：current 为下一层；叶子返回 U64(300)，
/// 每层 next 原样上抛（值保真）。深度 64：远离实测崩溃边界（~110-120，
/// Windows debug 默认 2MB 线程栈），仍远超 R1-R3 的任何嵌套深度（≤8）。
fn nested_seq(depth: u64) -> Action {
    if depth == 0 {
        return Action::Pure(Value::U64(300));
    }
    Action::Sequential {
        current: Box::new(nested_seq(depth - 1)),
        next: Box::new(Action::Pure),
    }
}

#[test]
fn nested_sequential_62_deep_recursive_frames_values_flow() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    // 62 层 = 阈值 64 的下沿余量（迭代 1 复测裁决：取消传播帧膨胀后
    // 实测 80 OK / 88 崩，阈值由 96 降为 64——64 层本身触发守卫）。
    let v = rt.run_blocking(nested_seq(62)).unwrap();
    assert_eq!(
        v,
        Value::U64(300),
        "64 层嵌套 Sequential 经递归子 future 路径执行，值逐层上抛保真"
    );
    assert!(rt.undo_stack().is_empty());
}
