//! A2 解释器集成测试（contracts.md §任务 A2；pdr.md §2.1 / §4 / §5.1）。
//!
//! 说明：`interpret` 的递归 future 经非 Send 的本地 Box 包装（`LocalBoxFuture`）
//! 而**非 Send**（`SyscallExecutor` 的 `Send` 超 trait 已落地，决策 D19）；
//! `Runtime` 自持 tokio reactor（D9），`Runtime::new` 在 tokio 上下文内会 panic。
//! 因此全部测试用普通 `#[test]` + 本地 current-thread runtime 驱动（`drive`），
//! 不在 `#[tokio::test]` 中嵌套 `Runtime::new`。
//!
//! 另：`Runtime::run_blocking` 与 `Runtime::virtual_clock()`（feature 开启时）为
//! 本文件新增的运行时入口/访问器，测试通过公开 API 驱动解释器。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use algeff_core::action::{Action, DataOp, OpenFlags, PipeFlags, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{
    AccessMode, Resource, ResourceHandle, ResourceRegistry, ResourceUsage,
};
use algeff_core::runtime::{interpret, Context, Runtime, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoOp};

use proptest::prelude::*;
use proptest::test_runner::TestCaseError;

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
    /// 每次 execute 的执行线程 id（与 `log` 同序，Fork 并行路径证据）。
    thread_log: Arc<Mutex<Vec<ThreadId>>>,
    /// undo 执行记录（recover 顺序）。
    undo_log: Arc<Mutex<Vec<String>>>,
    /// op 描述 → 返回结果（未配置 → Ok(Value::Unit)）。
    responses: HashMap<String, MockOutcome>,
    /// 每个 execute 的执行延迟（timeout 测试用 / Fork 并行窗口）。
    delay: Duration,
    /// 是否在 Ok 时附带 undo。
    with_undo: bool,
    /// registry 分配的 fd 序列（每次 Open/PipeOpen 追加；S6/A2 属性测试断言
    /// 整棵 Fork 树分配的 fd 两两不相交）。
    alloc_log: Arc<Mutex<Vec<u64>>>,
    /// Open 分配的 fd → 路径记录（嵌套 Fork 映射覆盖检测载体：若并发分支撞 fd，
    /// 后写覆盖先写 → 条目数 < Open 次数，读回内容张冠李戴）。
    path_log: Arc<Mutex<HashMap<u64, PathBuf>>>,
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
        DataOp::Open { path, .. } => format!("open:{}", path.display()),
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
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        let desc = describe(op);
        Box::pin(async move {
            self.log.lock().unwrap().push(desc.clone());
            self.thread_log
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            // Open/PipeOpen：由注册表分配全局唯一 fd（D1），不产生 undo（简化，同
            // concurrency_stress.rs 的 MockExecutor）。
            if matches!(op, DataOp::Open { .. } | DataOp::PipeOpen { .. }) {
                let fd =
                    registry.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
                self.alloc_log.lock().unwrap().push(fd);
                if let DataOp::Open { path, .. } = op {
                    self.path_log.lock().unwrap().insert(fd, path.clone());
                }
                return Ok((Value::Fd(fd), None));
            }
            // Read：若 fd 在本执行器开过（path_log 命中），返回路径内容 —— 供
            // 「读回内容验证映射未被覆盖」断言（fd 撞档时内容张冠李戴）。
            if let DataOp::Read { fd, len } = op {
                if let Some(p) = self.path_log.lock().unwrap().get(fd).cloned() {
                    let mut bytes = p.to_string_lossy().as_bytes().to_vec();
                    bytes.truncate(*len);
                    return Ok((Value::Bytes(bytes), None));
                }
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

// ── 14. Fork 并行化（D14 阶段 3）+ D10 Replace registry 清空 ───────────

/// 异资源 Fork 走并行路径（`Runtime` 的 Shared 执行器通道）：
/// - 左右分支在不同阻塞线程执行（MockExecutor 记录每 op 线程 id，左右 op
///   线程不同 = 真并行证据，spawn_blocking × 2 各自 drive current-thread runtime）；
/// - 结果合并正确（combine）；
/// - D13 合并回父：左分支 Open 在子隔离 registry 分配的 fd（0）以**原 fd**
///   并入父 registry，父 next_fd = max 后继续分配不冲突。
#[test]
fn fork_parallel_true_path() {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    // 每个 execute 延迟 30ms：保证两个 spawn_blocking 子任务同时在飞
    // （阻塞池两个线程同时被占用 → 线程断言确定、无调度竞态）。
    ex.delay = Duration::from_millis(30);
    ex.respond("write:7", MockOutcome::Value(Value::U64(20)));

    let ops_log = Arc::clone(&ex.log);
    let thread_log = Arc::clone(&ex.thread_log);
    let mut rt = Runtime::new(Box::new(ex));

    // 父 registry 预置 1 个句柄（右分支 Write 的对象为既有 fd 7，不新分配；
    // 左分支 Open 在子隔离 registry 中从 next_fd=1 起分配 → 无子间 fd 冲突）。
    rt.registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));

    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/left"),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path("/left".to_string()), AccessMode::Read)],
        )),
        right: Box::new(syscall_step(
            DataOp::Write {
                fd: 7,
                data: vec![0xCC],
            },
            vec![usage(Resource::Fd(7), AccessMode::Write)],
        )),
        combine: Box::new(|l, r| match (l, r) {
            (Value::Fd(lfd), Value::U64(rn)) => Action::Pure(Value::U64(lfd + rn)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = rt.run_blocking(action);
    // 左 fd = 1（父预置 1 个后子隔离分配）+ 右 20 = 21
    assert_eq!(v, Ok(Value::U64(21)), "combine 结果合并正确");

    // 并行路径证据：左右 op 出现在不同线程（真并行，非顺序回退）。
    // 并行下 op 日志顺序不确定（两子任务并发），只断言集合成员与线程差。
    let ops = ops_log.lock().unwrap().clone();
    let threads = thread_log.lock().unwrap().clone();
    assert_eq!(ops.len(), 2, "左右各一个 op");
    let idx_l = ops
        .iter()
        .position(|o| o == "open:/left")
        .expect("左分支 op 已执行");
    let idx_r = ops
        .iter()
        .position(|o| o == "write:7")
        .expect("右分支 op 已执行");
    assert_ne!(
        threads[idx_l], threads[idx_r],
        "左右 op 应在不同线程执行（真并行）"
    );

    // D13 合并回父：左分支子 registry 的句柄以原 fd（1）并入父；
    // 父 next_fd = max → 继续分配不冲突。
    assert!(
        rt.registry().lookup(1).is_some(),
        "子 registry 句柄应合并回父（原 fd 保留）"
    );
    let nfd = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(nfd > 1, "合并后父 next_fd = max，新分配不冲突");
}

/// Fork 并行后的 undo 合并顺序：right 的 undo 先、left 的后 —— LIFO recover
/// 先弹 right 再弹 left，与顺序路径「left 先执行」的观察序一致。
#[test]
fn fork_parallel_undo_merge() {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("write:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:2", MockOutcome::Value(Value::U64(20)));

    let undo_log = Arc::clone(&ex.undo_log);
    let mut rt = Runtime::new(Box::new(ex));

    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            vec![usage(Resource::Fd(1), AccessMode::Write)],
        )),
        right: Box::new(syscall_step(
            DataOp::Write {
                fd: 2,
                data: vec![0xBB],
            },
            vec![usage(Resource::Fd(2), AccessMode::Write)],
        )),
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(a), Value::U64(b)) => Action::Pure(Value::U64(a + b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = rt.run_blocking(action);
    assert_eq!(v, Ok(Value::U64(30)));
    assert_eq!(rt.undo_stack().len(), 2, "左右各压入一个 undo");

    drive(rt.recover());
    assert_eq!(
        *undo_log.lock().unwrap(),
        vec!["undo(write:2)".to_string(), "undo(write:1)".to_string()],
        "LIFO：right 的 undo 先弹出，left 的后（观察序一致）"
    );
    assert!(rt.undo_stack().is_empty(), "recover 后撤销栈清空");
}

/// F1 修复（blocker）：Fork 并行路径**两分支都分配新 fd** 的真实碰撞测试。
/// 左分支 Open、右分支 PipeOpen（MockExecutor 对两者都分配 fd）—— 修复前两
/// 分支同源于父 `next_fd`，会得到**相同 fd**（merge 时 `HashMap::extend` 静默
/// 覆盖丢弃左分支句柄，executor 内部轮换映射同样碰撞）；修复后右分支经
/// `offset_next_fd(1<<48)` 预分割 fd 区间，merge 后两 fd 不同且都能 lookup
/// 到各自句柄，父 `next_fd` 归一化后继续分配不冲突。
#[test]
fn fork_parallel_both_branches_allocate_fds_disjoint() {
    let ex = MockExecutor::new();
    let got = Arc::new(Mutex::new(None::<(u64, u64)>));
    let got2 = Arc::clone(&got);
    let mut rt = Runtime::new(Box::new(ex));

    // 异资源（Path /left 与 /right-pipe）→ can_parallel=true → 真并行路径。
    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/left"),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path("/left".to_string()), AccessMode::Read)],
        )),
        right: Box::new(syscall_step(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![usage(
                Resource::Path("/right-pipe".to_string()),
                AccessMode::Read,
            )],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::Fd(lfd), Value::Fd(rfd)) => {
                *got2.lock().unwrap() = Some((lfd, rfd));
                Action::Pure(Value::Unit)
            }
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = rt.run_blocking(action);
    assert_eq!(v, Ok(Value::Unit), "Fork 并行执行成功");
    let (lfd, rfd) = got.lock().unwrap().expect("combine 捕获两分支 fd");
    assert_ne!(lfd, rfd, "两分支分配的 fd 不得相同（F1 fd 区间预分割）");
    assert!(
        rt.registry().lookup(lfd).is_some(),
        "左分支句柄经 merge 以原 fd 可见（不得被右分支覆盖丢弃）"
    );
    assert!(
        rt.registry().lookup(rfd).is_some(),
        "右分支句柄经 merge 以原 fd 可见"
    );
    // 合并后父 next_fd = max 归一化（D1 单调）：继续分配不冲突、不复用。
    let nfd = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(
        nfd > lfd && nfd > rfd,
        "合并后父 next_fd 归一化，新分配不冲突"
    );
}

/// F2 修复（high）：冲突型 Fork（同资源 Write×Write → can_parallel=false →
/// 顺序路径）执行后**同样 merge 回父** —— 分支的线性标记（Write 消费）经 merge
/// 保留，父级对该资源再次 Write 被公理 A4 拒绝（修复前 l_reg/r_reg 被丢弃，
/// 父级同资源 Write 被错误放行）。
#[test]
fn fork_conflict_merge_keeps_linear_marks() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("write:1", MockOutcome::Value(Value::U64(10)));

    let w = usage(Resource::Fd(1), AccessMode::Write);
    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            vec![w.clone()],
        )),
        right: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xBB],
            },
            vec![w.clone()],
        )),
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(a), Value::U64(b)) => Action::Pure(Value::U64(a + b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(20)), "冲突 Fork 顺序执行，combine 正确");
    assert_eq!(ex.ops(), vec!["write:1", "write:1"], "left→right 顺序执行");

    // 分支的 Write 消费已随 merge 并入父：父级对该资源再次 Write 被拒（F2）
    assert_eq!(
        reg.check_linear(&w),
        Err(SysError::InvalidInput),
        "冲突型 Fork 后父级同资源 Write 应被 A4 拒绝（线性标记经 merge 保留）"
    );
}

/// F1 修复在顺序路径的落地：冲突型 Fork（顺序执行）两分支都 Open 时，右分支
/// 同样经 fd 区间预分割 —— merge 后两分支 fd 不同且都能 lookup（修复前顺序
/// 路径丢弃 l_reg/r_reg，本测试同时覆盖 F2 的 merge 行为）。
#[test]
fn fork_sequential_both_branches_allocate_fds_disjoint() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();

    let got = Arc::new(Mutex::new(None::<(u64, u64)>));
    let got2 = Arc::clone(&got);
    // Direct 通道（interpret）：Fork 恒走顺序路径（D14 阶段 1），与冲突无关。
    let action = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/seq-l"),
                flags: OpenFlags::default(),
            },
            vec![usage(
                Resource::Path("/seq-l".to_string()),
                AccessMode::Read,
            )],
        )),
        right: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/seq-r"),
                flags: OpenFlags::default(),
            },
            vec![usage(
                Resource::Path("/seq-r".to_string()),
                AccessMode::Read,
            )],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::Fd(lfd), Value::Fd(rfd)) => {
                *got2.lock().unwrap() = Some((lfd, rfd));
                Action::Pure(Value::Unit)
            }
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Unit), "顺序 Fork 执行成功");
    let (lfd, rfd) = got.lock().unwrap().expect("combine 捕获两分支 fd");
    assert_ne!(
        lfd, rfd,
        "顺序路径两分支分配的 fd 不得相同（F1 区间预分割）"
    );
    assert!(reg.lookup(lfd).is_some(), "左分支句柄经 merge 以原 fd 可见");
    assert!(reg.lookup(rfd).is_some(), "右分支句柄经 merge 以原 fd 可见");
}

// ── 15. 嵌套 Fork fd 全局唯一区间（S6/A2 HIGH 修复）+ 顺序错误路径 ────────

/// 构造嵌套 Fork 蓝图 `fork!( fork!(open(p1), open(p2)), open(p3) )`：三层
/// 资源互不相交 → 两层 can_parallel 均 true（并行路径）/ Direct 恒顺序（顺序
/// 路径）。内层 combine 记录 p1/p2 的 fd 并返回 List，外层 combine 记录 p3 的
/// fd；`got` 最终收集 [p1, p2, p3] 三个 fd。
fn nested_fork_action(p1: &str, p2: &str, p3: &str, got: Arc<Mutex<Vec<u64>>>) -> Action {
    let got_inner = Arc::clone(&got);
    let inner_fork = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from(p1),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path(p1.to_string()), AccessMode::Read)],
        )),
        right: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from(p2),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path(p2.to_string()), AccessMode::Read)],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::Fd(a), Value::Fd(b)) => {
                let mut g = got_inner.lock().unwrap();
                g.push(a);
                g.push(b);
                Action::Pure(Value::List(vec![Value::Fd(a), Value::Fd(b)]))
            }
            _ => Action::Pure(Value::Unit),
        }),
    };
    let got_outer = Arc::clone(&got);
    Action::Fork {
        left: Box::new(inner_fork),
        right: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from(p3),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path(p3.to_string()), AccessMode::Read)],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::List(_), Value::Fd(c)) => {
                got_outer.lock().unwrap().push(c);
                Action::Pure(Value::Unit)
            }
            _ => Action::Pure(Value::Unit),
        }),
    }
}

/// 断言 fd 列表两两不同，且每个 fd 在 registry 中可 lookup（句柄未被覆盖丢弃）。
fn assert_fds_disjoint_lookupable(fds: &[u64], reg: &ResourceRegistry) {
    let mut uniq = std::collections::HashSet::new();
    for fd in fds {
        assert!(uniq.insert(*fd), "fd 重复: {fd}，全部: {fds:?}");
        assert!(
            reg.lookup(*fd).is_some(),
            "fd {fd} 句柄不可 lookup（覆盖丢弃）"
        );
    }
    assert_eq!(uniq.len(), fds.len(), "fd 集合与列表一致");
}

/// 经 Runtime 读回 fd 内容（executor 的 fd→路径 记录为内容源）。
fn read_back_rt(rt: &mut Runtime, fd: u64) -> Vec<u8> {
    let action = Action::Sequential {
        current: Box::new(syscall_step(
            DataOp::Read { fd, len: 64 },
            vec![usage(Resource::Fd(fd), AccessMode::Read)],
        )),
        next: Box::new(|v| match v {
            Value::Bytes(b) => Action::Pure(Value::Bytes(b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    match rt.run_blocking(action) {
        Ok(Value::Bytes(b)) => b,
        other => panic!("fd {fd} 读回失败: {other:?}"),
    }
}

/// 经 Direct 通道读回 fd 内容（顺序路径版本）。
fn read_back_direct(ex: &mut MockExecutor, reg: &mut ResourceRegistry, fd: u64) -> Vec<u8> {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let action = Action::Sequential {
        current: Box::new(syscall_step(
            DataOp::Read { fd, len: 64 },
            vec![usage(Resource::Fd(fd), AccessMode::Read)],
        )),
        next: Box::new(|v| match v {
            Value::Bytes(b) => Action::Pure(Value::Bytes(b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    match drive(interpret(action, &mut ctx, &mut undo, reg, ex)) {
        Ok(Value::Bytes(b)) => b,
        other => panic!("fd {fd} 读回失败: {other:?}"),
    }
}

/// HIGH（嵌套 Fork 碰撞）并行路径回归：`fork!(fork!(open,open), open)` 全不相交
/// 资源 → 两层 can_parallel 均 true → 内外层右分支**并发**执行。批 5 修复下内层
/// 右分支从「左分支当前 next_fd（未分配仍为 N）」+2^48 起分配 → 与外层右分支
/// （N+2^48 起）分配**相同 fd** → executor fd→路径 映射互相覆盖 + merge 后写覆盖
/// 先写（句柄静默丢失）。修复后：三个 fd 两两不同、父 registry 三句柄均可
/// lookup、读回内容指向正确文件（映射未被覆盖）。
#[test]
fn fork_nested_parallel_fds_disjoint_and_readback() {
    let ex = MockExecutor::new();
    let path_log = ex.path_log.clone();
    let got = Arc::new(Mutex::new(Vec::<u64>::new()));
    let mut rt = Runtime::new(Box::new(ex));

    let action = nested_fork_action("/n-a", "/n-b", "/o-c", Arc::clone(&got));
    let v = rt.run_blocking(action);
    assert_eq!(v, Ok(Value::Unit), "嵌套 Fork 并行执行成功");

    let mut fds = got.lock().unwrap().clone();
    assert_eq!(fds.len(), 3, "combine 捕获三个分支 fd");
    fds.sort_unstable();
    assert_fds_disjoint_lookupable(&fds, rt.registry());

    // executor 的 fd→路径 记录：三个 Open 各占唯一 fd（无覆盖）。
    {
        let log = path_log.lock().unwrap();
        assert_eq!(log.len(), 3, "executor 记录三个唯一 fd→路径 映射（无覆盖）");
        for fd in &fds {
            assert!(
                log.contains_key(fd),
                "combine 的 fd {fd} 与 executor 记录一致"
            );
        }
    }
    // 读回内容验证映射未被覆盖：每 fd 读到对应路径字节。
    let entries: Vec<(u64, PathBuf)> = {
        let log = path_log.lock().unwrap();
        log.iter().map(|(fd, p)| (*fd, p.clone())).collect()
    };
    for (fd, path) in entries {
        let content = read_back_rt(&mut rt, fd);
        assert_eq!(
            String::from_utf8_lossy(&content),
            path.to_string_lossy(),
            "fd {fd} 读回内容应指向 {path:?}（映射未被覆盖）"
        );
    }
}

/// HIGH 嵌套修复的顺序路径回归：Direct 通道（interpret）Fork 恒顺序执行，嵌套
/// 左分支内的右分支与外层右分支同样不能撞 fd（批 5 顺序路径同样相对偏移）。
#[test]
fn fork_nested_sequential_fds_disjoint_and_readback() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    let path_log = ex.path_log.clone();
    let got = Arc::new(Mutex::new(Vec::<u64>::new()));

    let action = nested_fork_action("/s-a", "/s-b", "/s-c", Arc::clone(&got));
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Unit), "嵌套 Fork 顺序执行成功");

    let mut fds = got.lock().unwrap().clone();
    assert_eq!(fds.len(), 3, "combine 捕获三个分支 fd");
    fds.sort_unstable();
    assert_fds_disjoint_lookupable(&fds, &reg);

    {
        let log = path_log.lock().unwrap();
        assert_eq!(log.len(), 3, "executor 记录三个唯一 fd→路径 映射（无覆盖）");
    }
    let entries: Vec<(u64, PathBuf)> = {
        let log = path_log.lock().unwrap();
        log.iter().map(|(fd, p)| (*fd, p.clone())).collect()
    };
    for (fd, path) in entries {
        let content = read_back_direct(&mut ex, &mut reg, fd);
        assert_eq!(
            String::from_utf8_lossy(&content),
            path.to_string_lossy(),
            "fd {fd} 读回内容应指向 {path:?}（映射未被覆盖）"
        );
    }
}

/// 深层嵌套（3 层）冒烟：`fork!( fork!( fork!(open,open), open ), open )`
/// 并行路径，四个 fd 两两不同且全部可 lookup。
#[test]
fn fork_nested_depth3_smoke() {
    let ex = MockExecutor::new();
    let got = Arc::new(Mutex::new(Vec::<u64>::new()));
    let mut rt = Runtime::new(Box::new(ex));

    let got_ii = Arc::clone(&got);
    let inner_inner = Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/d1"),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path("/d1".to_string()), AccessMode::Read)],
        )),
        right: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/d2"),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path("/d2".to_string()), AccessMode::Read)],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::Fd(a), Value::Fd(b)) => {
                let mut g = got_ii.lock().unwrap();
                g.push(a);
                g.push(b);
                Action::Pure(Value::List(vec![Value::Fd(a), Value::Fd(b)]))
            }
            _ => Action::Pure(Value::Unit),
        }),
    };
    let got_mid = Arc::clone(&got);
    let middle = Action::Fork {
        left: Box::new(inner_inner),
        right: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/d3"),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path("/d3".to_string()), AccessMode::Read)],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::List(mut vs), Value::Fd(c)) => {
                got_mid.lock().unwrap().push(c);
                vs.push(Value::Fd(c));
                Action::Pure(Value::List(vs))
            }
            _ => Action::Pure(Value::Unit),
        }),
    };
    let got_outer = Arc::clone(&got);
    let action = Action::Fork {
        left: Box::new(middle),
        right: Box::new(syscall_step(
            DataOp::Open {
                path: PathBuf::from("/d4"),
                flags: OpenFlags::default(),
            },
            vec![usage(Resource::Path("/d4".to_string()), AccessMode::Read)],
        )),
        combine: Box::new(move |l, r| match (l, r) {
            (Value::List(_), Value::Fd(c)) => {
                got_outer.lock().unwrap().push(c);
                Action::Pure(Value::Unit)
            }
            _ => Action::Pure(Value::Unit),
        }),
    };

    let v = rt.run_blocking(action);
    assert_eq!(v, Ok(Value::Unit), "3 层嵌套 Fork 执行成功");

    let mut fds = got.lock().unwrap().clone();
    assert_eq!(fds.len(), 4, "combine 捕获四个 fd");
    fds.sort_unstable();
    assert_fds_disjoint_lookupable(&fds, rt.registry());
}

/// 审查 MEDIUM-2：顺序 Fork 左分支 Err → 右分支仍执行（op 记录）、merge 发生
/// （两分支 Write 线性标记并入父）、错误传播；recover 按 right→left 撤销
/// （LIFO：右分支效果后发生、先撤销）。
#[test]
fn fork_sequential_left_error_right_still_executes_and_merges() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("write:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:3", MockOutcome::Err(SysError::NotFound));
    ex.respond("write:2", MockOutcome::Value(Value::U64(20)));
    let undo_log = Arc::clone(&ex.undo_log);

    // 左分支：先 Ok（压 undo）再 Err（传播）；右分支：Ok（压 undo）。
    // Direct 通道（interpret）→ 恒顺序路径，与冲突无关。
    let left = Action::Sequential {
        current: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0x01],
            },
            vec![usage(Resource::Fd(1), AccessMode::Write)],
        )),
        next: Box::new(|_| {
            syscall_step(
                DataOp::Write {
                    fd: 3,
                    data: vec![0x03],
                },
                vec![usage(Resource::Fd(3), AccessMode::Write)],
            )
        }),
    };
    let right = syscall_step(
        DataOp::Write {
            fd: 2,
            data: vec![0x02],
        },
        vec![usage(Resource::Fd(2), AccessMode::Write)],
    );
    let action = Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    };

    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Err(SysError::NotFound), "左分支 Err 传播");
    assert_eq!(
        ex.ops(),
        vec!["write:1", "write:3", "write:2"],
        "右分支在左分支 Err 后仍执行（left→right 顺序）"
    );
    // merge 发生：两分支的线性标记并入父 —— 成功的 Write 消费并入
    // （fd 1/2）；左分支**失败**的 Write（fd 3，NotFound）标记已回滚
    // （RFC-12：失败 syscall 预插入的消费标记不残留），不并入父。
    for fd in [1u64, 2] {
        assert_eq!(
            reg.check_linear(&usage(Resource::Fd(fd), AccessMode::Write)),
            Err(SysError::InvalidInput),
            "分支 fd {fd} 的成功 Write 消费经 merge 并入父"
        );
    }
    assert_eq!(
        reg.check_linear(&usage(Resource::Fd(3), AccessMode::Write)),
        Ok(()),
        "左分支失败的 Write（fd 3）标记已回滚，经 merge 后父侧未消费"
    );
    // recover 按 right→left 撤销（LIFO：右分支效果后发生、先撤销）。
    drive(undo.recover());
    assert_eq!(
        *undo_log.lock().unwrap(),
        vec!["undo(write:2)".to_string(), "undo(write:1)".to_string()],
        "recover 先撤销右分支再左分支"
    );
}

// ── 16. 嵌套 Fork 通用属性（proptest，CTO 追加防复发）──────────────────────

/// Fork 树形状（属性测试用）：叶 = 单个 Open Syscall；内部节点 = 左右子形状。
#[derive(Debug, Clone)]
enum TreeShape {
    Leaf,
    Fork(Box<TreeShape>, Box<TreeShape>),
}

/// 随机 Fork 树形状策略：Fork 嵌套深度 ∈ [1, max_depth]（调用方传 max_depth+1
/// 允许最深 max_depth 层分叉），每层 70% 概率分叉（偏深覆盖）、30% 叶。
fn tree_shape_strategy(max_depth: u32) -> BoxedStrategy<TreeShape> {
    (0..10u32)
        .prop_flat_map(move |roll| {
            if roll < 7 && max_depth > 1 {
                (
                    tree_shape_strategy(max_depth - 1),
                    tree_shape_strategy(max_depth - 1),
                )
                    .prop_map(|(l, r)| TreeShape::Fork(Box::new(l), Box::new(r)))
                    .boxed()
            } else {
                Just(TreeShape::Leaf).boxed()
            }
        })
        .boxed()
}

/// 形状 → Action：DFS 分配唯一路径（/p0, /p1, …，全树叶路径互异 → 所有 Fork
/// 均可并行），combine 逐层 List 拼接。
fn shape_to_action(shape: &TreeShape, next_id: &mut usize) -> Action {
    match shape {
        TreeShape::Leaf => {
            let path = format!("/p{next_id}");
            *next_id += 1;
            syscall_step(
                DataOp::Open {
                    path: PathBuf::from(&path),
                    flags: OpenFlags::default(),
                },
                vec![usage(Resource::Path(path), AccessMode::Read)],
            )
        }
        TreeShape::Fork(l, r) => Action::Fork {
            left: Box::new(shape_to_action(l, next_id)),
            right: Box::new(shape_to_action(r, next_id)),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        },
    }
}

/// 属性断言：fd 列表两两不相交，且合并后父 registry 中每个 fd 均可 lookup
/// （句柄未被覆盖丢弃）。
fn check_fds_disjoint_lookupable(fds: &[u64], reg: &ResourceRegistry) -> Result<(), TestCaseError> {
    let mut seen = std::collections::HashSet::new();
    for fd in fds {
        prop_assert!(seen.insert(*fd), "fd 重复: {fd}，全部: {fds:?}");
        prop_assert!(
            reg.lookup(*fd).is_some(),
            "fd {fd} 句柄不可 lookup（覆盖丢弃），全部: {fds:?}"
        );
    }
    Ok(())
}

// S6/A2 通用属性（CTO 追加，防复发）：随机任意形状的 Fork 树（Fork 嵌套深度
// 1..=4，叶为分配 fd 的 Open），顺序路径（Direct/interpret）与并行路径
// （Runtime/Shared）各跑一遍，断言整棵树所有分支分配的 fd **两两不相交**
// （合并后父 registry 无重复 fd、每 fd 可 lookup 到独立句柄）。根因级终结
// 「相对偏移」类区间缺陷复发（fd 区间类缺陷第三次出现）。
proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]
    #[test]
    fn fork_tree_all_fds_pairwise_disjoint(
        shape in tree_shape_strategy(5),
    ) {
        // 顺序路径（Direct/interpret）：merge 回父后 fd 无重复、可 lookup。
        {
            let mut next_id = 0usize;
            let action = shape_to_action(&shape, &mut next_id);
            let leaf_count = next_id;
            prop_assert!(leaf_count >= 1, "树至少一个叶");

            let mut ctx = Context::new();
            let mut undo = UndoStack::new();
            let mut reg = ResourceRegistry::new();
            let mut ex = MockExecutor::new();
            let alloc_log = ex.alloc_log.clone();
            let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
            prop_assert!(v.is_ok(), "顺序路径执行失败: {v:?}");
            let fds = alloc_log.lock().unwrap().clone();
            prop_assert_eq!(fds.len(), leaf_count, "每个叶 Open 恰好分配一个 fd");
            check_fds_disjoint_lookupable(&fds, &reg)?;
        }

        // 并行路径（Runtime/Shared）：并发分支（含嵌套）同样互斥。
        {
            let mut next_id = 0usize;
            let action = shape_to_action(&shape, &mut next_id);
            let leaf_count = next_id;

            let ex = MockExecutor::new();
            let alloc_log = ex.alloc_log.clone();
            let mut rt = Runtime::new(Box::new(ex));
            let v = rt.run_blocking(action);
            prop_assert!(v.is_ok(), "并行路径执行失败: {v:?}");
            let fds = alloc_log.lock().unwrap().clone();
            prop_assert_eq!(fds.len(), leaf_count, "每个叶 Open 恰好分配一个 fd");
            check_fds_disjoint_lookupable(&fds, rt.registry())?;
        }
    }
}

/// D10 对齐：Replace 分支先 recover 再 `reg.clear()` —— handles 与线性标记
/// 全部释放（next_fd 保留 D1 单调），随后执行 target。
#[test]
fn replace_clears_registry() {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("gettime", MockOutcome::Value(Value::U64(1)));

    let undo_log = Arc::clone(&ex.undo_log);
    let mut rt = Runtime::new(Box::new(ex));

    // Replace 前 registry 积累：1 个句柄 + 1 条 Write 消费。
    let fd0 = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(rt
        .registry()
        .check_linear(&usage(Resource::Fd(fd0), AccessMode::Write))
        .is_ok());

    let action = Action::Sequential {
        current: Box::new(syscall_step(DataOp::GetTime, vec![])),
        next: Box::new(|_| Action::Replace {
            target: Box::new(Action::Pure(Value::U64(42))),
        }),
    };
    let v = rt.run_blocking(action);
    assert_eq!(v, Ok(Value::U64(42)), "Replace 后执行 target 的结果");
    assert_eq!(
        *undo_log.lock().unwrap(),
        vec!["undo(gettime)".to_string()],
        "recover 先执行（LIFO 撤销累积逆操作）"
    );
    assert!(rt.undo_stack().is_empty(), "recover 后撤销栈清空");

    // reg.clear()：handles 清空 + 线性复位（next_fd 保留 D1）。
    assert!(
        rt.registry().lookup(fd0).is_none(),
        "Replace 后句柄应全部释放"
    );
    assert!(
        rt.registry()
            .check_linear(&usage(Resource::Fd(fd0), AccessMode::Write))
            .is_ok(),
        "clear() 后同资源应可再次 Write（线性复位）"
    );
    let nfd = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(nfd > fd0, "fd 永不复用（决策 D1）");
}

// ══════════════════════════════════════════════════════════════════════
// RFC-11 修复回归（A2 批 7）：解释器递归深度守卫。
//
// R4c 对抗发现 HIGH：`run_sub_impl` → `interpret_impl` 每层递归 ~13-20KB
// 栈帧（debug），Windows 默认 2MB 测试线程栈下实测崩溃边界深度 ~104-108
// （100/104 通过、108 即 STATUS_STACK_OVERFLOW 进程级 abort；R4c 审计记录
// ~110-120 同量级），不受信任蓝图 ~百层嵌套即可使宿主进程崩溃（拒绝服务面）。
// 修复：interpret_impl 递归入口维护嵌套深度计数器，超阈值（96，比实测边界
// 留 ~8% 余量）返回 `SysError::Other(105)`（ENOBUFS「嵌套资源耗尽」语义）
// 替代栈溢出 —— 守卫在栈溢出**之前**触发，错误可被外层 Catch 捕获。
//
// 以下 3 个测试覆盖：安全深度 64 不受影响；超深 200 返回可捕获错误且进程
// 不 abort（测试能跑完即证明）；超深蓝图外包 Catch → handler 收到 Other(105)。
// ══════════════════════════════════════════════════════════════════════

/// 深度 depth 的嵌套 Sequential：current 为下一层；叶子返回 U64(300)，每层
/// next 原样上抛（值保真）。与 `adversarial_r4c.rs::nested_seq` 同构。
fn nested_seq_chain(depth: u64) -> Action {
    if depth == 0 {
        return Action::Pure(Value::U64(300));
    }
    Action::Sequential {
        current: Box::new(nested_seq_chain(depth - 1)),
        next: Box::new(Action::Pure),
    }
}

/// RFC-11 a：安全深度 64 正常执行（与 R4c 固定安全深度一致，远低于守卫阈值
/// 96 与实测崩溃边界 ~104）——守卫不误伤合法嵌套。
#[test]
fn deep_nesting_under_limit_ok() {
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let v = rt.run_blocking(nested_seq_chain(64));
    assert_eq!(
        v,
        Ok(Value::U64(300)),
        "64 层嵌套应在守卫阈值（96）之下正常执行，收到 {v:?}"
    );
    assert!(rt.undo_stack().is_empty());
}

/// RFC-11 b：深度 200（Windows debug 会溢出的量级）→ 守卫在栈溢出前触发，
/// 返回 `Err(Other(105))` 且进程不 abort —— 测试本身能跑完即证明未发生
/// STATUS_STACK_OVERFLOW。
#[test]
fn deep_nesting_over_limit_returns_error() {
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let v = rt.run_blocking(nested_seq_chain(200));
    assert_eq!(
        v,
        Err(SysError::Other(105)),
        "超深嵌套应返回深度守卫错误（ENOBUFS=105 语义近似），收到 {v:?}"
    );
}

/// RFC-11 c：超深蓝图外包 Catch → 守卫错误可被捕获（拒绝服务面转为可恢复
/// 错误）：handler 收到 Other(105) 并执行。
#[test]
fn deep_nesting_catchable() {
    let mut rt = Runtime::new(Box::new(MockExecutor::new()));
    let action = Action::Catch {
        action: Box::new(nested_seq_chain(200)),
        handler: Box::new(|e| Action::Pure(Value::Str(format!("handled:{e}")))),
    };
    let v = rt.run_blocking(action);
    assert_eq!(
        v,
        Ok(Value::Str("handled:Other(105)".to_string())),
        "Catch 应捕获深度守卫错误并执行 handler，收到 {v:?}"
    );
}
