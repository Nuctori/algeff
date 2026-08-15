//! A2 解释器集成测试（contracts.md §任务 A2；pdr.md §2.1 / §4 / §5.1）。
//!
//! 说明：`interpret` 的 future 因冻结签名 `&mut dyn SyscallExecutor`（trait 无
//! `Send` 超 trait）而**非 Send**；`Runtime` 自持 tokio reactor（D9），`Runtime::new`
//! 在 tokio 上下文内会 panic。因此全部测试用普通 `#[test]` + 本地 current-thread
//! runtime 驱动（`drive`），不在 `#[tokio::test]` 中嵌套 `Runtime::new`。
//!
//! 另：`Runtime::run_blocking` 与 `Runtime::virtual_clock()`（feature 开启时）为
//! 本文件新增的运行时入口/访问器，测试通过公开 API 驱动解释器。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use algeff_core::action::{Action, DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{AccessMode, Resource, ResourceRegistry, ResourceUsage};
use algeff_core::runtime::{interpret, Context, Runtime, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoOp};

/// 本地 current-thread runtime 驱动（interpret future 非 Send，不能用多线程 block_on）。
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

/// execute 的返回结果配置。
#[derive(Clone)]
enum MockOutcome {
    Value(Value),
    Err(SysError),
}

/// 可配置 Mock 执行器：记录 op 调用序列，可按 op 描述返回 Value/Err/延迟/undo。
#[derive(Default)]
struct MockExecutor {
    /// 每次 execute 的 op 描述（调用顺序）。
    log: Arc<Mutex<Vec<String>>>,
    /// undo 执行记录（recover 顺序）。
    undo_log: Arc<Mutex<Vec<String>>>,
    /// op 描述 → 返回结果（未配置 → Ok(Value::Unit)）。
    responses: HashMap<String, MockOutcome>,
    /// 每个 execute 的执行延迟（timeout 测试用）。
    delay: Duration,
    /// 是否在 Ok 时附带 undo。
    with_undo: bool,
}

impl MockExecutor {
    fn new() -> Self {
        Self::default()
    }

    fn respond(&mut self, desc: &str, out: MockOutcome) {
        self.responses.insert(desc.to_string(), out);
    }

    fn ops(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn undo_ops(&self) -> Vec<String> {
        self.undo_log.lock().unwrap().clone()
    }
}

/// op → 稳定描述串（测试按此配置响应并断言调用序）。
fn describe(op: &DataOp) -> String {
    match op {
        DataOp::Write { fd, .. } => format!("write:{fd}"),
        DataOp::Read { fd, len } => format!("read:{fd}:{len}"),
        DataOp::GetTime => "gettime".to_string(),
        other => format!("{other:?}"),
    }
}

impl SyscallExecutor for MockExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        let desc = describe(op);
        Box::pin(async move {
            self.log.lock().unwrap().push(desc.clone());
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let out = match self.responses.get(&desc).cloned() {
                Some(MockOutcome::Err(e)) => return Err(e),
                Some(MockOutcome::Value(v)) => v,
                None => Value::Unit,
            };
            let undo: Option<UndoOp> = if self.with_undo {
                let label = format!("undo({desc})");
                let undo_log = self.undo_log.clone();
                Some(Box::pin(
                    async move { undo_log.lock().unwrap().push(label) },
                ))
            } else {
                None
            };
            Ok((out, undo))
        })
    }
}

/// 构造返回自身结果的单步 Syscall（next = 恒等 Pure）。
fn syscall_step(op: DataOp, resources: Vec<ResourceUsage>) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(Action::Pure),
    }
}

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

// ── 1. Pure 单位元 ────────────────────────────────────────────────────

#[test]
fn pure_unit() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    let v = drive(interpret(
        Action::Pure(Value::Unit),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    assert_eq!(v, Ok(Value::Unit));
    assert!(ex.ops().is_empty());
}

// ── 2. Sequential 值传递 ─────────────────────────────────────────────

#[test]
fn sequential_value_flow() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(21)));

    // current 产生 21 → next 变换为 42
    let action = Action::Sequential {
        current: Box::new(syscall_step(DataOp::GetTime, vec![])),
        next: Box::new(|v| match v {
            Value::U64(n) => Action::Pure(Value::U64(n * 2)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(42)));
    assert_eq!(ex.ops(), vec!["gettime"]);
}

// ── 3. Choose 分支选择 ───────────────────────────────────────────────

#[test]
fn choose_picks_then_branch() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(5)));
    ex.respond("read:7:4", MockOutcome::Value(Value::Bool(true)));

    // cur 初始为 Unit → cond 成立 → then 分支
    let action = Action::Choose {
        cond: Box::new(|cur| matches!(cur, Value::Unit)),
        then_branch: Box::new(syscall_step(DataOp::GetTime, vec![])),
        else_branch: Box::new(syscall_step(DataOp::Read { fd: 7, len: 4 }, vec![])),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(5)));
    assert_eq!(ex.ops(), vec!["gettime"]);
}

#[test]
fn choose_picks_else_branch() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(5)));
    ex.respond("read:7:4", MockOutcome::Value(Value::Bool(true)));

    let action = Action::Choose {
        cond: Box::new(|_| false),
        then_branch: Box::new(syscall_step(DataOp::GetTime, vec![])),
        else_branch: Box::new(syscall_step(DataOp::Read { fd: 7, len: 4 }, vec![])),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Bool(true)));
    assert_eq!(ex.ops(), vec!["read:7:4"]);
}

// ── 4. Fork：冲突检测 + 顺序执行 ─────────────────────────────────────

#[test]
fn fork_conflict_sequential_execution() {
    // 同资源 Write：can_parallel = false（断言检测值，与解释器内部一致）
    let l_set = vec![usage(Resource::Fd(1), AccessMode::Write)];
    let r_set = vec![usage(Resource::Fd(1), AccessMode::Write)];
    assert!(!ResourceRegistry::new().can_parallel(&l_set, &r_set));

    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("write:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("read:1:8", MockOutcome::Value(Value::U64(20)));

    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            l_set,
        )),
        right: Box::new(syscall_step(DataOp::Read { fd: 1, len: 8 }, r_set)),
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(a), Value::U64(b)) => Action::Pure(Value::U64(a + b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(30))); // combine 结果正确
    assert_eq!(ex.ops(), vec!["write:1", "read:1:8"]); // 顺序执行（先左后右）
}

#[test]
fn fork_disjoint_resources_can_parallel() {
    // 异资源：can_parallel = true
    let l_set = vec![usage(Resource::Fd(1), AccessMode::Write)];
    let r_set = vec![usage(Resource::Fd(2), AccessMode::Write)];
    assert!(ResourceRegistry::new().can_parallel(&l_set, &r_set));

    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("write:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:2", MockOutcome::Value(Value::U64(20)));

    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![],
            },
            l_set,
        )),
        right: Box::new(syscall_step(
            DataOp::Write {
                fd: 2,
                data: vec![],
            },
            r_set,
        )),
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(a), Value::U64(b)) => Action::Pure(Value::U64(a + b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(30)));
    assert_eq!(ex.ops(), vec!["write:1", "write:2"]);
}

// ── 5. Scope：cwd 恢复 ───────────────────────────────────────────────

#[test]
fn scope_restores_cwd() {
    let mut ctx = Context::new();
    ctx.cwd = PathBuf::from("/app");
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(7)));

    let action = Action::Scope {
        base: PathBuf::from("sub/dir"),
        inner: Box::new(syscall_step(DataOp::GetTime, vec![])),
        next: Box::new(Action::Pure),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(7)));
    assert_eq!(ctx.cwd, PathBuf::from("/app")); // 恢复原 cwd
    assert_eq!(ex.ops(), vec!["gettime"]);
}

#[test]
fn scope_restores_cwd_on_error() {
    let mut ctx = Context::new();
    ctx.cwd = PathBuf::from("/app");
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Err(SysError::NotFound));

    let action = Action::Scope {
        base: PathBuf::from("sub"),
        inner: Box::new(syscall_step(DataOp::GetTime, vec![])),
        next: Box::new(Action::Pure),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Err(SysError::NotFound));
    assert_eq!(ctx.cwd, PathBuf::from("/app")); // 错误路径同样恢复
}

// ── 6. Alloc：全零字节 ───────────────────────────────────────────────

#[test]
fn alloc_zeroed_bytes() {
    let got = Arc::new(Mutex::new(None));
    let got2 = Arc::clone(&got);
    let action = Action::Alloc {
        len: 8,
        next: Box::new(move |v| {
            *got2.lock().unwrap() = Some(v);
            Action::Pure(Value::Unit)
        }),
    };
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Unit));
    assert_eq!(*got.lock().unwrap(), Some(Value::Bytes(vec![0u8; 8])));
}

// ── 7. Sleep：真实等待（默认 feature；virtual-clock 下见下方替换测试）──

#[cfg(not(feature = "virtual-clock"))]
#[test]
fn sleep_elapses() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    let action = Action::Sleep {
        duration: Duration::from_millis(10),
        next: Box::new(Action::Pure),
    };
    let start = std::time::Instant::now();
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Unit));
    assert!(
        start.elapsed() >= Duration::from_millis(10),
        "elapsed {:?}",
        start.elapsed()
    );
}

// ── 8. Timeout：超时触发 on_timeout ──────────────────────────────────

#[test]
fn timeout_fires_on_timeout() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.delay = Duration::from_millis(100); // 慢执行

    let action = Action::Timeout {
        action: Box::new(syscall_step(DataOp::GetTime, vec![])),
        duration: Duration::from_millis(10),
        on_timeout: Box::new(Action::Pure(Value::U64(99))),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(99))); // 超时 → on_timeout 结果
    assert_eq!(ex.ops(), vec!["gettime"]); // 慢 op 已启动后被取消
}

// ── 9. Catch：错误处理 ───────────────────────────────────────────────

#[test]
fn catch_error_invokes_handler() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Err(SysError::NotFound));
    let handled = Arc::new(Mutex::new(false));
    let handled2 = Arc::clone(&handled);

    let action = Action::Catch {
        action: Box::new(syscall_step(DataOp::GetTime, vec![])),
        handler: Box::new(move |e| {
            *handled2.lock().unwrap() = true;
            Action::Pure(Value::Str(format!("handled:{e}")))
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Str("handled:NotFound(errno 2)".to_string())));
    assert!(*handled.lock().unwrap());
}

#[test]
fn catch_passthrough_on_ok() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(1)));
    let handled = Arc::new(Mutex::new(false));
    let handled2 = Arc::clone(&handled);

    let action = Action::Catch {
        action: Box::new(syscall_step(DataOp::GetTime, vec![])),
        handler: Box::new(move |_e| {
            *handled2.lock().unwrap() = true;
            Action::Pure(Value::Unit)
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(1)));
    assert!(!*handled.lock().unwrap()); // Ok 不触发 handler
}

// ── 10. Replace：先 recover 清空撤销栈，再执行 target ────────────────

#[test]
fn replace_recovers_undo_stack() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("gettime", MockOutcome::Value(Value::U64(1)));
    ex.respond("read:2:4", MockOutcome::Value(Value::U64(2)));

    // 两个 Syscall 累积 undo → Replace{ target }
    let action = Action::Sequential {
        current: Box::new(syscall_step(DataOp::GetTime, vec![])),
        next: Box::new(|_| Action::Sequential {
            current: Box::new(syscall_step(DataOp::Read { fd: 2, len: 4 }, vec![])),
            next: Box::new(|_| Action::Replace {
                target: Box::new(Action::Pure(Value::U64(55))),
            }),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(55))); // target 结果（不回原流）
    assert_eq!(
        ex.undo_ops(),
        vec!["undo(read:2:4)", "undo(gettime)"] // recover：LIFO
    );
    assert!(undo.is_empty()); // 撤销栈已清空
}

// ── 11. Undo 栈 LIFO ─────────────────────────────────────────────────

#[test]
fn undo_stack_lifo_order() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.with_undo = true;

    let action = Action::Sequential {
        current: Box::new(syscall_step(DataOp::GetTime, vec![])),
        next: Box::new(|_| Action::Sequential {
            current: Box::new(syscall_step(DataOp::Read { fd: 2, len: 4 }, vec![])),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        }),
    };
    let v = drive(async {
        let v = interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex).await;
        assert_eq!(undo.len(), 2); // 两个 undo 已压栈
        undo.recover().await; // recoverΓ：LIFO
        v
    });
    assert_eq!(v, Ok(Value::Unit));
    assert_eq!(ex.undo_ops(), vec!["undo(read:2:4)", "undo(gettime)"]);
    assert!(undo.is_empty());
}

// ── 12. Runtime 冒烟 ─────────────────────────────────────────────────

#[test]
fn runtime_smoke() {
    // Runtime 自持 tokio reactor（D9）：在普通 #[test]（tokio 上下文之外）构造。
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let v = rt.run_blocking(Action::Pure(Value::Unit));
    assert_eq!(v, Ok(Value::Unit));
}

#[test]
fn runtime_run_blocking_full_path() {
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let v = rt.run_blocking(Action::Alloc {
        len: 3,
        next: Box::new(Action::Pure),
    });
    assert_eq!(v, Ok(Value::Bytes(vec![0, 0, 0])));
}

// ── 13. 边界补强（批 2）：空链收敛 / 嵌套 / 错误路径 / 默认 ENOSYS ──

#[test]
fn sequential_empty_chain_convergence() {
    // current 为 Pure：无副作用、不产生 syscall，next 仍正确接续（trampoline 收敛）。
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();

    let action = Action::Sequential {
        current: Box::new(Action::Pure(Value::U64(3))),
        next: Box::new(|v| match v {
            Value::U64(n) => Action::Pure(Value::U64(n + 1)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(4)));
    assert!(ex.ops().is_empty());
}

#[test]
fn scope_nested_cwd_join_and_restore() {
    let mut ctx = Context::new();
    ctx.cwd = PathBuf::from("/app");
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(7)));

    // 拼接语义：外层 base "a" → cwd /app/a；内层 base "b" 在 /app/a 上再拼 → /app/a/b。
    // （与解释器内部同源的 canonicalize 语义，先行断言期望拼接结果）
    let joined = reg.canonicalize_path(
        std::path::Path::new("b"),
        &reg.canonicalize_path(std::path::Path::new("a"), std::path::Path::new("/app")),
    );
    assert_eq!(joined, PathBuf::from("/app/a/b"));

    let action = Action::Scope {
        base: PathBuf::from("a"),
        inner: Box::new(Action::Scope {
            base: PathBuf::from("b"),
            inner: Box::new(syscall_step(DataOp::GetTime, vec![])),
            next: Box::new(Action::Pure),
        }),
        next: Box::new(Action::Pure),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(7))); // 内层 syscall 正常执行并透传
    assert_eq!(ctx.cwd, PathBuf::from("/app")); // 逐层恢复回原 cwd
    assert_eq!(ex.ops(), vec!["gettime"]);
}

#[test]
fn timeout_nested_inner_fires_first() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.delay = Duration::from_millis(100); // 慢执行

    // 内层 20ms 先超时 → 1；外层 500ms 不触发 → 整体返回内层 on_timeout 结果
    let action = Action::Timeout {
        action: Box::new(Action::Timeout {
            action: Box::new(syscall_step(DataOp::GetTime, vec![])),
            duration: Duration::from_millis(20),
            on_timeout: Box::new(Action::Pure(Value::U64(1))),
        }),
        duration: Duration::from_millis(500),
        on_timeout: Box::new(Action::Pure(Value::U64(2))),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(1))); // 内层超时优先
    assert_eq!(ex.ops(), vec!["gettime"]); // 慢 op 已启动后被内层取消
}

#[test]
fn timeout_nested_outer_fires_first() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.delay = Duration::from_millis(100); // 慢执行

    // 内层 200ms 尚未超时，外层 50ms 先触发 → 2（外层 on_timeout，内层整体被取消）
    let action = Action::Timeout {
        action: Box::new(Action::Timeout {
            action: Box::new(syscall_step(DataOp::GetTime, vec![])),
            duration: Duration::from_millis(200),
            on_timeout: Box::new(Action::Pure(Value::U64(1))),
        }),
        duration: Duration::from_millis(50),
        on_timeout: Box::new(Action::Pure(Value::U64(2))),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(2))); // 外层超时优先
    assert_eq!(ex.ops(), vec!["gettime"]);
}

#[test]
fn catch_after_partial_undo_keeps_stack() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("gettime", MockOutcome::Value(Value::U64(1)));
    ex.respond("read:2:4", MockOutcome::Err(SysError::NotFound));

    // gettime 成功（压入 undo）→ read 失败 → Catch handler 执行。
    // Catch 只处理错误，不得清空撤销栈：栈内容保留供后续 recover/Replace 使用。
    let action = Action::Catch {
        action: Box::new(Action::Sequential {
            current: Box::new(syscall_step(DataOp::GetTime, vec![])),
            next: Box::new(|_| Action::Sequential {
                current: Box::new(syscall_step(DataOp::Read { fd: 2, len: 4 }, vec![])),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
        }),
        handler: Box::new(|_e| Action::Pure(Value::U64(7))),
    };
    let v = drive(async {
        let v = interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex).await;
        assert_eq!(
            undo.len(),
            1,
            "Catch handler 执行时 undo 栈应保留 gettime 的逆操作"
        );
        v
    });
    assert_eq!(v, Ok(Value::U64(7)));
    // 栈仍可用：recover 按 LIFO 执行已压栈的逆操作
    drive(undo.recover());
    assert_eq!(ex.undo_ops(), vec!["undo(gettime)"]);
    assert!(undo.is_empty());
}

#[test]
fn watch_signal_default_enosys() {
    // MockExecutor 未实现 watch_signal → trait 默认返回 ENOSYS，解释器原样透传。
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();

    let action = Action::WatchSignal {
        signal: 2,
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Err(SysError::Other(38)));
    assert!(undo.is_empty()); // 出错不压栈
}

#[test]
fn invoke_default_enosys() {
    // MockExecutor 未实现 invoke → trait 默认返回 ENOSYS，解释器原样透传。
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();

    let action = Action::Invoke {
        foreign_id: 1,
        captures: vec![],
        yields: vec![],
        deterministic: true,
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Err(SysError::Other(38)));
    assert!(undo.is_empty());
}

#[test]
fn runtime_run_async_full_path() {
    // Runtime::run（async）完整路径：与 run_blocking 共用 interpret 语义。
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let v = drive(rt.run(Action::Alloc {
        len: 3,
        next: Box::new(Action::Pure),
    }));
    assert_eq!(v, Ok(Value::Bytes(vec![0, 0, 0])));
}

// ── 7b. Sleep + virtual clock（feature `virtual-clock`）──────────────

#[cfg(feature = "virtual-clock")]
#[test]
fn sleep_advances_virtual_clock() {
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let start = std::time::Instant::now();
    let v = rt.run_blocking(Action::Sleep {
        duration: Duration::from_secs(60),
        next: Box::new(Action::Pure),
    });
    assert_eq!(v, Ok(Value::Unit));
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "virtual clock 不应真实等待，elapsed {:?}",
        start.elapsed()
    );
    assert_eq!(
        rt.virtual_clock().expect("virtual clock 存在").now(),
        Duration::from_secs(60)
    );
}
