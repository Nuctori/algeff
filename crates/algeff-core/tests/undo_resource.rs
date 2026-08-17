//! A6 Verification 批 2：撤销 × 资源生命周期 组合属性测试（pdr.md §四 A6 / §四 A4 / §1.1）。
//!
//! 交叉验证：公理 A6 撤销双态（w;w̄=1，pdr.md §四 A6 / §11）与公理 A4 资源线性
//! （Write/Own 恰好消费一次，pdr.md §四 A4）在 `ResourceRegistry` 上的组合性质 ——
//! **可重放性**（pdr.md §1.1 支柱一：控制流可缓存、重放）。
//!
//! registry 侧无直接 undo API（逆操作由 syscall 执行时动态构造并压入 UndoStack，
//! 决策 D4）；本文件用「手动恢复」模式模拟撤销：
//! - `take` 释放句柄（Close 的逆）；
//! - `clear()` 复位线性标记（Write 消费的逆，pdr.md §5.1.3 recoverΓ 的 registry 等价）。
//!
//! 测试目标：撤销动作执行后 registry 的线性状态必须允许重放同一序列（重放性 pdr.md §1.1）。
//!
//! 注意：`interpret`/`Runtime::run` 仍为 A2 的 `todo!()`，本文件禁止调用；
//! interpret 就绪后的执行级测试清单见文件末尾注释。

use std::sync::Arc;

use algeff_core::error::SysError;
use algeff_core::resource::{
    AccessMode, Resource, ResourceHandle, ResourceRegistry, ResourceUsage,
};
use algeff_core::runtime::UndoStack;
use proptest::prelude::*;

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

fn mutex_handle() -> ResourceHandle {
    ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(())))
}

// ── A6×A4：撤销恢复线性状态 → 重放同一序列（pdr.md §1.1 / §5.1.3）─────────

#[test]
fn undo_restores_linear_state() {
    // 三步操作：Open=allocate / Write=check_linear(Write) / Close=take
    let run_sequence =
        |reg: &mut ResourceRegistry, h: &ResourceHandle| -> Vec<Result<(), SysError>> {
            let mut trace = Vec::new();
            let fd = reg.allocate(h.clone()); // Open：分配句柄
            trace.push(Ok(()));
            trace.push(reg.check_linear(&usage(Resource::Fd(fd), AccessMode::Write))); // Write：线性消费
            trace.push(match reg.take(fd) {
                // Close：take 释放
                Some(_) => Ok(()),
                None => Err(SysError::InvalidInput),
            });
            trace
        };

    let handle = mutex_handle();
    let mut reg = ResourceRegistry::new();

    // 第一次完整序列：Open → Write → Close 全部成功
    let t1 = run_sequence(&mut reg, &handle);
    assert!(
        t1.iter().all(Result::is_ok),
        "第一次 Open→Write→Close 均应 Ok"
    );

    // 撤销（手动恢复模式）：take 已释放句柄，clear 复位线性标记
    // （Write 消费的逆 —— pdr.md §5.1.3 recoverΓ 的 registry 等价）
    reg.clear();

    // 可重放性（pdr.md §1.1）：撤销后 registry 的线性状态允许重放同一序列，
    // 第二次完整序列可再次成功执行，且两次 Ok/Err 轨迹完全一致
    let t2 = run_sequence(&mut reg, &handle);
    assert!(
        t2.iter().all(Result::is_ok),
        "撤销后重放 Open→Write→Close 仍应 Ok"
    );
    assert_eq!(t1, t2, "两次序列结果应完全一致（可重放性）");

    // 负向对照：Own 终结标记残留 → 重放被线性约束拒绝，
    // 证明「可重放」依赖撤销动作（状态复位），而非 check_linear 本身放行
    let mut reg2 = ResourceRegistry::new();
    let fd2 = reg2.allocate(handle);
    let r = Resource::Fd(fd2);
    assert!(reg2
        .check_linear(&usage(r.clone(), AccessMode::Own))
        .is_ok());
    assert_eq!(
        reg2.check_linear(&usage(r.clone(), AccessMode::Read)),
        Err(SysError::InvalidInput),
        "无撤销时 Own 后任何 usage 应被拒绝（A4 move 终结）"
    );
}

// ── A6：注册表逆序释放（LIFO）与 UndoStack 呼应（pdr.md §5.1.4 / §11）────

#[tokio::test]
async fn undo_lifo_with_registry() {
    // 分配 3 个句柄（实际 fd 自 0 起单调递增、永不复用 —— 决策 D1；
    // 概念上即 Fd 1/2/3）
    let mut reg = ResourceRegistry::new();
    let mut fds = Vec::new();
    for _ in 0..3 {
        let fd = reg.allocate(mutex_handle());
        assert!(reg.lookup(fd).is_some(), "分配后 fd {fd} 应可见");
        fds.push(fd);
    }
    assert_eq!(fds.len(), 3);
    assert!(
        fds.windows(2).all(|w| w[0] < w[1]),
        "Fd 单调递增（决策 D1）"
    );

    // 模拟撤销：按分配逆序（3→2→1）逐一 take，lookup 状态逐步收敛为空
    for fd in fds.iter().rev() {
        assert!(reg.lookup(*fd).is_some(), "释放前 fd {fd} 可见");
        assert!(reg.take(*fd).is_some(), "逆序 take fd {fd} 应命中句柄");
        assert!(reg.lookup(*fd).is_none(), "take 后 fd {fd} 不可见");
    }
    for fd in &fds {
        assert!(
            reg.lookup(*fd).is_none(),
            "全量逆序释放后 registry 收敛为空: fd {fd}"
        );
    }

    // 与 UndoStack LIFO 语义呼应（pdr.md §5.1.4）：按分配序压栈逆操作，
    // recover 后执行顺序必须为分配逆序 —— 与上述 registry 逆序释放次序一致
    let log: Arc<std::sync::Mutex<Vec<u64>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut stack = UndoStack::new();
    for fd in &fds {
        let log_ref = log.clone();
        let fd_val = *fd;
        stack.push(Box::pin(async move {
            log_ref.lock().unwrap().push(fd_val);
            Ok(())
        }));
    }
    assert_eq!(stack.len(), 3);
    stack.recover().await.unwrap();
    assert_eq!(
        *log.lock().unwrap(),
        fds.iter().rev().copied().collect::<Vec<u64>>(),
        "UndoStack 逆操作执行顺序 = 分配逆序（LIFO），与 registry 释放次序一致"
    );
    assert!(stack.is_empty(), "recover 后撤销栈清空");
}

// ── A6×A4 属性测试：可重放性（pdr.md §1.1）──────────────────────────────

/// 随机操作脚本：Alloc（分配句柄）/ Write(slot)（A4 线性写）/ Close(slot)（take 释放）。
/// slot 为相对句柄表的下标（而非具体 fd 值），保证脚本可在撤销后的新注册表上重放。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScriptOp {
    Alloc,
    Write(usize),
    Close(usize),
}

fn arb_script() -> impl Strategy<Value = Vec<ScriptOp>> {
    proptest::collection::vec(
        prop_oneof![
            Just(ScriptOp::Alloc),
            (0usize..8).prop_map(ScriptOp::Write),
            (0usize..8).prop_map(ScriptOp::Close),
        ],
        0..24,
    )
}

/// 在注册表上执行脚本，返回每步 Ok/Err 轨迹。
/// `slots` 是本测试维护的句柄表（与 registry 同步），使脚本可被重放：
/// 撤销（clear）后句柄表重建、fd 重新分配（D1 单调不复用），同一脚本的相对语义不变。
fn run_script(reg: &mut ResourceRegistry, script: &[ScriptOp]) -> Vec<Result<(), SysError>> {
    let mut slots: Vec<u64> = Vec::new();
    let mut trace = Vec::new();
    for op in script {
        match op {
            ScriptOp::Alloc => {
                let fd = reg.allocate(mutex_handle());
                slots.push(fd);
                trace.push(Ok(()));
            }
            ScriptOp::Write(i) => {
                let out = match slots.get(*i) {
                    Some(&fd) => reg.check_linear(&usage(Resource::Fd(fd), AccessMode::Write)),
                    None => Err(SysError::InvalidInput), // 句柄已关闭/不存在
                };
                trace.push(out);
            }
            ScriptOp::Close(i) => {
                let out = match slots.get(*i) {
                    Some(&fd) => {
                        slots.remove(*i);
                        if reg.take(fd).is_some() {
                            Ok(())
                        } else {
                            Err(SysError::InvalidInput)
                        }
                    }
                    None => Err(SysError::InvalidInput),
                };
                trace.push(out);
            }
        }
    }
    trace
}

proptest! {
    /// 可重放性属性：任意随机脚本 → 执行 → 全量回滚（clear 复位，等价于 recoverΓ
    /// 执行全部逆操作后的初始态）→ 再执行同一脚本 → 两次 Ok/Err 轨迹完全一致。
    /// 即撤销动作恢复 registry 线性状态后，重放同一序列必须可再次成功执行。
    #[test]
    fn undo_replay_same_script_identical_trace(ref script in arb_script()) {
        let mut reg = ResourceRegistry::new();
        let t1 = run_script(&mut reg, script);

        // 全量回滚：clear 复位句柄与线性标记（Write 消费被撤销）
        reg.clear();

        let t2 = run_script(&mut reg, script);
        prop_assert_eq!(t1, t2, "回滚后重放同一脚本应产生完全一致的 Ok/Err 轨迹");
    }
}

// ── interpret / Runtime::run 就绪后的执行级测试清单（rfc，供 A2 合并后补充）──
//
// 本文件只覆盖静态 registry + UndoStack 组合；A2 的 interpret 合并后需追加执行级
// 撤销×生命周期测试：
//   1. A6 执行级撤销往返：interpret(Syscall Open→Write→Close) 后 Runtime::recover，
//      registry 线性状态复位（write 消费清除、句柄释放），同一蓝图可重跑成功；
//   2. A6 执行级可重放性：interpret 跑随机脚本（Alloc/Write/Close 经 SyscallExecutor）
//      → recover → 重跑同蓝图 → 两次 Value/Err 结果一致（本批静态版的执行级推广）；
//   3. D10 Replace 语义：先 recover 再执行 target —— 解释器级验证 recover 与
//      线性状态复位（clear）在 Replace 中的顺序；
//   4. UndoStack LIFO 与 registry 释放次序的端到端一致性（本批静态版已覆盖，
//      执行级补 Value 级断言：recover 后所有 fd lookup 为空、线性标记复位）。
