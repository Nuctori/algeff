//! 执行级公理验证（A6 Verification 批 3，pdr.md §七「验证方式」工程落地）。
//!
//! 前置：A2 的 `interpret`（trampoline 解释器，15 节点 + 17 测试）已合并。
//! 本文件以自建 MockExecutor（记录 op 序列、可配置返回值/undo/错误）驱动真实
//! 解释器，把公理从「静态/属性层」推进到「执行层」：
//!   - exec_A1_associativity：`(a;b);c` 与 `a;(b;c)` 的 op 调用序列 + 最终 Value 一致
//!     （A1 结合律执行等价，spec/verification-plan.md §1 A1 行）；
//!   - exec_A2_identity：`Pure(())` 前缀/后缀链与纯链 op 序列一致、前缀值等价、
//!     Pure 不产生 UndoOp（§1 A2 行「执行级」验收）；
//!   - exec_A4_linearity_runtime：同资源两次 Write 经 interpret，第二次在运行时
//!     `check_linear` 处返回 Err(InvalidInput)（§1 A4 行「执行级」验收）；
//!   - exec_A6_undo_roundtrip：Runtime 路径——interpret 后 undo 栈非空，recover 后
//!     逆序执行 + 栈清空（w;w̄=1 双态，§2 P4 行）；
//!   - exec_D10_replace_order：先 recover 再 target，undo 先执行且最终值为 target
//!     （contracts.md D10）；
//!   - exec_fork_conflict_static：Fork 左右同资源 Write → can_parallel=false →
//!     顺序执行 + combine 正确合并（contracts.md D14，§1 A3 动态调度行）。
//!
//! 命名约定：`exec_<公理/决策>_<要点>`，与 `spec/verification-plan.md` §1/§2 公理矩阵
//! 对齐（测试名含公理编号，属任务强制命名，故文件级允许 non_snake_case）。
//!
//! 工程约束：`interpret` 的 future 因冻结签名 `&mut dyn SyscallExecutor`（trait 无
//! `Send` 超 trait）而**非 Send**；`Runtime::new` 须在 tokio 上下文之外调用（D9）。
//! 因此全部测试用普通 `#[test]` + 本地 current-thread runtime 驱动（`drive`），
//! 与 `interpreter.rs` 同构。本文件独立新建，不依赖 interpreter.rs 内部实现。

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use algeff_core::action::{Action, DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{AccessMode, Resource, ResourceRegistry, ResourceUsage};
use algeff_core::runtime::{fork_conflict, interpret, Context, Runtime, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoOp};

/// 本地 current-thread runtime 驱动（interpret/recover future 非 Send，
/// 不能用多线程 block_on 直接驱动）。
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

/// 可配置 Mock 执行器：记录 op 调用序列，可按 op 描述返回 Value/Err/undo。
#[derive(Default)]
struct MockExecutor {
    /// 每次 execute 的 op 描述（调用顺序）。
    log: Arc<Mutex<Vec<String>>>,
    /// undo 执行记录（recover 顺序）。
    undo_log: Arc<Mutex<Vec<String>>>,
    /// op 描述 → 返回结果（未配置 → Ok(Value::Unit)）。
    responses: HashMap<String, MockOutcome>,
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
/// Write 附带 data 长度以区分同 fd 的多次写（Fork 冲突场景用）。
fn describe(op: &DataOp) -> String {
    match op {
        DataOp::Write { fd, data } => format!("write:{fd}:{}", data.len()),
        DataOp::Read { fd, len } => format!("read:{fd}:{len}"),
        DataOp::Close { fd: 999 } => "Close { fd: 999 }".to_string(),
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

// ── MockExecutor 错误配置：经解释器传播 ──────────────────────────────
// 执行器配置 Err → interpret 原样返回该错误（execute 错误传播路径，
// 佐证 MockExecutor 的「可配置错误」能力被真实使用）。

#[test]
fn exec_syscall_error_propagates() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("Close { fd: 999 }", MockOutcome::Err(SysError::NotFound));

    let v = drive(interpret(
        syscall_step(DataOp::Close { fd: 999 }, vec![]),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    assert_eq!(
        v,
        Err(SysError::NotFound),
        "执行器错误经 interpret 原样传播"
    );
    assert_eq!(ex.ops(), vec!["Close { fd: 999 }"], "错误在 execute 记录后返回");
}

// ── A1 结合律：执行等价 ────────────────────────────────────────────────
// (a;b);c 与 a;(b;c) 两种 Sequential 嵌套 → 解释器 op 调用序列一致 + 最终 Value 一致。

#[test]
fn exec_A1_associativity() {
    // 混合链 a;b;c：a=Syscall(GetTime)→10，b=Pure(U64(20))，c=Syscall(Read)→30；
    // c 的 next 把前一结果（U64(20)）与自身结果相加 → 最终 50。
    fn mk_a() -> Action {
        syscall_step(DataOp::Close { fd: 999 }, vec![])
    }
    fn mk_b() -> Action {
        Action::Pure(Value::U64(20))
    }
    fn mk_c(prev: Value) -> Action {
        Action::Syscall {
            op: DataOp::Read { fd: 2, len: 4 },
            resources: vec![usage(Resource::Fd(2), AccessMode::Read)],
            next: Box::new(move |vc| match (prev, vc) {
                (Value::U64(p), Value::U64(c)) => Action::Pure(Value::U64(p + c)),
                _ => Action::Pure(Value::Unit),
            }),
        }
    }
    fn cfg_ex() -> MockExecutor {
        let mut ex = MockExecutor::new();
        ex.respond("Close { fd: 999 }", MockOutcome::Value(Value::U64(10)));
        ex.respond("read:2:4", MockOutcome::Value(Value::U64(30)));
        ex
    }

    // (a;b);c：左结合嵌套
    let chain_left = Action::Sequential {
        current: Box::new(Action::Sequential {
            current: Box::new(mk_a()),
            next: Box::new(|_| mk_b()),
        }),
        next: Box::new(mk_c),
    };
    // a;(b;c)：右结合嵌套
    let chain_right = Action::Sequential {
        current: Box::new(mk_a()),
        next: Box::new(|_| Action::Sequential {
            current: Box::new(mk_b()),
            next: Box::new(mk_c),
        }),
    };

    let mut ctx1 = Context::new();
    let mut undo1 = UndoStack::new();
    let mut reg1 = ResourceRegistry::new();
    let mut ex1 = cfg_ex();
    let v1 = drive(interpret(
        chain_left, &mut ctx1, &mut undo1, &mut reg1, &mut ex1,
    ));

    let mut ctx2 = Context::new();
    let mut undo2 = UndoStack::new();
    let mut reg2 = ResourceRegistry::new();
    let mut ex2 = cfg_ex();
    let v2 = drive(interpret(
        chain_right,
        &mut ctx2,
        &mut undo2,
        &mut reg2,
        &mut ex2,
    ));

    assert_eq!(v1, v2, "(a;b);c 与 a;(b;c) 最终 Value 一致（A1 执行等价）");
    assert_eq!(v1, Ok(Value::U64(50)), "值流穿透两种嵌套（20+30）");
    assert_eq!(ex1.ops(), ex2.ops(), "(a;b);c 与 a;(b;c) op 调用序列一致");
    assert_eq!(ex1.ops(), vec!["Close { fd: 999 }", "read:2:4"]);
}

// ── A2 单位元：执行等价 ────────────────────────────────────────────────
// Pure(()) 开头/结尾的链与纯链 op 序列一致；前缀还要求值等价；Pure 不产生 UndoOp。

#[test]
fn exec_A2_identity() {
    // 纯链 a = GetTime;Read（两个 Syscall）
    fn mk_chain() -> Action {
        Action::Sequential {
            current: Box::new(syscall_step(DataOp::Close { fd: 999 }, vec![])),
            next: Box::new(|_| {
                syscall_step(
                    DataOp::Read { fd: 2, len: 4 },
                    vec![usage(Resource::Fd(2), AccessMode::Read)],
                )
            }),
        }
    }
    fn cfg_ex() -> MockExecutor {
        let mut ex = MockExecutor::new();
        ex.respond("Close { fd: 999 }", MockOutcome::Value(Value::U64(5)));
        ex.respond("read:2:4", MockOutcome::Value(Value::U64(6)));
        ex
    }

    // 纯链 a
    let mut ex_plain = cfg_ex();
    let v_plain = drive(interpret(
        mk_chain(),
        &mut Context::new(),
        &mut UndoStack::new(),
        &mut ResourceRegistry::new(),
        &mut ex_plain,
    ));
    assert_eq!(v_plain, Ok(Value::U64(6)));
    let plain_ops = ex_plain.ops();

    // Pure(()) 前缀：1;a —— 值等价 + op 序列一致
    let prefixed = Action::Sequential {
        current: Box::new(Action::Pure(Value::Unit)),
        next: Box::new(|_| mk_chain()),
    };
    let mut ex_pre = cfg_ex();
    let v_pre = drive(interpret(
        prefixed,
        &mut Context::new(),
        &mut UndoStack::new(),
        &mut ResourceRegistry::new(),
        &mut ex_pre,
    ));
    assert_eq!(v_pre, v_plain, "run(Pure(());a) 与 run(a) 值等价（单位元）");
    assert_eq!(ex_pre.ops(), plain_ops, "前缀单位元 op 序列一致");

    // Pure(()) 后缀：a;1 —— op 序列一致（CPS 语义下尾 Pure 以自身值 Unit 结束）
    let suffixed = Action::Sequential {
        current: Box::new(mk_chain()),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let mut ex_suf = cfg_ex();
    let v_suf = drive(interpret(
        suffixed,
        &mut Context::new(),
        &mut UndoStack::new(),
        &mut ResourceRegistry::new(),
        &mut ex_suf,
    ));
    assert_eq!(ex_suf.ops(), plain_ops, "后缀单位元 op 序列一致");
    assert_eq!(v_suf, Ok(Value::Unit), "尾 Pure(()) 以 Unit 结束");

    // 双侧包裹：1;a;1
    let both = Action::Sequential {
        current: Box::new(Action::Pure(Value::Unit)),
        next: Box::new(|_| Action::Sequential {
            current: Box::new(mk_chain()),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        }),
    };
    let mut ex_both = cfg_ex();
    let v_both = drive(interpret(
        both,
        &mut Context::new(),
        &mut UndoStack::new(),
        &mut ResourceRegistry::new(),
        &mut ex_both,
    ));
    assert_eq!(ex_both.ops(), plain_ops, "双侧包裹 op 序列一致");
    assert_eq!(v_both, Ok(Value::Unit));

    // Pure 不产生 UndoOp：纯 Pure 蓝图不触碰执行器、不压撤销栈
    let mut ex_pure = cfg_ex();
    ex_pure.with_undo = true;
    let v_pure = drive(interpret(
        Action::Pure(Value::Unit),
        &mut Context::new(),
        &mut UndoStack::new(),
        &mut ResourceRegistry::new(),
        &mut ex_pure,
    ));
    assert_eq!(v_pure, Ok(Value::Unit));
    assert!(ex_pure.ops().is_empty(), "Pure 不触发任何 op");
    assert!(ex_pure.undo_ops().is_empty(), "Pure 不产生 UndoOp");
}

// ── A4 资源线性：运行时断言 ────────────────────────────────────────────
// 同资源 Write 两次（两个 Sequential Syscall 共享同一 registry）→ 第二次 Err(InvalidInput)。

#[test]
fn exec_A4_linearity_runtime() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.respond("write:1:1", MockOutcome::Value(Value::U64(10)));

    let w = usage(Resource::Fd(1), AccessMode::Write);
    let action = Action::Sequential {
        current: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            vec![w.clone()],
        )),
        next: Box::new(move |_| {
            syscall_step(
                DataOp::Write {
                    fd: 1,
                    data: vec![0xBB],
                },
                vec![w],
            )
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(
        v,
        Err(SysError::InvalidInput),
        "第二次同资源 Write 在运行时被 A4 线性检查拒绝"
    );
    assert_eq!(
        ex.ops(),
        vec!["write:1:1"],
        "拒绝发生在第二次 execute 之前（check_linear 先行）"
    );
    assert!(undo.is_empty(), "被拒绝的第二次 Write 未产生任何副作用");
}

// ── A6 撤销双态：Runtime 路径往返 ─────────────────────────────────────
// interpret 后 undo 栈非空 → recover → 逆序执行 + 栈清空（w;w̄=1 执行级验证）。

#[test]
fn exec_A6_undo_roundtrip() {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("write:1:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:2:1", MockOutcome::Value(Value::U64(20)));

    let w1 = usage(Resource::Fd(1), AccessMode::Write);
    let w2 = usage(Resource::Fd(2), AccessMode::Write);
    let action = Action::Sequential {
        current: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            vec![w1],
        )),
        next: Box::new(move |_| {
            syscall_step(
                DataOp::Write {
                    fd: 2,
                    data: vec![0xBB],
                },
                vec![w2],
            )
        }),
    };

    // Runtime 路径（D9：Runtime::new 在 tokio 上下文之外构造）
    let ops_log = Arc::clone(&ex.log);
    let undo_log = Arc::clone(&ex.undo_log);
    let mut rt = Runtime::new(Box::new(ex));
    let v = rt.run_blocking(action);
    assert_eq!(v, Ok(Value::U64(20)), "第二个 Write 的结果");
    assert_eq!(
        *ops_log.lock().unwrap(),
        vec!["write:1:1".to_string(), "write:2:1".to_string()],
        "两个 Write 依次执行"
    );
    assert_eq!(rt.undo_stack().len(), 2, "interpret 后两个 undo 已压栈");

    // recoverΓ：驱动 Runtime::recover（本地 current-thread runtime）
    drive(rt.recover());

    assert!(rt.undo_stack().is_empty(), "recover 后撤销栈清空");
    assert_eq!(
        *undo_log.lock().unwrap(),
        vec!["undo(write:2:1)".to_string(), "undo(write:1:1)".to_string()],
        "LIFO 逆序执行：后压入的先撤销（w;w̄=1 执行级验证）"
    );
}

// ── D10 Replace：先 recover 再 target ─────────────────────────────────
// Sequence: Syscall(压入 undo) → Replace{target=Pure} → undo 先执行且最终值为 target 值。

#[test]
fn exec_D10_replace_order() {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("Close { fd: 999 }", MockOutcome::Value(Value::U64(1)));

    let action = Action::Sequential {
        current: Box::new(syscall_step(DataOp::Close { fd: 999 }, vec![])),
        next: Box::new(|_| Action::Replace {
            target: Box::new(Action::Pure(Value::U64(99))),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(99)), "Replace 以 target 结果结束（D10）");
    assert_eq!(
        ex.undo_ops(),
        vec!["undo(Close { fd: 999 })"],
        "undo 先执行：recover 在 target 之前完成（标记顺序）"
    );
    assert!(undo.is_empty(), "recover 后撤销栈已清空");
    assert_eq!(ex.ops(), vec!["Close { fd: 999 }"], "原流 Syscall 只执行一次");
}

// ── D14 Fork：静态冲突检测 + 顺序执行 ─────────────────────────────────
// 左右同资源 Write → can_parallel=false → 顺序执行（left→right）且 combine 合并正确。

#[test]
fn exec_fork_conflict_static() {
    // 静态冲突矩阵：同资源 Write×Write 不可并行
    let w = usage(Resource::Fd(1), AccessMode::Write);
    let l_set = vec![w.clone()];
    let r_set = vec![w.clone()];
    assert!(
        !ResourceRegistry::new().can_parallel(&l_set, &r_set),
        "同资源 Write×Write 冲突（pdr.md §9.1 / D14）"
    );

    let left = syscall_step(
        DataOp::Write {
            fd: 1,
            data: vec![0xAA],
        },
        l_set,
    );
    let right = syscall_step(
        DataOp::Write {
            fd: 1,
            data: vec![0xAA, 0xBB],
        },
        r_set,
    );

    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    assert!(
        fork_conflict(&reg, &left, &right),
        "解释器的静态冲突检测（D14）应报冲突"
    );

    let mut ex = MockExecutor::new();
    ex.respond("write:1:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:1:2", MockOutcome::Value(Value::U64(20)));

    let action = Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(a), Value::U64(b)) => Action::Pure(Value::U64(a + b)),
            _ => Action::Pure(Value::Unit),
        }),
    };
    let v = drive(interpret(action, &mut ctx, &mut undo, &mut reg, &mut ex));
    assert_eq!(v, Ok(Value::U64(30)), "combine 正确合并左右值（10+20）");
    assert_eq!(
        ex.ops(),
        vec!["write:1:1", "write:1:2"],
        "D14 阶段 1：冲突 Fork 顺序执行（left→right）"
    );
}
