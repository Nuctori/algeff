//! R4a 对抗审计（第 4 轮，分块 A：组合态深挖 + 值流极端 + 撤销残留）。
//! E2E 外部行为攻击，真实 Runtime + TokioExecutor，零 mock。
//!
//! R1 已覆盖：可逆深链/游标、线性绕过、并发 Fork、错误路径 put_back、
//! 值流（and_then 5 层、Scope 3 层 cwd）、确定性、Timeout 保留 undo。
//! R2 已覆盖：fd 区间、arbiter-MutexLock、Fork 错误传播 + Catch、Timeout 内
//! Fork、Mmap 边界、嵌套 Sequential + Replace。
//! R3a 已覆盖：Catch+Replace、Catch+Timeout、Timeout 内 Catch、Fork 分支内
//! Scope 错误（**顺序冲突路径**）、Scope 内 Replace、8 文件撤销压力、LIFO、
//! D10 复位。
//! R3b/R3c 已覆盖：复杂蓝图 100 轮轨迹、隐藏闭包盲区、网络深度、嵌套 Fork 深。
//! **R4a 攻击 R1-R3 未覆盖的组合深挖**：
//!
//! 1. **三维修组合**：
//!    - Catch×Replace×Timeout：action 部分副作用后出错 → handler 内 Replace
//!      （recover+clear）→ Replace 的 target 是 Timeout（Sleep 被 30ms 打断
//!      走 on_timeout）——错误恢复与超时控制在同一条嵌套路径上接续；
//!    - Scope×Fork×Catch：**并行** Fork（两分支资源不相交 → 真并行）左分支
//!      内 Scope 出错（finally 恢复分支 cwd）→ 错误传播 → 外层 Catch；右分支
//!      成功副作用照常发生；两分支 undo 均合并回父（错误路径也合并）→
//!      后续 Replace 逆序全恢复；
//!    - undo 栈在 Replace 后残留：两次 Replace（= 两次 recover）——第二次
//!      recover 对空栈必须是无副作用 no-op，随后同路径重开再写成功（D10 复位、
//!      无残留毒化）。
//! 2. **Scope 深层**：4 层嵌套 Scope，错误分别注入第 1/2/3/4 层（每层不同
//!    lvl1..lvl4 前缀）→ 无论错误深度，cwd 逐层 finally 全恢复；成功路径 4 层
//!    同样恢复。内层 Scope 内 Replace 后外层 Scope 退出时 cwd 恢复 + 外层
//!    继续执行（Replace 的终端语义局限在内层，外层组合节点照常接续）+ fd
//!    跨 Replace 单调（D1）。
//! 3. **值流极端**：and_then 10 层嵌套（Open + 8×Dup + Read，Fd 值贯穿全部
//!    10 层、每层断言单调）；Sequential 100 元素链（100 次逐字节 Read，
//!    值/字节贯穿 100 层、EOF 终止验证）。
//! 4. **Registry API 对抗**（模拟解释器外部使用，见
//!    `crates/algeff-core/tests/adversarial_r4a.rs`）：直接 allocate/take/
//!    merge/clear 组合序列，断言 D1 单调、D13 merge 语义、clear 后 next_fd 保留。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_std::adapters;
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R3 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
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

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

/// Open(path, rw+create) → Pure(Fd)。
fn open_fd(rt: &mut Runtime, path: PathBuf) -> u64 {
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path)],
            Action::Pure,
        ))
        .unwrap();
    fd_of(&v)
}

/// 确定的错误 syscall：Read 不存在 fd → NotFound（无 undo、无副作用）。
fn read_missing(fd: u64) -> Action {
    syscall(DataOp::Read { fd, len: 1 }, vec![rd(fd)], Action::Pure)
}

/// Open + Write 一个文件（返回 fd）。
fn write_file(rt: &mut Runtime, path: &std::path::Path, data: Vec<u8>) -> u64 {
    let fd = open_fd(rt, path.to_path_buf());
    rt.run_blocking(syscall(
        DataOp::Write { fd, data },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    fd
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1：三维修组合
// ══════════════════════════════════════════════════════════════════════

/// Catch×Replace×Timeout 组合（错误→Replace→再 Timeout 的嵌套路径）：
/// action 在部分副作用（Open+Write 已落盘）后出错 → handler 内 Replace
/// （recover 撤销部分副作用 + clear 释放句柄）→ Replace 的 target 是 Timeout
/// （Sleep 10s 被 30ms 打断 → on_timeout=42）。验证：错误恢复动作与超时控制
/// 在同一条嵌套路径上接续执行，且 Replace 的复位语义不破坏 Timeout 的执行。
#[test]
fn tri_catch_replace_timeout_error_then_replace_then_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("tri.txt");
    std::fs::write(&pa, b"tri-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let bp = Action::Catch {
        action: Box::new(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa.clone())],
            move |v| {
                let fd = fd_of(&v);
                syscall(
                    DataOp::Write {
                        fd,
                        data: b"TMP".to_vec(),
                    },
                    vec![wr(fd)],
                    move |_| read_missing(999_999),
                )
            },
        )),
        handler: Box::new(|e| {
            assert_eq!(e, SysError::NotFound, "action 部分副作用后出错");
            // 错误 → Replace（recover + clear）→ 再 Timeout（on_timeout 收敛）。
            Action::Replace {
                target: Box::new(Action::Timeout {
                    action: Box::new(Action::Sleep {
                        duration: Duration::from_secs(10),
                        next: Box::new(|_| Action::Pure(Value::U64(7))),
                    }),
                    duration: Duration::from_millis(30),
                    on_timeout: Box::new(Action::Pure(Value::U64(42))),
                }),
            }
        }),
    };
    let v = rt.run_blocking(bp).unwrap();
    assert_eq!(
        v,
        Value::U64(42),
        "handler 内 Replace 后再执行 Timeout → on_timeout 的值"
    );
    assert!(rt.undo_stack().is_empty(), "Replace 已 recover 清空撤销栈");
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"tri-original",
        "action 的部分写副作用被 handler 内 Replace 撤销"
    );
    assert!(
        rt.registry().lookup(0).is_none(),
        "Open fd(0) 句柄随 Replace clear 释放"
    );
    // 运行时未被毒化：错误→Replace→Timeout 整链后仍可用。
    rt.run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
}

/// Scope×Fork×Catch 三组合（**并行** Fork 路径）：左分支内 Scope 出错
/// （finally 恢复分支 cwd）→ 错误传播 → 外层 Catch 捕获；右分支 Scope 内
/// 成功副作用照常发生。两分支的 Write undo 都合并回父（错误路径也合并，
/// run_fork_parallel「子任务错误仍合并」），随后 Replace 逆序全恢复。
/// 与 R3a 的区别：R3a 走顺序冲突 Fork（两分支同 fd → 串行），本测试两分支
/// 资源不相交 → 真并行路径 + 分支内 Scope 错误 + 外层 Catch 的组合。
#[test]
fn tri_parallel_fork_scope_error_outer_catch_effects_merged() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("pf-a.txt");
    let pb = dir.path().join("pf-b.txt");
    std::fs::write(&pa, b"pa-original").unwrap();
    std::fs::write(&pb, b"pb-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let pa_fd_slot: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let pb_fd_slot: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let pf = pa_fd_slot.clone();
    let rf = pb_fd_slot.clone();

    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Fork {
                left: Box::new(Action::Scope {
                    base: PathBuf::from("pf-l"),
                    inner: Box::new(syscall(
                        DataOp::Open {
                            path: pa.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(pa.clone())],
                        move |v| {
                            let fd = fd_of(&v);
                            *pf.lock().unwrap() = Some(fd);
                            syscall(
                                DataOp::Write {
                                    fd,
                                    data: b"LL".to_vec(),
                                },
                                vec![wr(fd)],
                                move |_| read_missing(999_999),
                            )
                        },
                    )),
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                right: Box::new(Action::Scope {
                    base: PathBuf::from("pf-r"),
                    inner: Box::new(syscall(
                        DataOp::Open {
                            path: pb.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(pb.clone())],
                        move |v| {
                            let fd = fd_of(&v);
                            *rf.lock().unwrap() = Some(fd);
                            syscall(
                                DataOp::Write {
                                    fd,
                                    data: b"R".to_vec(),
                                },
                                vec![wr(fd)],
                                Action::Pure,
                            )
                        },
                    )),
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "左分支 Scope 内错误传播到外层 Catch");
                Action::Pure(Value::U64(5))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(5), "外层 Catch 捕获并行分支内 Scope 错误");

    let pa_fd = pa_fd_slot.lock().unwrap().expect("左分支 Open 已执行");
    let pb_fd = pb_fd_slot.lock().unwrap().expect("右分支 Open 已执行");
    assert_ne!(pa_fd, pb_fd, "并行分支 fd 互异（右分支全局唯一区间）");
    assert_eq!(
        rt.context().cwd,
        before,
        "并行分支内 Scope（错误/成功两路径）退出后 cwd 均恢复"
    );
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "错误左分支与成功右分支的 Write undo 都合并回父"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"LL-original",
        "左分支部分副作用落盘（错误后仍可被撤销）"
    );
    assert_eq!(
        std::fs::read(&pb).unwrap(),
        b"Rb-original",
        "右分支成功副作用落盘"
    );
    assert!(
        rt.registry().lookup(pa_fd).is_some() && rt.registry().lookup(pb_fd).is_some(),
        "错误路径分支句柄也随 merge 并入父（D13）"
    );

    // 两分支 undo（left 先压、right 后压）一次 Replace 逆序全恢复。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"pa-original",
        "左分支写被撤销"
    );
    assert_eq!(
        std::fs::read(&pb).unwrap(),
        b"pb-original",
        "右分支写被撤销"
    );
    assert!(
        rt.registry().lookup(pa_fd).is_none() && rt.registry().lookup(pb_fd).is_none(),
        "Replace 释放全部合并句柄"
    );
}

/// undo 栈在 Replace 后残留：两次 Replace（Replace 内部 = recover + clear，
/// 故第二次即对空栈的再次 recover）必须是无副作用 no-op——栈保持空、文件
/// 内容不被二次撤销破坏；随后同路径重开再写成功（无残留毒化 A4/D10）。
#[test]
fn undo_replace_twice_no_residue_recover_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r2-a.txt");
    let pb = dir.path().join("r2-b.txt");
    std::fs::write(&pa, b"r2a-original").unwrap();
    std::fs::write(&pb, b"r2b-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 两文件各写一次 → 2 条 undo（长短混合：截断路径与扩展路径）。
    write_file(&mut rt, &pa, b"AAAA".to_vec());
    write_file(&mut rt, &pb, b"BB".to_vec());
    assert_eq!(rt.undo_stack().len(), 2);
    assert_eq!(std::fs::read(&pa).unwrap(), b"AAAAoriginal");
    assert_eq!(std::fs::read(&pb).unwrap(), b"BBb-original");

    // Replace#1：recover（执行 2 条 undo）+ clear。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "第一次 Replace 清空撤销栈");
    assert_eq!(std::fs::read(&pa).unwrap(), b"r2a-original");
    assert_eq!(std::fs::read(&pb).unwrap(), b"r2b-original");

    // Replace#2：第二次 recover——栈已空，必须是无副作用 no-op（幂等）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(
        rt.undo_stack().is_empty(),
        "第二次 recover 后栈仍空（无残留）"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"r2a-original",
        "内容未被二次撤销破坏"
    );
    assert_eq!(std::fs::read(&pb).unwrap(), b"r2b-original");

    // 无残留的实证：同路径重开 + 再写成功（A4 线性标记已随 clear 复位，D10）。
    let fd3 = open_fd(&mut rt, pa.clone());
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd3,
            data: b"Z".to_vec(),
        },
        vec![wr(fd3)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&pa).unwrap(), b"Z2a-original");
    assert_eq!(rt.undo_stack().len(), 1, "新写产生新 undo");

    // 最终清理。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"r2a-original");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：Scope 深层
// ══════════════════════════════════════════════════════════════════════

/// 构造 `depth` 层嵌套 Scope，前缀依次 "lvl1".."lvl{depth}"（每层不同），
/// 最内层为 `inner`；每层 next 透传内层值（值可贯穿多层 Scope 供外层断言）。
fn nested_scope(depth: usize, inner: Action) -> Action {
    fn rec(d: usize, inner: Action) -> Action {
        Action::Scope {
            base: PathBuf::from(format!("lvl{d}")),
            inner: Box::new(if d == 1 { inner } else { rec(d - 1, inner) }),
            next: Box::new(Action::Pure),
        }
    }
    rec(depth, inner)
}

/// 4 层嵌套 Scope：错误分别注入第 1/2/3/4 层（每层不同 lvl 前缀）→ 无论
/// 错误深度，全部已进入的 Scope 逐层 finally 恢复 cwd；成功路径 4 层全走通
/// 后 cwd 同样恢复。R1 只测 3 层最内层出错；本测试把错误深度当变量扫 4 层。
#[test]
fn scope_4_level_cwd_restore_error_at_each_depth() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();
    let err = || read_missing(999_999);

    for depth in 1..=4usize {
        let e = rt.run_blocking(nested_scope(depth, err())).unwrap_err();
        assert_eq!(e, SysError::NotFound, "第 {depth} 层出错：错误传播");
        assert_eq!(
            rt.context().cwd,
            before,
            "第 {depth} 层出错：已进入的 {depth} 层 Scope 全部恢复 cwd"
        );
        assert!(
            rt.undo_stack().is_empty(),
            "第 {depth} 层出错：无 undo 残留"
        );
    }

    // 成功路径：4 层不同前缀全走通，cwd 同样恢复。
    let v = rt
        .run_blocking(nested_scope(
            4,
            syscall(DataOp::GetTime, vec![], Action::Pure),
        ))
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "成功路径执行 GetTime");
    assert_eq!(rt.context().cwd, before, "成功路径 4 层 cwd 恢复");
}

/// 内层 Scope 内 Replace 后外层 Scope 退出时 cwd 恢复 + 外层照常继续执行：
/// Replace 的终端语义（recover+clear 后执行 target 即结束，不回原流）局限在
/// 内层子树，外层组合节点（Sequential/Scope）不受影响——外层 next 继续
/// Open(pb)+Write；同时 fd 跨 Replace 单调（D1 不被 clear 破坏）。
#[test]
fn scope_inner_replace_outer_exit_restores_cwd_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("si-a.txt");
    let pb = dir.path().join("si-b.txt");
    std::fs::write(&pa, b"pa-original").unwrap();
    std::fs::write(&pb, b"pb-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let pa_fd_slot: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let pf = pa_fd_slot.clone();

    let bp = Action::Scope {
        base: PathBuf::from("out"),
        inner: Box::new(Action::Sequential {
            current: Box::new(Action::Scope {
                base: PathBuf::from("in"),
                inner: Box::new(syscall(
                    DataOp::Open {
                        path: pa.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pa.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        *pf.lock().unwrap() = Some(fd);
                        syscall(
                            DataOp::Write {
                                fd,
                                data: b"INNER".to_vec(),
                            },
                            vec![wr(fd)],
                            move |_| Action::Replace {
                                target: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
                            },
                        )
                    },
                )),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
            // 内层 Scope 内 Replace 是终端节点（不回原流），但外层 Sequential
            // 的 next 照常接续：Open(pb) → Write(pb)。
            next: Box::new({
                let pb_inner = pb.clone();
                move |_| {
                    syscall(
                        DataOp::Open {
                            path: pb_inner.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(pb_inner.clone())],
                        move |v| {
                            let fd2 = fd_of(&v);
                            syscall(
                                DataOp::Write {
                                    fd: fd2,
                                    data: b"OUTER".to_vec(),
                                },
                                vec![wr(fd2)],
                                move |_| Action::Pure(Value::U64(fd2)),
                            )
                        },
                    )
                }
            }),
        }),
        next: Box::new(Action::Pure),
    };
    let v = rt.run_blocking(bp).unwrap();
    let pb_fd = match v {
        Value::U64(f) => f,
        other => panic!("期望 U64(fd)，得到 {other:?}"),
    };
    let pa_fd = pa_fd_slot.lock().unwrap().expect("内层 Open 已执行");

    assert_eq!(
        rt.context().cwd,
        before,
        "内层 Scope 内 Replace 后外层 Scope 退出时 cwd 恢复"
    );
    assert_eq!(rt.undo_stack().len(), 1, "仅外层 Write 的 undo 保留");
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"pa-original",
        "内层 Replace 撤销了内层写"
    );
    assert_eq!(
        std::fs::read(&pb).unwrap(),
        b"OUTERiginal",
        "外层继续执行：pb 写生效"
    );
    assert!(
        pb_fd > pa_fd,
        "fd 跨 Replace 单调（D1 不被 clear 破坏）：{pa_fd} → {pb_fd}"
    );
    assert!(
        rt.registry().lookup(pa_fd).is_none(),
        "内层 Replace 释放 pa 句柄"
    );
    assert!(rt.registry().lookup(pb_fd).is_some(), "外层 pb 句柄可见");

    // 收尾：外层 undo 一次 Replace 恢复。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pb).unwrap(), b"pb-original");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：值流极端
// ══════════════════════════════════════════════════════════════════════

/// and_then 10 层嵌套值传递：Open + 8×Dup + Read 构成 10 层 and_then 链，
/// Fd 值贯穿全部 10 层（每层闭包都收到 Value::Fd 并断言单调），最内层 Read
/// 经第 10 层 fd 读回原内容——证明 10 层值流无丢失、句柄链有效。
/// R1 只测 5 层（Open→Seek→Read→Close）；本测试 10 层且每层断言值身份。
#[test]
fn flow_and_then_10_layer_fd_value_chain() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("at10.txt");
    std::fs::write(&pa, b"0123456789").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let content: Vec<u8> = b"0123456789".to_vec();

    // remaining = 剩余 Dup 层数；remaining == 0 时为第 10 层（Read）。
    fn dup_chain(fd: u64, remaining: u64, expect: Vec<u8>) -> Action {
        if remaining == 0 {
            return adapters::and_then(adapters::read(fd, expect.len()), move |v| {
                let b = match v {
                    Value::Bytes(b) => b,
                    other => panic!("第 10 层期望 Bytes，得到 {other:?}"),
                };
                assert_eq!(
                    b, expect,
                    "第 10 层经 fd {fd} 读回原内容（值贯穿 10 层后句柄链有效）"
                );
                Action::Pure(Value::U64(fd))
            });
        }
        adapters::and_then(adapters::dup(fd), move |v| {
            let nfd = fd_of(&v);
            assert!(nfd > fd, "D1：dup 新 fd 单调递增（{fd} → {nfd}）");
            dup_chain(nfd, remaining - 1, expect)
        })
    }

    // 1(open) + 8(dup) + 1(read) = 10 层 and_then。
    let bp = adapters::and_then(adapters::open_file(pa.clone(), rw_flags()), move |v| {
        dup_chain(fd_of(&v), 8, content.clone())
    });
    let v = rt.run_blocking(bp).unwrap();
    let last_fd = match v {
        Value::U64(f) => f,
        other => panic!("期望 U64(fd)，得到 {other:?}"),
    };
    assert!(rt.undo_stack().is_empty(), "Open/Dup/Read 均不产生 undo");
    assert!(
        rt.registry().lookup(last_fd).is_some(),
        "第 10 层 fd 句柄仍注册（Dup 链句柄可见）"
    );
}

/// Sequential 100 元素链：Open 后 100 层 Sequential 各 Read(fd,1) 一字节，
/// 字节值与序号贯穿 100 层（每层断言读到预期字节、计数递增），最终值 100；
/// 随后第 101 次 Read 得到 EOF（空 Bytes）——链执行完 fd 未毒化。
#[test]
fn flow_seq_100_element_chain_value_threading() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("seq100.txt");
    let content: Vec<u8> = (0..100u8).map(|i| b'0' + (i % 10)).collect();
    std::fs::write(&pa, &content).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let content_arc: Arc<Vec<u8>> = Arc::new(content.clone());

    fn read_chain(fd: u64, k: usize, content: Arc<Vec<u8>>) -> Action {
        if k >= content.len() {
            return Action::Pure(Value::U64(k as u64));
        }
        let expected = content[k];
        Action::Sequential {
            current: Box::new(syscall(
                DataOp::Read { fd, len: 1 },
                vec![rd(fd)],
                Action::Pure,
            )),
            next: Box::new(move |v| {
                let b = match v {
                    Value::Bytes(b) => b,
                    other => panic!("第 {k} 层期望 Bytes，得到 {other:?}"),
                };
                assert_eq!(b.len(), 1, "第 {k} 层每次恰好读 1 字节");
                assert_eq!(b[0], expected, "第 {k} 层字节值正确");
                read_chain(fd, k + 1, Arc::clone(&content))
            }),
        }
    }

    let fd_slot: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let sf = fd_slot.clone();
    let bp = adapters::and_then(
        adapters::open_file(
            pa.clone(),
            OpenFlags {
                read: true,
                ..Default::default()
            },
        ),
        move |v| {
            let fd = fd_of(&v);
            *sf.lock().unwrap() = Some(fd);
            read_chain(fd, 0, content_arc)
        },
    );
    let v = rt.run_blocking(bp).unwrap();
    assert_eq!(
        v,
        Value::U64(100),
        "100 元素 Sequential 链全部执行并逐字节验证"
    );
    assert!(rt.undo_stack().is_empty(), "Read 不产生 undo");

    // EOF 终止验证：同一 fd 游标已到文件尾，第 101 次 Read 返回空 Bytes。
    let fd = fd_slot.lock().unwrap().expect("Open 已执行");
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd, len: 1 },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    match v {
        Value::Bytes(b) => assert!(b.is_empty(), "100 次读后游标在文件尾：EOF（空 Bytes）"),
        other => panic!("期望 Bytes(EOF)，得到 {other:?}"),
    }
}
