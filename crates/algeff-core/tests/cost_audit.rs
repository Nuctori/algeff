//! 开销代数化落地审计（D-104 落点 a，运行时可审计路线）。
//!
//! 不 mock、全部经真实 `Runtime`（`run_blocking` → `interpret` 全链路）驱动。
//! Mock executor 真实执行 DataOp（返回 Invertible/Identity），使 `exec_via`
//! 落点触发 `undo.add_cost`。断言 `UndoStack::accrued_cost()` 的运行时累计值。
//!
//! 关键不变量（文档 C1）：幂等键命中 → 不调 `exec_via` → 开销不累计。

use algeff_core::{
    Action, BoxFuture, DataOp, ResourceRegistry, Runtime, SysError, SyscallExecutor,
    UndoCapability, UndoStack, Value,
};

/// Mock executor：真实执行 DataOp 并返回对应代数角色（Open→Invertible 等），
/// 使 `exec_via` 落点触发运行时开销记账。Stat 等 Identity 同样落点但记零占用。
struct RecordingExecutor;

impl SyscallExecutor for RecordingExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        // 角色不依赖具体 op：统一返回 Invertible（open/close 等均有逆，测试不关心逆体）。
        Box::pin(async {
            Ok((
                Value::Unit,
                UndoCapability::Invertible(Box::pin(async { Ok(()) })),
            ))
        })
    }
}

fn open(path: &str, next: impl FnOnce(Value) -> Action + Send + 'static) -> Action {
    Action::Syscall {
        op: DataOp::Open {
            path: path.into(),
            flags: Default::default(),
        },
        resources: Vec::new(),
        next: Box::new(next),
    }
}

fn stat(path: &str, next: impl FnOnce(Value) -> Action + Send + 'static) -> Action {
    Action::Syscall {
        op: DataOp::Stat { path: path.into() },
        resources: Vec::new(),
        next: Box::new(next),
    }
}

fn idempotent(key: &str, inner: Action) -> Action {
    Action::Idempotent {
        key: key.to_string(),
        inner: Box::new(inner),
        next: Box::new(Action::Pure),
    }
}

/// 顺序执行一个 Open + 一个 Stat → 累计开销 = write{1}（Open）+ read{1}（Stat）
/// + occupy{1}（Open 创建句柄）= {read:[1,1], write:[1,1], occupy:[1,1]}。
#[test]
fn accrued_cost_sums_sequential_effects() {
    let mut rt = Runtime::new(Box::new(RecordingExecutor));
    let act = open("/a", |_| stat("/a", |_| Action::Pure(Value::Unit)));
    rt.run_blocking(act).unwrap();
    let cost = rt.undo_stack().accrued_cost();
    assert_eq!(cost.read.min, 1);
    assert_eq!(cost.read.max, 1);
    assert_eq!(cost.write.min, 1);
    assert_eq!(cost.write.max, 1);
    assert_eq!(cost.occupy.min, 1);
    assert_eq!(cost.occupy.max, 1);
}

/// 幂等键命中 → 开销不累计（文档 C1 幂等塌缩）。
/// 第一次执行累计 {read:1, write:1, occupy:1}；相同 key 重试命中缓存，
/// 不调 `exec_via` → 累计值保持原值（不翻倍）。
#[test]
fn idempotent_cache_hit_collapses_cost() {
    let mut rt = Runtime::new(Box::new(RecordingExecutor));
    let make = || idempotent("cost:idem:1", open("/x", |_| Action::Pure(Value::Unit)));
    rt.run_blocking(make()).unwrap();
    let first = rt.undo_stack().accrued_cost();
    assert_eq!(first.write.max, 1);
    assert_eq!(first.occupy.max, 1);
    // 重试：命中缓存，inner 不执行 → 开销不变。
    rt.run_blocking(make()).unwrap();
    let second = rt.undo_stack().accrued_cost();
    assert_eq!(second.write.max, 1, "幂等命中不应重复累计开销（C1 塌缩）");
    assert_eq!(second.occupy.max, 1, "幂等命中不应重复累计占用（C1 塌缩）");
}

/// 纯计算（无 DataOp）开销为零：Pure / Alloc 不触发 `exec_via`。
#[test]
fn pure_compute_has_zero_cost() {
    let mut rt = Runtime::new(Box::new(RecordingExecutor));
    let act = Action::Pure(Value::Unit);
    rt.run_blocking(act).unwrap();
    assert_eq!(
        rt.undo_stack().accrued_cost(),
        UndoStack::new().accrued_cost()
    );
}
