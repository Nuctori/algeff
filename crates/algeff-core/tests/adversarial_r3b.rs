//! R3b 对抗审计（分块 B，第 1 部分）：Alloc 内存面（E2E，真实 Runtime 全链路）。
//!
//! 攻击方法论：与 R1/R2 相同——不 mock、全部经真实 `Runtime`（`run_blocking`
//! → `interpret` 全链路）驱动。本文件聚焦 **Alloc 内存面**，这是 R1/R2 未
//! 覆盖的边界：
//!
//! 1. **Alloc 0 长度**：返回空 Bytes（`vec![0u8; 0]`），链式 Alloc 正常，
//!    不产生 undo（Alloc 纯值流，无逆操作）；
//! 2. **Alloc 1MB 后 Replace 释放（50 轮循环）**：R1 只验证单次 1MB+Replace，
//!    本测试验证 50 轮连续分配/释放周期：每轮长度正确、缓冲区全零、
//!    Replace 后撤销栈空 + registry 无句柄 + 运行时不被毒化；
//! 3. **Alloc 在 Fork 分支内（值流 + 合并）**：
//!    - 并行 Fork：两分支各 Alloc（512KB / 0），值经 combine 贯穿合并回父
//!      （真并行路径 `run_fork_parallel`）；
//!    - 冲突 Fork（顺序路径）：两分支声明同资源（Fd(77) Write）→ 静态冲突
//!      → 顺序执行；各分支 Alloc 值流经 combine 合并；分支线性标记经 merge
//!      并入父（父级同资源再声明被 A4 拦截），Replace 后复位。
//!
//! 说明：Alloc 是 interpret 层语义（`next(Value::Bytes(vec![0u8; len]))`），
//! 不触碰执行器；本文件 `NoopExecutor` 仅用于构造 `Runtime`，在语义验证中
//! 不参与（Alloc/Fork/Replace 的断言均不依赖执行器返回值）。executor 相关
//! 面见 `algeff-std/tests/adversarial_r3b.rs`。
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 与 `run_blocking` 在 tokio 上下文之外调用。

use algeff_core::{
    AccessMode, Action, BoxFuture, DataOp, Resource, ResourceRegistry, ResourceUsage, Runtime,
    SysError, SyscallExecutor, UndoCapability, Value,
};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

/// 线性绕过：手工构造 ResourceUsage（pdr §18 用户责任边界；本文件用于构造
/// Fork 静态冲突，声明与 op 不必对应——声明集是冲突检测的输入）。
fn wu(fd: u64) -> ResourceUsage {
    ResourceUsage {
        resource: Resource::Fd(fd),
        mode: AccessMode::Write,
    }
}

fn syscall(
    op: DataOp,
    resources: Vec<ResourceUsage>,
    next: impl FnOnce(Value) -> Action + Send + 'static,
) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(next),
    }
}

/// 最小执行器：仅用于构造 Runtime。Alloc 语义（本文件的攻击面）不经执行器，
/// 故本执行器不参与任何被断言的行为。
struct NoopExecutor;

impl SyscallExecutor for NoopExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, UndoCapability), SysError>> {
        Box::pin(async { Ok((Value::Unit, UndoCapability::Identity)) })
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1a：Alloc 0 长度
// ══════════════════════════════════════════════════════════════════════

/// Alloc(0)：返回空 Bytes（`vec![0u8; 0]`，非报错非非法参数）；链式
/// Alloc（0 → 4）值流正常；Alloc 全程不产生 undo（纯值流，无逆操作）。
#[test]
fn alloc_zero_len_empty_bytes_and_chain() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let v = rt
        .run_blocking(Action::Alloc {
            len: 0,
            next: Box::new(Action::Pure),
        })
        .unwrap();
    assert_eq!(v, Value::Bytes(vec![]), "Alloc(0) 应返回空 Bytes");

    // 链式：Alloc(0) 的 next 内再 Alloc(4)——值流贯穿
    let v = rt
        .run_blocking(Action::Alloc {
            len: 0,
            next: Box::new(|b| {
                assert_eq!(b, Value::Bytes(vec![]), "next 收到空 Bytes");
                Action::Alloc {
                    len: 4,
                    next: Box::new(Action::Pure),
                }
            }),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::Bytes(vec![0, 0, 0, 0]),
        "链式第二个 Alloc(4) 正常返回零填充缓冲区"
    );
    assert!(rt.undo_stack().is_empty(), "Alloc 纯值流，不产生 undo");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1b：Alloc 1MB 后 Replace 释放（50 轮循环）
// ══════════════════════════════════════════════════════════════════════

/// 50 轮 Alloc(1MB) + Replace 周期：每轮长度正确、缓冲区全零（首/中/尾）、
/// Replace（D10：recover + reg.clear）后撤销栈空 + registry 无句柄；循环后
/// 运行时仍可继续分配（释放不毒化）。R1 只验证单次，本测试压周期复用。
#[test]
fn alloc_1mb_replace_50_rounds() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    for round in 0..50u32 {
        let v = rt
            .run_blocking(Action::Alloc {
                len: 1024 * 1024,
                next: Box::new(|b| match b {
                    Value::Bytes(bytes) => {
                        assert_eq!(bytes.len(), 1024 * 1024, "1MB 缓冲区长度");
                        assert!(bytes.iter().all(|&x| x == 0), "分配缓冲区全零初始化");
                        Action::Pure(Value::U64(bytes.len() as u64))
                    }
                    other => panic!("期望 Bytes，得到 {other:?}"),
                }),
            })
            .unwrap();
        assert_eq!(
            v,
            Value::U64(1024 * 1024),
            "第 {round} 轮 1MB 分配返回正确长度"
        );

        // Replace = recover + reg.clear()（D10）：栈清空、句柄清空、next_fd 保留
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap();
        assert!(
            rt.undo_stack().is_empty(),
            "第 {round} 轮 Replace 后撤销栈空"
        );
        assert!(
            rt.registry().lookup(0).is_none(),
            "第 {round} 轮 registry 无句柄"
        );
    }
    // 50 轮周期后运行时未毒化：可继续分配
    let v = rt
        .run_blocking(Action::Alloc {
            len: 8,
            next: Box::new(|b| match b {
                Value::Bytes(bytes) => Action::Pure(Value::U64(bytes.len() as u64)),
                other => panic!("期望 Bytes，得到 {other:?}"),
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(8), "循环后继续分配正常");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1c：Alloc 在 Fork 分支内（值流 + 合并）
// ══════════════════════════════════════════════════════════════════════

/// 并行 Fork：两分支各 Alloc（512KB / 0），值经 combine 贯穿合并回父——
/// 真并行路径（`run_fork_parallel`，双分支零资源冲突）下 Alloc 值流无丢失、
/// 无交错污染；分支无 undo；父级可继续分配。
#[test]
fn alloc_in_parallel_fork_values_flow() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(Action::Alloc {
                len: 512 * 1024,
                next: Box::new(Action::Pure),
            }),
            right: Box::new(Action::Alloc {
                len: 0,
                next: Box::new(Action::Pure),
            }),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    match &v {
        Value::List(l) if l.len() == 2 => {
            match &l[0] {
                Value::Bytes(b) => {
                    assert_eq!(b.len(), 512 * 1024, "左分支 512KB 分配长度");
                    assert!(b.iter().all(|&x| x == 0), "左分支缓冲区全零");
                }
                other => panic!("期望 Bytes，得到 {other:?}"),
            }
            assert_eq!(l[1], Value::Bytes(vec![]), "右分支 Alloc(0) → 空 Bytes");
        }
        other => panic!("期望 List([Bytes, Bytes])，得到 {other:?}"),
    }

    // Alloc 无 undo、无句柄：并行合并后父状态干净
    assert!(rt.undo_stack().is_empty(), "并行 Fork 内 Alloc 不产生 undo");
    assert!(
        rt.registry().lookup(0).is_none(),
        "Alloc 不分配 registry 句柄"
    );

    // 父级继续分配（值流 + 状态未毒化）
    let v = rt
        .run_blocking(Action::Alloc {
            len: 8,
            next: Box::new(|b| match b {
                Value::Bytes(bytes) => Action::Pure(Value::U64(bytes.len() as u64)),
                other => panic!("期望 Bytes，得到 {other:?}"),
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(8));
}

/// 冲突 Fork（顺序路径）：两分支顶层都声明 wu(77) → 静态冲突 → 顺序执行
/// （left→right）；各分支 Alloc 值流经 combine 合并回父；分支的 A4 线性
/// 标记（Fd(77) Write 消费）经 merge 并入父 → 父级同资源再声明被拦截；
/// Replace 后线性复位、同资源再可用。
#[test]
fn alloc_in_conflict_fork_sequential_values_and_merge() {
    let mut rt = Runtime::new(Box::new(NoopExecutor));
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(Action::Sequential {
                current: Box::new(syscall(DataOp::GetTime, vec![wu(77)], Action::Pure)),
                next: Box::new(|_| Action::Alloc {
                    len: 16,
                    next: Box::new(|b| match b {
                        Value::Bytes(bytes) => Action::Pure(Value::U64(bytes.len() as u64)),
                        other => panic!("期望 Bytes，得到 {other:?}"),
                    }),
                }),
            }),
            right: Box::new(Action::Sequential {
                current: Box::new(syscall(DataOp::GetTime, vec![wu(77)], Action::Pure)),
                next: Box::new(|_| Action::Alloc {
                    len: 0,
                    next: Box::new(|b| match b {
                        Value::Bytes(bytes) => Action::Pure(Value::U64(bytes.len() as u64)),
                        other => panic!("期望 Bytes，得到 {other:?}"),
                    }),
                }),
            }),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    match &v {
        Value::List(l) if l.len() == 2 => {
            assert_eq!(l[0], Value::U64(16), "左分支 Alloc(16) 值流合并");
            assert_eq!(l[1], Value::U64(0), "右分支 Alloc(0) 值流合并");
        }
        other => panic!("期望 List([U64, U64])，得到 {other:?}"),
    }

    // F2 合并：两分支的 Fd(77) Write 消费并入父 → 父级同资源再声明被 A4 拦截
    let e = rt
        .run_blocking(syscall(DataOp::GetTime, vec![wu(77)], Action::Pure))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::InvalidInput,
        "冲突 Fork 后父级同资源 Write 声明应被 A4 拦截（线性标记合并）"
    );

    // Replace（D10）复位线性标记 → 同资源再可用
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    rt.run_blocking(syscall(DataOp::GetTime, vec![wu(77)], Action::Pure))
        .unwrap();
}
