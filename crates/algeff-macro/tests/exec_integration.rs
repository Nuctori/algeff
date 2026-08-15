//! A4 批4：宏 × 解释器执行级集成测试（pdr.md §八「宏仅语法糖」的端到端验证）。
//!
//! 目标：证明 plan!/fork!/scope!/choose! 的展开产物是真实可执行的 `Action`，
//! 能被 A2 解释器 `interpret` 驱动到最终值 —— 而非仅 AST 形状断言
//! （tests/macros.rs 的互补面）。
//!
//! 说明：
//! - `interpret` 的 future 因冻结签名 `&mut dyn SyscallExecutor`（trait 无 `Send`
//!   超 trait）而**非 Send**；`Runtime::new` 自持 tokio reactor（D9），在 tokio
//!   上下文内构造会 panic。故全部测试用普通 `#[test]` + 本地 current-thread
//!   runtime 驱动（`drive`，沿用 algeff-core/tests/interpreter.rs 的 MockExecutor
//!   思路，自建不依赖其内部）。
//! - MockExecutor：记录 op 调用序列，按 op 描述返回可配置 Value/Err。
//! - 宏语义冻结（crates/algeff-macro/src/lib.rs 只读）：plan! 末元素 next 收敛为
//!   `Pure(Unit)`；fork! combine 固定收敛 `Pure(Unit)`；choose! cond 为
//!   `move |_|`（忽略解释器当前值，值依赖经 next 闭包捕获流入）；scope! inner
//!   为闭包调用。本文件断言与冻结语义对齐。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use algeff_core::runtime::interpret;
use algeff_core::syscall::BoxFuture;
use algeff_core::prelude::*;
use algeff_macro::{choose, fork, plan, scope};

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

/// 可配置 Mock 执行器：记录 op 调用序列，可按 op 描述返回 Value/Err。
#[derive(Default)]
struct MockExecutor {
    /// 每次 execute 的 op 描述（调用顺序）。
    log: Arc<Mutex<Vec<String>>>,
    /// op 描述 → 返回结果（未配置 → Ok(Value::Unit)）。
    responses: HashMap<String, MockOutcome>,
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
            match self.responses.get(&desc).cloned() {
                Some(MockOutcome::Err(e)) => Err(e),
                Some(MockOutcome::Value(v)) => Ok((v, None)),
                None => Ok((Value::Unit, None)),
            }
        })
    }
}

/// 无资源、返回自身结果的单步 Syscall（next = 恒等 Pure）。
fn syscall_step(op: DataOp) -> Action {
    Action::Syscall {
        op,
        resources: vec![],
        next: Box::new(Action::Pure),
    }
}

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage { resource: r, mode: m }
}

// ── 1. plan! 纯链可执行 ───────────────────────────────────────────────

#[test]
fn test_plan_runs() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();

    let action = plan! {
        Action::Pure(Value::U64(1));
        Action::Pure(Value::U64(2));
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    // plan! 语义：元素逐个装箱为 Sequential，末元素 next 收敛为 Pure(Unit)
    // （宏语义冻结），故最终值 = Unit；纯链不触发任何 syscall。
    assert_eq!(v, Ok(Value::Unit));
    assert!(ex.ops().is_empty(), "纯链不应产生 op，got {:?}", ex.ops());

    // 执行性证明：首元素为 Alloc（纯节点），其 next 在 interpret 期间被调用，
    // 证明解释器真实遍历了宏展开出的 Sequential 链（而非仅构造 Action 值）。
    let seen = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let action = plan! {
        Action::Alloc {
            len: 4,
            next: Box::new(move |v| {
                *seen2.lock().unwrap() = Some(v);
                Action::Pure(Value::Unit)
            }),
        };
        Action::Pure(Value::U64(9));
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Unit));
    assert_eq!(*seen.lock().unwrap(), Some(Value::Bytes(vec![0u8; 4])));
    assert!(ex.ops().is_empty());

    // 错误传播：plan! 链中 syscall 返回 Err → interpret 向上传播（宏产物执行语义）。
    // 同时验证 MockExecutor 可配置 Err 响应。
    ex.respond("gettime", MockOutcome::Err(SysError::NotFound));
    let action = plan! { syscall_step(DataOp::GetTime) };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Err(SysError::NotFound));
    assert_eq!(ex.ops(), vec!["gettime"]);
}

// ── 2. choose! 执行级分支隔离 ────────────────────────────────────────

#[test]
fn test_choose_executes_branch() {
    // cond 为 true：只执行 then 分支的 op（A5 分支隔离，执行级验证）
    {
        let mut ctx = Context::new();
        let mut undo = UndoStack::new();
        let mut reg = ResourceRegistry::new();
        let mut ex = MockExecutor::new();
        ex.respond("gettime", MockOutcome::Value(Value::U64(5)));
        ex.respond("read:7:4", MockOutcome::Value(Value::Bool(true)));

        let action = choose!(
            true,
            then: plan! { syscall_step(DataOp::GetTime) },
            else: plan! { syscall_step(DataOp::Read { fd: 7, len: 4 }) },
        );
        let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
        assert_eq!(v, Ok(Value::Unit)); // plan! 分支末 next 收敛 Pure(Unit)
        assert_eq!(ex.ops(), vec!["gettime"]); // 仅 then 分支的 op
    }

    // cond 为 false：只执行 else 分支的 op（对称断言，强化分支隔离）
    {
        let mut ctx = Context::new();
        let mut undo = UndoStack::new();
        let mut reg = ResourceRegistry::new();
        let mut ex = MockExecutor::new();
        ex.respond("gettime", MockOutcome::Value(Value::U64(5)));
        ex.respond("read:7:4", MockOutcome::Value(Value::Bool(true)));

        let action = choose!(
            false,
            then: plan! { syscall_step(DataOp::GetTime) },
            else: plan! { syscall_step(DataOp::Read { fd: 7, len: 4 }) },
        );
        let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
        assert_eq!(v, Ok(Value::Unit));
        assert_eq!(ex.ops(), vec!["read:7:4"]); // 仅 else 分支的 op
    }
}

// ── 3. fork! combine 收敛（D14：静态检测 + 顺序执行 left→right）──────

#[test]
fn test_fork_combine_merges() {
    // D14 前置：异资源 → can_parallel = true（可并行，但不强制并行）
    let l_set = vec![usage(Resource::Fd(1), AccessMode::Write)];
    let r_set = vec![usage(Resource::Fd(2), AccessMode::Write)];
    assert!(ResourceRegistry::new().can_parallel(&l_set, &r_set));

    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("write:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:2", MockOutcome::Value(Value::U64(20)));

    let action = fork! {
        left: plan! {
            Action::Syscall {
                op: DataOp::Write { fd: 1, data: vec![0xAA] },
                resources: l_set,
                next: Box::new(Action::Pure),
            }
        },
        right: plan! {
            Action::Syscall {
                op: DataOp::Write { fd: 2, data: vec![0xBB] },
                resources: r_set,
                next: Box::new(Action::Pure),
            }
        },
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    // fork! combine 固定收敛为 Pure(Unit)（宏语义冻结）：两分支结果经 combine
    // 汇入主循环并正常终止；op 均执行且顺序 left→right（D14 顺序执行）。
    assert_eq!(v, Ok(Value::Unit));
    assert_eq!(ex.ops(), vec!["write:1", "write:2"]);
}

// ── 4. scope! cwd 恢复 ───────────────────────────────────────────────

#[test]
fn test_scope_restores_cwd() {
    let mut ctx = Context::new();
    ctx.cwd = PathBuf::from("/app");
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("gettime", MockOutcome::Value(Value::U64(7)));

    let action = scope!("/tmp", || plan! {
        syscall_step(DataOp::GetTime)
    });
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::Unit));
    assert_eq!(ctx.cwd, PathBuf::from("/app")); // 执行后 cwd 恢复原值
    assert_eq!(ex.ops(), vec!["gettime"]); // inner 确实在 scope 内执行
}

// ── 5. choose! cond 依赖运行时当前值 ─────────────────────────────────

#[test]
fn test_choose_cond_value() {
    // 用 Sequential 先产出一个 Value，再由 next 闭包基于该值构建 choose!。
    // choose! 的 cond 为宏生成的 `move |_|` 闭包（冻结语义），值依赖经捕获流入，
    // 使 choose 决策在 interpret 执行期作出 —— 验证「当前值 → 分支」的执行链路。
    fn choose_from(v: Value) -> Action {
        let is_seven = matches!(v, Value::U64(7));
        choose!(
            is_seven,
            then: plan! { syscall_step(DataOp::GetTime) },
            else: plan! { syscall_step(DataOp::Read { fd: 7, len: 4 }) },
        )
    }

    // 方向 1：前序值 = 7 → cond true → then 分支
    {
        let mut ctx = Context::new();
        let mut undo = UndoStack::new();
        let mut reg = ResourceRegistry::new();
        let mut ex = MockExecutor::new();
        ex.respond("gettime", MockOutcome::Value(Value::U64(5)));
        ex.respond("read:7:4", MockOutcome::Value(Value::Bool(true)));

        let action = Action::Sequential {
            current: Box::new(Action::Pure(Value::U64(7))),
            next: Box::new(choose_from),
        };
        let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
        assert_eq!(v, Ok(Value::Unit));
        assert_eq!(ex.ops(), vec!["gettime"]);
    }

    // 方向 2：前序值 = 8 → cond false → else 分支
    {
        let mut ctx = Context::new();
        let mut undo = UndoStack::new();
        let mut reg = ResourceRegistry::new();
        let mut ex = MockExecutor::new();
        ex.respond("gettime", MockOutcome::Value(Value::U64(5)));
        ex.respond("read:7:4", MockOutcome::Value(Value::Bool(true)));

        let action = Action::Sequential {
            current: Box::new(Action::Pure(Value::U64(8))),
            next: Box::new(choose_from),
        };
        let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
        assert_eq!(v, Ok(Value::Unit));
        assert_eq!(ex.ops(), vec!["read:7:4"]);
    }
}
