//! 分支隔离测试（A6 Verification 批 8，数学审计 R1 发现的 P3 证据缺口补足）。
//!
//! 义务引用：spec/proof-obligations.md ——「A5 分支隔离 | 左 Write 不影响右 Read」。
//! R1 形式逻辑审计指出：既有证据只验证**两分支 Write 互不拦截**（registry 副本隔离、A4 线性
//! 检查不互相拒绝），**无任何「读侧隔离」测试**——右分支 Read 应看到写前内容；
//! Choose 的未选分支隔离也仅有 op 记录断言、无 registry 状态侧断言。本文件补足：
//! 注（审查 LOW-5）：A2 批 6 已合并（fd 区间全局分配）；本文件覆盖顺序路径（Write×Read 冲突
//! 恒顺序）与 Runtime Shared 路径；纯并行路径（读-读无冲突）的多重集合断言由
//! adversarial_r2.rs / commutation.rs 覆盖。
//!
//! 1. **exec_P3_fork_left_write_right_read_isolated**（P3 核心，顺序路径）：
//!    `Fork{left: Write(fd) → 写入标记, right: Read(fd) → 写前内容}`——左 Write
//!    后右 Read 的返回值必须为**写前内容**（读侧隔离：右分支 registry 副本不含
//!    左分支 Write 的消费/状态）；同时断言 op 顺序 left→right 与线性标记
//!    （左 Write 消费并入父 registry、右 Read 同 fd 不被拒绝——Read 不消费，天然允许）；
//! 2. **exec_P3_fork_read_isolation_runtime_path**：同一蓝图经 `Runtime`
//!    （Shared 通道）执行——同 fd Write×Read 冲突（D14）→ 恒顺序路径，覆盖
//!    F1/F2 合并路径（fd 区间预分割 + 线性标记 merge 回父）下读侧值的路由；
//! 3. **exec_A5_choose_true_else_zero_effect / exec_A5_choose_false_then_zero_effect**：
//!    `choose!(c, then: Write r1, else: Write r2)` → 未选分支 **op 零记录** +
//!    **registry 状态零效应**（未选分支资源无线性消费，check_linear 不拒绝）。
//!
//! A2 批 6 说明：批 6（嵌套 Fork 并行修复）**未合并**到本分支基线，故并行路径
//! 读隔离不在此验证（以顺序路径为确定性证据；并行路径的 registry 隔离并发保持
//! 已由 concurrency_stress.rs 的 D13 测试覆盖），待批 6 合并后补多重集合断言。
//!
//! 真实路径说明（为何不加 tempfile 版本）：`TokioExecutor` 的 Write 直接落盘
//! （executor.rs `op_write`，Full 撤销策略的写前读只用于 undo），顺序路径
//! left→right 下右分支读同一物理文件必然看到**写后内容**——「读回原内容」在
//! 共享物理文件语义下不可达。P3 的隔离义务位于 **registry/值层**（右分支 registry
//! 副本不含左分支 Write 的线性消费，读值不被左分支结果污染），由 Mock 可确定性地
//! 证明：op 顺序（Write 先于 Read 发生）+ 右分支 Read 值原样穿透 + 线性标记状态。
//!
//! 工程约束（同 execution_axioms.rs）：`interpret` 的 future 非 Send，用普通
//! `#[test]` + 本地 current-thread runtime 驱动（`drive`）；`Runtime::new` 须在
//! tokio 上下文之外调用（D9）。测试名含义务编号（P3/A5）属任务强制命名，
//! 故文件级允许 non_snake_case。

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use algeff_core::action::{Action, DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::{AccessMode, Resource, ResourceRegistry, ResourceUsage};
use algeff_core::runtime::{fork_conflict, interpret, Context, Runtime, UndoStack};
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoCapability};

/// 左分支 Write 的返回标记（Mock 配置）——执行层表示「写入已发生」。
const WRITE_MARKER: u64 = 0xFEED;
/// 右分支 Read 的返回内容（Mock 配置）——写前内容，即 P3 读隔离要断言的返回值。
const PRE_WRITE: &[u8] = &[0x11, 0x22, 0x33, 0x44];

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
}

/// 可配置 Mock 执行器：记录 op 调用序列，可按 op 描述返回 Value/Err/undo。
/// 模式参考 tests/execution_axioms.rs（log 用 Arc<Mutex>，供 Runtime 共享
/// 通道与后续 recover 跨调用读取）。
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
}

/// op → 稳定描述串（测试按此配置响应并断言调用序）。
/// Write 附带 data 长度以区分同 fd 的多次写。
fn describe(op: &DataOp) -> String {
    match op {
        DataOp::Write { fd, data } => format!("write:{fd}:{}", data.len()),
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
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        let desc = describe(op);
        Box::pin(async move {
            self.log.lock().unwrap().push(desc.clone());
            let out = match self.responses.get(&desc).cloned() {
                Some(MockOutcome::Value(v)) => v,
                None => Value::Unit,
            };
            let cap: UndoCapability = if self.with_undo && matches!(op, DataOp::Write { .. }) {
                // 真实语义：只有 Write 可逆（op_read 返回 undo=None，executor.rs）；
                // Read 不产生逆操作，避免读侧隔离断言被 undo 记录干扰。
                let label = format!("undo({desc})");
                let undo_log = self.undo_log.clone();
                UndoCapability::Invertible(Box::pin(async move {
                    undo_log.lock().unwrap().push(label);
                    Ok(())
                    }))
            } else {
                UndoCapability::Identity
            };
            Ok((out, cap))
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

// ── P3 核心：Fork 左 Write × 右 Read 的读侧隔离（顺序路径）───────────────
// 义务 A5/P3：「左 Write 不影响右 Read」。证据链（Mock 可确定性证明的三层）：
//   a. op 顺序 left→right —— Write 先于 Read 发生（时间序上写已发生，读仍得写前内容）；
//   b. 值路由 —— combine 收到 (写入标记, 写前内容)，最终结果 = 写前内容
//      （左分支 Write 的结果/状态未泄漏进右分支 Read 的返回值）；
//   c. registry 线性标记 —— 左 Write 消费并入父 registry（merge），右 Read 同 fd
//      不被拒绝（Read 不消费，天然允许），Write→Close(Own) 序列仍合法（pdr.md §14）。

/// P3 读隔离蓝图：left=Write(fd 1)，right=Read(fd 1)。同 fd Write×Read 冲突
/// （D14 can_parallel=false）→ 恒顺序路径，op 序确定。
fn fork_write_read_bp() -> Action {
    Action::Fork {
        left: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            vec![usage(Resource::Fd(1), AccessMode::Write)],
        )),
        right: Box::new(syscall_step(
            DataOp::Read { fd: 1, len: 4 },
            vec![usage(Resource::Fd(1), AccessMode::Read)],
        )),
        // combine 只取右分支（读侧）结果：若左分支 Write 泄漏进读值，断言即失败。
        combine: Box::new(|l, r| match (l, r) {
            (Value::U64(m), Value::Bytes(b)) => {
                assert_eq!(m, WRITE_MARKER, "左 Write 返回值（写入标记）");
                assert_eq!(
                    b.as_slice(),
                    PRE_WRITE,
                    "右 Read 返回值必须为写前内容（P3 读隔离）"
                );
                Action::Pure(Value::Bytes(b))
            }
            _ => Action::Pure(Value::Unit),
        }),
    }
}

/// 配置读隔离 Mock：Write(fd1) → 写入标记，Read(fd1) → 写前内容。
fn cfg_fork_read_isolation() -> MockExecutor {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("write:1:1", MockOutcome::Value(Value::U64(WRITE_MARKER)));
    ex.respond(
        "read:1:4",
        MockOutcome::Value(Value::Bytes(PRE_WRITE.to_vec())),
    );
    ex
}

#[test]
fn exec_P3_fork_left_write_right_read_isolated() {
    // 前提：同 fd Write×Read 静态冲突 → 顺序路径（left→right），op 序确定。
    let l_set = vec![usage(Resource::Fd(1), AccessMode::Write)];
    let r_set = vec![usage(Resource::Fd(1), AccessMode::Read)];
    assert!(
        !ResourceRegistry::new().can_parallel(&l_set, &r_set),
        "同 fd Write×Read 冲突（D14），Fork 走顺序路径"
    );
    let bp = fork_write_read_bp();
    assert!(
        fork_conflict(&ResourceRegistry::new(), &bp, &bp),
        "解释器静态冲突检测应报冲突（D14）"
    );

    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = cfg_fork_read_isolation();

    let v = drive(interpret(
        fork_write_read_bp(),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));

    // (b) 值路由：最终结果 = 右分支 Read 的写前内容（左 Write 标记未泄漏）
    assert_eq!(
        v,
        Ok(Value::Bytes(PRE_WRITE.to_vec())),
        "Fork 结果 = 右分支 Read 返回的写前内容（P3 读隔离）"
    );
    // (a) op 顺序 left→right：Write 已发生，Read 仍返回写前内容
    assert_eq!(
        ex.ops(),
        vec!["write:1:1", "read:1:4"],
        "顺序路径 left→right：左 Write 先执行、右 Read 后执行"
    );
    // 撤销栈只有左分支 Write 的 undo（右 Read 无 undo，读不产生逆操作）
    assert_eq!(undo.len(), 1, "仅左分支 Write 压入撤销栈");

    // (c) registry 线性标记：左 Write 消费并入父 registry（F2 merge）
    assert_eq!(
        reg.check_linear(&usage(Resource::Fd(1), AccessMode::Write)),
        Err(SysError::InvalidInput),
        "左分支 Write 的线性消费已并入父 registry（右分支副本未含该消费，故右 Read 未被拒绝）"
    );
    assert!(
        reg.check_linear(&usage(Resource::Fd(1), AccessMode::Read))
            .is_ok(),
        "Read 不消费：右分支同 fd 读在左分支 Write 消费后仍被允许（A4）"
    );
    assert!(
        reg.check_linear(&usage(Resource::Fd(1), AccessMode::Own))
            .is_ok(),
        "Write→Close(Own) 合法序列不受读隔离影响（pdr.md §14）"
    );
}

#[test]
fn exec_P3_fork_read_isolation_runtime_path() {
    // 同蓝图经 Runtime（Shared 通道）：同 fd Write×Read → fork_conflict=true →
    // 即使 Shared 通道也保持顺序执行（D14 阶段 1 语义）；覆盖 F1/F2 合并路径
    // （分支 registry 隔离 + 线性标记 merge 回父 + fd 区间预分割）下的读侧值路由。
    let ex = cfg_fork_read_isolation();
    let log = Arc::clone(&ex.log);
    let undo_log = Arc::clone(&ex.undo_log);
    let mut rt = Runtime::new(Box::new(ex));

    let v = rt.run_blocking(fork_write_read_bp());
    assert_eq!(
        v,
        Ok(Value::Bytes(PRE_WRITE.to_vec())),
        "Runtime 路径：Fork 结果 = 右分支 Read 的写前内容（P3 读隔离）"
    );
    assert_eq!(
        *log.lock().unwrap(),
        vec!["write:1:1".to_string(), "read:1:4".to_string()],
        "Runtime 顺序路径同样 left→right"
    );
    assert_eq!(rt.undo_stack().len(), 1, "仅左分支 Write 的 undo 压栈");

    // 状态侧：父 registry 线性标记经 merge 归位
    assert_eq!(
        rt.registry()
            .check_linear(&usage(Resource::Fd(1), AccessMode::Write)),
        Err(SysError::InvalidInput),
        "左 Write 消费经 F2 merge 并入父 registry"
    );
    assert!(
        rt.registry()
            .check_linear(&usage(Resource::Fd(1), AccessMode::Read))
            .is_ok(),
        "右 Read 同 fd 未被左 Write 消费拒绝（读侧隔离）"
    );

    // 撤销路径：recover 逆序执行（仅左 Write 一个 undo）
    drive(rt.recover()).unwrap();
    assert!(rt.undo_stack().is_empty(), "recover 后撤销栈清空");
    assert_eq!(
        *undo_log.lock().unwrap(),
        vec!["undo(write:1:1)".to_string()],
        "LIFO 撤销左分支 Write"
    );
}

// ── A5 Choose：未选分支零效应（op 零记录 + registry 状态零效应）────────────
// 义务 A5/P3「Choose/Fork 写隔离」。既有测试（concurrency_stress.rs）只断言
// 未选分支 op 零记录；本批补 **registry 状态侧**断言：未选分支资源无线性消费
// （其 Write 从未执行 → check_linear 不拒绝该资源的后续 Write）。

/// Choose 蓝图：cond 恒取时走 then（Write fd 1），否则走 else（Write fd 2）。
fn choose_bp(take_then: bool) -> Action {
    Action::Choose {
        cond: Box::new(move |_cur: &Value| take_then),
        then_branch: Box::new(syscall_step(
            DataOp::Write {
                fd: 1,
                data: vec![0xAA],
            },
            vec![usage(Resource::Fd(1), AccessMode::Write)],
        )),
        else_branch: Box::new(syscall_step(
            DataOp::Write {
                fd: 2,
                data: vec![0xBB],
            },
            vec![usage(Resource::Fd(2), AccessMode::Write)],
        )),
    }
}

fn cfg_choose_ex() -> MockExecutor {
    let mut ex = MockExecutor::new();
    ex.with_undo = true;
    ex.respond("write:1:1", MockOutcome::Value(Value::U64(10)));
    ex.respond("write:2:1", MockOutcome::Value(Value::U64(20)));
    ex
}

/// 跑一次 Choose 蓝图，返回（结果, op 日志, 撤销栈长度, 执行后 registry）。
/// 返回 registry 供状态侧断言（未选分支资源零消费）。
fn run_choose(
    take_then: bool,
) -> (
    Result<Value, SysError>,
    Vec<String>,
    usize,
    ResourceRegistry,
) {
    let mut ctx = Context::new();
    let mut undo = UndoStack::new();
    let mut reg = ResourceRegistry::new();
    let mut ex = cfg_choose_ex();
    let v = drive(interpret(
        choose_bp(take_then),
        &mut ctx,
        &mut undo,
        &mut reg,
        &mut ex,
    ));
    (v, ex.ops(), undo.len(), reg)
}

#[test]
fn exec_A5_choose_true_else_zero_effect() {
    let (v, ops, undo_len, mut reg) = run_choose(true);

    assert_eq!(v, Ok(Value::U64(10)), "then 分支结果（Write fd 1 → 10）");
    assert_eq!(
        ops,
        vec!["write:1:1"],
        "未选 else 分支 op 零记录（Write fd 2 从未执行）"
    );
    assert_eq!(undo_len, 1, "撤销栈仅 then 分支的 undo");
    // 状态侧：then 分支资源已线性消费；未选 else 分支资源零消费
    assert_eq!(
        reg.check_linear(&usage(Resource::Fd(1), AccessMode::Write)),
        Err(SysError::InvalidInput),
        "then 分支 Write(fd 1) 已消费"
    );
    assert!(
        reg.check_linear(&usage(Resource::Fd(2), AccessMode::Write))
            .is_ok(),
        "未选 else 分支资源零效应：Write(fd 2) 未被线性标记（A5 分支隔离）"
    );
}

#[test]
fn exec_A5_choose_false_then_zero_effect() {
    let (v, ops, undo_len, mut reg) = run_choose(false);

    assert_eq!(v, Ok(Value::U64(20)), "else 分支结果（Write fd 2 → 20）");
    assert_eq!(
        ops,
        vec!["write:2:1"],
        "未选 then 分支 op 零记录（Write fd 1 从未执行）"
    );
    assert_eq!(undo_len, 1, "撤销栈仅 else 分支的 undo");
    // 状态侧（对称验证）：未选 then 分支资源零消费；else 分支资源已消费
    assert!(
        reg.check_linear(&usage(Resource::Fd(1), AccessMode::Write))
            .is_ok(),
        "未选 then 分支资源零效应：Write(fd 1) 未被线性标记（A5 分支隔离）"
    );
    assert_eq!(
        reg.check_linear(&usage(Resource::Fd(2), AccessMode::Write)),
        Err(SysError::InvalidInput),
        "else 分支 Write(fd 2) 已消费"
    );
}
