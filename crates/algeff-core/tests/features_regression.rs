//! A6 Verification 批 6：feature 组合 × 默认路径共存冒烟测试。
//!
//! 背景：`coeffects` / `virtual-clock` 为可选 feature（Cargo.toml），
//! runtime_features.rs 已覆盖 feature 专属行为。本文件验证 feature 开启时
//! **普通 interpret 路径（无 coeffects 参与）行为与默认构建一致** ——
//! feature 只附加能力，不得改变 Pure/Sequential 等基础节点语义。
//!
//! 门控：与 runtime_features.rs 同构，默认（features 皆关）下本文件不编译，
//! `cargo test --workspace` 不受影响；feature 开启时随 workspace 全量编译执行。

#![cfg(any(feature = "coeffects", feature = "virtual-clock"))]

use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;

use algeff_core::action::{Action, DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::ResourceRegistry;
use algeff_core::runtime::{interpret, Context, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoCapability};

/// 本地 current-thread runtime 驱动（interpret future 非 Send）。
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

/// 最小执行器：GetTime 返回固定值，其余 op 返回 Ok((Unit, None))。
#[derive(Default)]
struct MockExecutor {
    log: Arc<Mutex<Vec<String>>>,
}

impl MockExecutor {
    fn ops(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl SyscallExecutor for MockExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        let desc = format!("{op:?}");
        Box::pin(async move {
            self.log.lock().unwrap().push(desc);
            match op {
                DataOp::GetTime => Ok((Value::U64(21), UndoCapability::Identity)),
                _ => Ok((Value::Unit, UndoCapability::Identity)),
            }
        })
    }
}

/// feature 开启时，普通 interpret 路径（无 coeffects 参与）与默认语义一致：
/// Pure 单位元 + Sequential 值传递 + 无副作用产生（undo 栈空、无 op 记录）。
/// 注：virtual-clock 下 GetTime 语义改为读虚拟时钟（见 get_time 测试），故
/// 本测试（含「GetTime 到达执行器」断言）仅在非 virtual-clock 组合下编译。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn plain_path_unchanged_under_features() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::default();

    // Pure 单位元：返回 Unit，不触碰执行器/撤销栈。
    let v = drive(interpret(
        Action::Pure(Value::Unit),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    assert_eq!(v, Ok(Value::Unit));
    assert!(ex.ops().is_empty(), "Pure 不应产生 syscall");
    assert!(undo.is_empty(), "Pure 不应压入撤销操作");

    // Sequential 值传递：current 产生 21 → next 变换为 42（与默认构建相同）。
    let action = Action::Sequential {
        current: Box::new(Action::Syscall {
            op: DataOp::GetTime,
            resources: Default::default(),
            next: Box::new(Action::Pure),
        }),
        next: Box::new(|v| match v {
            Value::U64(n) => Action::Pure(Value::U64(n * 2)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(42)));
    assert_eq!(ex.ops(), vec!["GetTime"]);
    assert!(undo.is_empty(), "无 undo 的 syscall 不应压栈");
}

/// virtual-clock 下 GetTime 语义（审计 R1 契约-F2 修复）：时间读操作路由到
/// 虚拟时钟 `vc.now()`（确定性重放承诺，executor 注释同源），**不**到达
/// 物理执行器、也**不**推进时钟（读取非推进）。
#[cfg(feature = "virtual-clock")]
#[test]
fn get_time_reads_virtual_clock_without_advancing() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::default();

    let v = drive(interpret(
        Action::Sequential {
            current: Box::new(Action::Syscall {
                op: DataOp::GetTime,
                resources: Default::default(),
                next: Box::new(Action::Pure),
            }),
            next: Box::new(Action::Pure),
        },
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    // 时钟起点 ZERO → 读得 0ms（而非执行器 mock 的 21）
    assert_eq!(v, Ok(Value::U64(0)));
    assert!(
        ex.ops().is_empty(),
        "virtual-clock 下 GetTime 不得到达物理执行器"
    );
    assert_eq!(
        ctx.virtual_clock_mut().expect("virtual clock 存在").now(),
        std::time::Duration::ZERO,
        "读取 GetTime 不得推进逻辑时钟"
    );
}
