//! R5a 对抗审计（第 5 轮，分块 A，终轮 —— 修复回归终验）：递归深度守卫边界。
//!
//! 终轮重点：**修复回归**。RFC-11（A2 批 7）为 R4c 发现的 HIGH（递归栈溢出 →
//! 进程级 abort，Windows debug 2MB 栈实测崩溃边界 ~104-108）新增深度守卫：
//! `interpret_impl` 递归入口维护嵌套深度计数器，超阈值 64（迭代 1 复测裁决，比实测
//! 边界留 ~8% 余量）返回 `SysError::Other(105)`（ENOBUFS=105「嵌套资源耗尽」
//! 语义近似）——拒绝服务面转为可恢复错误，且不误伤合法嵌套。
//!
//! 本文件（core 侧，解释器语义面，零 Syscall）攻击 **守卫边界精确压力**：
//! - 深度 63（阈值下沿）成功、64/65（上沿 +1/+2）Err(Other(105)) ——
//!   守卫不误伤合法嵌套、且在任何合法深度下于栈溢出之前触发（测试能跑完即
//!   证明未发生 STATUS_STACK_OVERFLOW abort）；
//! - 深度 62 确认下沿余量；守卫错误后同 Runtime 再跑 63 成功 —— 守卫按调用
//!   起算、不粘滞。
//!
//! E2E 组合面（五连回归 / Catch×Timeout 组合 / 50 轮风暴 / 修复点交互）在
//! `crates/algeff-std/tests/adversarial_r5a.rs`（真实 TokioExecutor）。
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 在 tokio 上下文之外调用。`NoopExecutor` 仅用于构造 Runtime，全部断言不
//! 依赖执行器返回值（本文件不含任何 Syscall）。

use algeff_core::{
    Action, BoxFuture, ResourceRegistry, Runtime, SysError, SyscallExecutor, UndoOp, Value,
};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R3b/R4c core 侧相同约定）──────────

/// 最小执行器：仅用于构造 Runtime。本文件的全部断言均不依赖执行器。
struct NoopExecutor;

impl SyscallExecutor for NoopExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a algeff_core::DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async { Ok((Value::Unit, None)) })
    }
}

/// 深度 depth 的嵌套 Sequential：current 为下一层；叶子返回 U64(300)，每层
/// next 原样上抛（值保真）。与 `interpreter.rs`/`adversarial_r4c.rs` 同构。
/// 深度语义（RFC-11 守卫计数）：`interpret_impl` 递归入口 depth 从 0 起算，
/// `run_sub_impl` 每层 +1，超 64 即返回 `Other(105)`。故 depth=63 的叶子在
/// depth 63（<64）正常执行；depth=64/65 的叶子/中途节点在 depth 64 触发守卫。
fn nested_seq(depth: u64) -> Action {
    if depth == 0 {
        return Action::Pure(Value::U64(300));
    }
    Action::Sequential {
        current: Box::new(nested_seq(depth - 1)),
        next: Box::new(|v| Action::Pure(v)),
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2a：守卫边界精确压力 —— 深度 63/64/65（63 成功、64/65 Err(Other(105))）
// ══════════════════════════════════════════════════════════════════════

/// 边界下沿 62/63 成功 + 上沿 64/65 触发守卫（Err(Other(105))）：
/// 守卫不误伤合法嵌套、且在栈溢出之前触发。进程不 abort（测试能跑完即证明）。
#[test]
fn guard_boundary_63_ok_64_65_err() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));

    // 下沿：94/95 均正常执行（95 是阈值下沿 —— 叶子恰在 depth 95）。
    assert_eq!(
        rt.run_blocking(nested_seq(62)).unwrap(),
        Value::U64(300),
        "深度 62 应在守卫阈值（64）之下正常执行"
    );
    assert_eq!(
        rt.run_blocking(nested_seq(63)).unwrap(),
        Value::U64(300),
        "深度 63（阈值下沿）应正常执行，守卫不误伤合法嵌套"
    );

    // 上沿：96/97 均在深度 96 触发守卫 → Err(Other(105))（ENOBUFS 语义近似）。
    assert_eq!(
        rt.run_blocking(nested_seq(64)).unwrap_err(),
        SysError::Other(105),
        "深度 64（阈值上沿）应返回深度守卫错误，收到非 Other(105)"
    );
    assert_eq!(
        rt.run_blocking(nested_seq(65)).unwrap_err(),
        SysError::Other(105),
        "深度 65 同样应返回深度守卫错误"
    );

    // 守卫不产生副作用残留：undo 空、registry 无句柄。
    assert!(rt.undo_stack().is_empty(), "守卫错误不产生 undo");
    assert!(
        rt.registry().lookup(0).is_none() && rt.registry().lookup(u64::MAX).is_none(),
        "守卫错误不分配资源（registry 空）"
    );

    // 守卫按调用起算、不粘滞：同 Runtime 守卫触发后再跑下沿 95 依旧成功。
    assert_eq!(
        rt.run_blocking(nested_seq(63)).unwrap(),
        Value::U64(300),
        "守卫触发后同 Runtime 再执行下沿深度 63 应成功（守卫不粘滞）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2b：守卫错误沿调用链上抛 = 可捕获错误（拒绝服务面转可恢复）。
// （Catch/Timeout 组合的 E2E 版在 std 侧；此处验证解释器层裸 Catch 捕获。）
// ══════════════════════════════════════════════════════════════════════

/// 超深蓝图外包 Catch → handler 收到 Other(105) 并执行：守卫错误沿嵌套
/// Sequential 链逐层上抛至最近的 Catch，不触发进程级 abort。
#[test]
fn guard_depth64_wrapped_catch_handler_receives_105() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let action = Action::Catch {
        action: Box::new(nested_seq(64)),
        handler: Box::new(|e| Action::Pure(Value::Str(format!("handled:{e}")))),
    };
    assert_eq!(
        rt.run_blocking(action).unwrap(),
        Value::Str("handled:Other(105)".to_string()),
        "Catch 应捕获深度守卫错误（深度 64）并执行 handler"
    );
    assert!(rt.undo_stack().is_empty(), "捕获路径不残留 undo");
}
