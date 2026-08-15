//! R5a 对抗审计（第 5 轮，分块 A，**终轮** —— 修复回归终验）。
//!
//! E2E 外部行为攻击，真实 Runtime + TokioExecutor，零 mock。R5 是终轮：
//! 不再追求新攻击面，重点是 **R1-R4 修复点的回归** 与 **组合压力**。
//!
//! R1-R4 已覆盖（不重复）：游标撤销/线性/并发 Fork/错误路径 put_back/值流/
//! 确定性/MutexLock 仲裁（R1）、fd 区间/arbiter-WouldBlock/Fork 错误传播 +
//! Catch/Timeout 内 Fork（R2）、Catch×Replace×Timeout/Scope 深层/值流极端/
//! 规模栈深（R3/R4）、递归深度守卫（RFC-11，A2 批 7）。
//!
//! **R5a 攻击面 = 修复点回归 × 组合压力**：
//!
//! 1. **修复点五连回归**（同一组合蓝图依次触发）：cursor undo → flush 可见性
//!    → put_back 错误恢复 → arbiter WouldBlock → depth guard —— 断言每个修复
//!    点在混合场景（错误+Catch+Replace / dup 共享 / 仲裁竞争 / 超深嵌套）下
//!    仍生效（`fix_five_point_regression_single_blueprint`）。
//! 2. **守卫边界 × 组合**：深度 96 内嵌 Catch → 可捕获；深度 90 + Timeout →
//!    正常完成；守卫错误经 Timeout 原样透传（`guard_depth96_catch_catchable_...`；
//!    纯 95/96/97 边界在 core 侧 `adversarial_r5a.rs`）。
//! 3. **组合风暴**：Catch×Fork×Timeout×Scope 混合蓝图 50 轮，每轮轨迹一致
//!    （左分支超时 42、右分支错误捕获 1、cwd 恢复、写入立即可见），undo 栈
//!    50 条合并回父、终局 Replace 后栈空 + 全部内容恢复（`combo_storm_...`）。
//! 4. **修复点间交互**：depth guard 触发后 registry 干净（可继续新蓝图）；
//!    WouldBlock 后同 id 重试成功；flush 后 undo 恢复内容与游标
//!    （`interact_*` 三测）。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_std::adapters;
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R4 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
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

/// List([Fd, Fd]) → (fd0, fd1)（PipeOpen 返回值）。
fn pair_of(v: &Value) -> (u64, u64) {
    match v {
        Value::List(l) if l.len() == 2 => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
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

/// 确定的错误 syscall：Read 不存在 fd → NotFound（无 undo、无副作用）。
fn read_missing(fd: u64) -> Action {
    syscall(DataOp::Read { fd, len: 1 }, vec![rd(fd)], Action::Pure)
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

/// 深度 depth 的嵌套 Sequential：current 为下一层；叶子返回 U64(300)。
/// 与 core 侧 `adversarial_r5a.rs` 同构（RFC-11 守卫计数：depth≥96 → Other(105)）。
fn nested_seq(depth: u64) -> Action {
    if depth == 0 {
        return Action::Pure(Value::U64(300));
    }
    Action::Sequential {
        current: Box::new(nested_seq(depth - 1)),
        next: Box::new(|v| Action::Pure(v)),
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1：修复点五连回归 —— 同一组合蓝图依次触发
// （cursor undo → flush 可见性 → put_back 错误恢复 → arbiter WouldBlock →
//  depth guard），断言每个修复点在混合场景下仍生效。
// ══════════════════════════════════════════════════════════════════════

/// 单蓝图五阶段依次触发五个历史修复点（每个修复点嵌在组合/错误上下文中）：
///
/// 阶段 1（cursor undo，R1 A6 双态修复）：Open→Seek(0)→Write("XY") 部分
/// 副作用后注入 NotFound 错误 → Catch → handler 内 Replace（recover+clear）
/// —— 内容与**游标**（写后 2）都必须回到写前（0）。
/// 阶段 2（flush 可见性，R1 flaky 根因修复）：Open→Write("VISIBLE")，Write
/// 的 next 闭包内立即同步 `std::fs::read` 必须读到新内容（Write 完成 ⇔ OS
/// 落盘，D-039）。
/// 阶段 3（put_back 错误恢复，blocker-3 修复）：PipeOpen → Dup 写端（共享
/// Arc）→ 写 dup 出的写端 → `Arc::get_mut` 不可得 → InvalidInput，句柄必须
/// 被 put_back 放回（后续双端 Close 全部成功 = 未吞句柄）。
/// 阶段 4（arbiter WouldBlock，R-1/D16 修复）：MutexLock(777) 成功后同 id
/// 二次加锁 → 动态仲裁有限重试耗尽 → WouldBlock（绝不挂死）。
/// 阶段 5（depth guard，RFC-11 修复）：超深嵌套 → 守卫返回 Other(105) 且被
/// Catch 捕获（拒绝服务面转可恢复错误）。
///
/// 蓝图结束后：undo 栈恰 2 条（阶段 2 写 + 阶段 4 锁）；终局 Replace 逆序
/// 全恢复、registry 清空、可继续新蓝图（D10 无残留毒化）。
#[test]
fn fix_five_point_regression_single_blueprint() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("f5-cursor.txt");
    let pb = dir.path().join("f5-flush.txt");
    std::fs::write(&pa, b"cursor-original").unwrap();
    std::fs::write(&pb, b"flush-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 跨阶段 fd 槽位（测试内共享）。
    let pa_fd: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let pb_fd: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
    let pipe_fds: Arc<Mutex<Option<(u64, u64)>>> = Arc::new(Mutex::new(None));

    // ── 阶段 1：cursor undo（错误 → Catch → handler 内 Replace）──
    let s1 = pa_fd.clone();
    let phase1 = Action::Catch {
        action: Box::new(adapters::and_then(
            syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(pa.clone())],
                Action::Pure,
            ),
            move |v| {
                let fd = fd_of(&v);
                *s1.lock().unwrap() = Some(fd);
                // Seek(0) → Write("XY") → 游标 2 → 错误注入。
                adapters::and_then(
                    syscall(
                        DataOp::Seek {
                            fd,
                            offset: 0,
                            whence: std::io::SeekFrom::Start(0),
                        },
                        vec![rd(fd)],
                        Action::Pure,
                    ),
                    move |_| {
                        adapters::and_then(
                            syscall(
                                DataOp::Write {
                                    fd,
                                    data: b"XY".to_vec(),
                                },
                                vec![wr(fd)],
                                Action::Pure,
                            ),
                            move |_| read_missing(999_999),
                        )
                    },
                )
            },
        )),
        handler: Box::new(|e| {
            assert_eq!(e, SysError::NotFound, "阶段1：Open+Write 部分副作用后出错");
            // 修复点：错误路径 handler 内 Replace = recover + clear（D10）。
            Action::Replace {
                target: Box::new(Action::Pure(Value::U64(0))),
            }
        }),
    };

    // ── 阶段 2：flush 可见性（Write op 返回 ⇔ 同步读可见）──
    let s2 = pb_fd.clone();
    let pb2 = pb.clone();
    let phase2 = adapters::and_then(
        syscall(
            DataOp::Open {
                path: pb.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pb.clone())],
            Action::Pure,
        ),
        move |v| {
            let fd = fd_of(&v);
            *s2.lock().unwrap() = Some(fd);
            syscall(
                DataOp::Write {
                    fd,
                    data: b"VISIBLE".to_vec(),
                },
                vec![wr(fd)],
                move |_| {
                    // 修复点：Write 完成后效果必须立即可观察（R1 flaky 根因回归）。
                    assert_eq!(
                        &std::fs::read(&pb2).unwrap()[..7],
                        b"VISIBLE",
                        "阶段2：flush 后同步读立即可见新内容"
                    );
                    Action::Pure(Value::U64(1))
                },
            )
        },
    );

    // ── 阶段 3：put_back 错误恢复（dup 共享 Arc → 写失败 → 句柄放回）──
    let s3 = pipe_fds.clone();
    let phase3 = adapters::and_then(
        syscall(
            DataOp::PipeOpen {
                flags: Default::default(),
            },
            vec![],
            Action::Pure,
        ),
        move |v| {
            let (rfd, wfd) = pair_of(&v);
            *s3.lock().unwrap() = Some((rfd, wfd));
            adapters::and_then(
                syscall(DataOp::Dup { fd: wfd }, vec![wr(wfd)], Action::Pure),
                move |v| {
                    let wfd2 = fd_of(&v);
                    // 写 dup 出的写端：两注册表条目共享同一 Arc → get_mut 不可得
                    // → InvalidInput + put_back（blocker-3 错误路径恢复句柄）。
                    Action::Catch {
                        action: Box::new(syscall(
                            DataOp::Write {
                                fd: wfd2,
                                data: b"x".to_vec(),
                            },
                            vec![wr(wfd2)],
                            Action::Pure,
                        )),
                        handler: Box::new(|e| {
                            assert_eq!(
                                e,
                                SysError::InvalidInput,
                                "阶段3：dup 共享 Arc 后 &mut 不可得 → InvalidInput"
                            );
                            Action::Pure(Value::U64(1))
                        }),
                    }
                },
            )
        },
    );

    // ── 阶段 4：arbiter WouldBlock（同 id 二次加锁 → 仲裁重试耗尽）──
    let phase4 = adapters::and_then(
        syscall(DataOp::MutexLock { id: 777 }, vec![], Action::Pure),
        move |_| Action::Catch {
            action: Box::new(syscall(DataOp::MutexLock { id: 777 }, vec![], Action::Pure)),
            handler: Box::new(|e| {
                assert_eq!(
                    e,
                    SysError::WouldBlock,
                    "阶段4：胜者持有期同 id 竞争 → 有限重试后 WouldBlock（不挂死）"
                );
                Action::Pure(Value::U64(1))
            }),
        },
    );

    // ── 阶段 5：depth guard（超深嵌套 → Other(105) 可捕获）──
    let phase5 = Action::Catch {
        action: Box::new(nested_seq(95)),
        handler: Box::new(|e| {
            assert_eq!(e, SysError::Other(105), "阶段5：深度守卫错误（ENOBUFS=105）");
            Action::Pure(Value::U64(1))
        }),
    };

    // 同一组合蓝图：五阶段依次接续（Sequential 链）。
    let bp = adapters::and_then(
        phase1,
        move |_| {
            adapters::and_then(phase2, move |_| {
                adapters::and_then(phase3, move |_| adapters::and_then(phase4, move |_| phase5))
            })
        },
    );
    let v = rt.run_blocking(bp).unwrap();
    assert_eq!(v, Value::U64(1), "五阶段依次执行，末阶段返回 handler 值");

    // ── 修复点断言（全部在混合蓝图执行后成立）──
    // 1. cursor undo：内容 + 游标恢复（A6 双态 w;w̄=1，游标是经 Seek 可观察态）。
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"cursor-original",
        "阶段1：handler 内 Replace 撤销写副作用，内容恢复"
    );
    let pa_fd = pa_fd.lock().unwrap().expect("阶段1 Open 已执行");
    let pos = rt
        .run_blocking(syscall(
            DataOp::Seek {
                fd: pa_fd,
                offset: 0,
                whence: std::io::SeekFrom::Current(0),
            },
            vec![rd(pa_fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(pos, Value::U64(0), "阶段1：undo 恢复游标到写前位置 0（非写后 2）");

    // 2. flush 可见性：阶段 2 写入已落盘（内联断言之外再确认）。
    let _pb_fd = pb_fd.lock().unwrap().expect("阶段2 Open 已执行");
    assert_eq!(
        std::fs::read(&pb).unwrap(),
        b"VISIBLEriginal",
        "阶段2：写入内容可见（flush 契约；7 字节覆盖原文前 7 字节）"
    );

    // 3. put_back：写失败后句柄未被吞 → 双端 Close 均成功（未吞句柄证明）。
    let (rfd, wfd) = pipe_fds.lock().unwrap().expect("阶段3 PipeOpen 已执行");
    rt.run_blocking(syscall(DataOp::Close { fd: rfd }, vec![ow(rfd)], Action::Pure))
        .unwrap();
    rt.run_blocking(syscall(DataOp::Close { fd: wfd }, vec![ow(wfd)], Action::Pure))
        .unwrap();

    // 4. WouldBlock 后胜者锁 undo 保留、无状态毒化（阶段 4 锁 undo 仍在栈上）。
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "undo 栈 = 阶段2 Write + 阶段4 锁 各 1 条（阶段1 已被 Replace 清空）"
    );

    // 5. 终局 Replace：逆序恢复全部 undo + registry 清空 + 可继续新蓝图。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "终局 Replace 清空撤销栈");
    assert_eq!(
        std::fs::read(&pb).unwrap(),
        b"flush-original",
        "终局 Replace 撤销阶段2 写入"
    );
    rt.run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：守卫边界 × 组合（纯 95/96/97 边界在 core 侧）
// ══════════════════════════════════════════════════════════════════════

/// 守卫 × Catch/Timeout 组合：
/// - 深度 96 内嵌 Catch → 守卫错误（Other(105)）被捕获（拒绝服务面转可恢复）；
/// - 深度 90 + Timeout → 在超时窗口内正常完成（守卫不误伤、Timeout 不误触发）；
/// - 超深蓝图外包 Timeout+Catch → 守卫错误经 Timeout 原样透传（Timeout 只拦截
///   Elapsed，不吞错误）并可被外层 Catch 捕获。
#[test]
fn guard_depth96_catch_catchable_depth90_timeout_ok() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 深度 96 内嵌 Catch → 可捕获。
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(nested_seq(96)),
            handler: Box::new(|e| Action::Pure(Value::Str(format!("caught:{e}")))),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::Str("caught:Other(105)".to_string()),
        "深度 96 的守卫错误应被 Catch 捕获并执行 handler"
    );
    assert!(rt.undo_stack().is_empty(), "守卫错误不残留 undo");

    // 深度 90 + Timeout 组合：正常完成（5s 窗口远大于执行时间）。
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(nested_seq(90)),
            duration: Duration::from_secs(5),
            on_timeout: Box::new(Action::Pure(Value::U64(999))),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::U64(300),
        "深度 90 在 Timeout 内正常完成（未误触守卫/超时）"
    );

    // 守卫错误经 Timeout 原样透传（Timeout 对 Err 不转换），外层 Catch 可捕获。
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Timeout {
                action: Box::new(nested_seq(96)),
                duration: Duration::from_secs(5),
                on_timeout: Box::new(Action::Pure(Value::U64(999))),
            }),
            handler: Box::new(|e| Action::Pure(Value::Str(format!("t:{e}")))),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::Str("t:Other(105)".to_string()),
        "守卫错误经 Timeout 原样透传并可被外层 Catch 捕获"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：组合风暴 —— Catch×Fork×Timeout×Scope 混合蓝图 50 轮轨迹一致
// + undo 栈最终为空
// ══════════════════════════════════════════════════════════════════════

/// 单轮混合蓝图（每轮独立文件，轨迹形状一致）：
/// Scope{base:"storm"} 包 Fork：
/// - 左分支：Timeout{ Sleep(10s) 被 30ms 打断 → on_timeout=42 }（纯值，无资源
///   声明 → 与右分支无冲突 → 真并行路径）；
/// - 右分支：Catch{ Open(新文件) → Write("X{i}") → 错误注入 NotFound → handler=1 }
///   （真实 IO：Open+Write 经共享执行器锁；写 undo 合并回父）；
/// - combine：List([42, 1]) —— 每轮轨迹值恒定。
/// 每轮还承担 flush 可见性回归（Write 后立即同步读可见）。
fn storm_round(pa: PathBuf, i: u64) -> Action {
    Action::Scope {
        base: PathBuf::from("storm"),
        inner: Box::new(Action::Fork {
            left: Box::new(Action::Timeout {
                action: Box::new(Action::Sleep {
                    duration: Duration::from_secs(10),
                    next: Box::new(|_| Action::Pure(Value::U64(0))),
                }),
                duration: Duration::from_millis(30),
                on_timeout: Box::new(Action::Pure(Value::U64(42))),
            }),
            right: Box::new(Action::Catch {
                action: Box::new(adapters::and_then(
                    syscall(
                        DataOp::Open {
                            path: pa.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(pa.clone())],
                        Action::Pure,
                    ),
                    move |v| {
                        let fd = fd_of(&v);
                        adapters::and_then(
                            syscall(
                                DataOp::Write {
                                    fd,
                                    data: format!("X{i:02}").into_bytes(),
                                },
                                vec![wr(fd)],
                                Action::Pure,
                            ),
                            move |_| read_missing(999_999),
                        )
                    },
                )),
                handler: Box::new(|e| {
                    assert_eq!(e, SysError::NotFound, "右分支内错误被 Catch 捕获");
                    Action::Pure(Value::U64(1))
                }),
            }),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        }),
        next: Box::new(|v| Action::Pure(v)),
    }
}

/// 50 轮组合风暴：
/// - 每轮轨迹一致（值恒为 List([42, 1])、cwd 恢复、写入立即可见）；
/// - 每轮 1 条 Write undo 经 Fork 合并回父（50 条）；
/// - 终局 Replace 后 undo 栈为空、50 个文件内容全部恢复、可继续新蓝图。
#[test]
fn combo_storm_50_rounds_catch_fork_timeout_scope_trajectory() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();
    let mut files = Vec::new();
    let mut first: Option<Value> = None;

    for i in 0..50u64 {
        let pa = dir.path().join(format!("storm-{i:02}.txt"));
        std::fs::write(&pa, b"storm-original").unwrap();
        let data = format!("X{i:02}").into_bytes();
        let v = rt.run_blocking(storm_round(pa.clone(), i)).unwrap();

        // 轨迹一致（1）：每轮结果与首轮完全相同（左超时 42 + 右捕获 1）。
        match &first {
            None => first = Some(v.clone()),
            Some(f) => assert_eq!(&v, f, "第 {i} 轮轨迹与首轮不一致"),
        }
        assert_eq!(
            v,
            Value::List(vec![Value::U64(42), Value::U64(1)]),
            "第 {i} 轮：左分支超时收敛 42 + 右分支错误捕获 1"
        );
        // 轨迹一致（2）：Scope finally 恢复 cwd。
        assert_eq!(rt.context().cwd, before, "第 {i} 轮 Scope 退出后 cwd 恢复");
        // 轨迹一致（3）+ flush 回归：右分支 Write 立即可观察（覆盖原文同长前缀）。
        let expect = [
            data.clone(),
            b"storm-original"[data.len()..].to_vec(),
        ]
        .concat();
        assert_eq!(
            std::fs::read(&pa).unwrap(),
            expect,
            "第 {i} 轮：Write op 完成后内容立即可见（flush 契约）"
        );
        files.push(pa);
    }

    // 每轮 1 条 Write undo 合并回父（Fork 并行/顺序路径均合并，D13）。
    assert_eq!(rt.undo_stack().len(), 50, "50 轮 × 每轮 1 条 Write undo");
    assert_eq!(rt.context().cwd, before, "50 轮后 cwd 仍恢复");

    // 终局 Replace：50 条 undo 逆序全恢复 + registry 清空。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "组合风暴后 undo 栈最终为空");
    for (i, pa) in files.iter().enumerate() {
        assert_eq!(
            std::fs::read(pa).unwrap(),
            b"storm-original",
            "第 {i} 轮写入被终局 Replace 撤销恢复"
        );
    }

    // 无残留毒化：可继续新蓝图。
    let v = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "风暴后运行时仍可用");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4：修复点间交互
// ══════════════════════════════════════════════════════════════════════

/// depth guard 触发后 registry 干净：超深蓝图 → Other(105)，undo 空、registry
/// 无句柄、fd 从 0 重新起算 —— 守卫错误不毒化运行时，可继续新蓝图（Open→
/// Write→Replace 全链成功）。
#[test]
fn interact_depth_guard_clean_registry_then_new_blueprint() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("ig-guard.txt");
    std::fs::write(&pa, b"ig-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 触发深度守卫（深度 200 ≫ 阈值 96）→ Other(105)，进程不 abort。
    let e = rt.run_blocking(nested_seq(200)).unwrap_err();
    assert_eq!(e, SysError::Other(105), "超深蓝图应触发深度守卫");
    // 守卫错误零副作用：undo 空、registry 无句柄。
    assert!(rt.undo_stack().is_empty(), "守卫错误不产生 undo");
    assert!(
        rt.registry().lookup(0).is_none() && rt.registry().lookup(u64::MAX).is_none(),
        "守卫错误不分配资源（registry 干净）"
    );

    // 可继续新蓝图：registry 干净 → fd 从 0 起算（D1 单调未被守卫破坏）。
    let fd = open_fd(&mut rt, pa.clone());
    assert_eq!(fd, 0, "守卫后新蓝图 Open 从 fd 0 起（registry 无残留）");
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"NEW".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"NEWoriginal",
        "守卫后新蓝图写生效（3 字节覆盖原文前 3 字节）"
    );
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"ig-original", "新蓝图 undo 恢复");
}

/// WouldBlock 后同 id 重试成功：首锁成功（undo 入栈）→ 同 id 二次加锁
/// WouldBlock（合法占有）→ 不同 id 立即可用（arbiter 无全局残留）→ Replace
/// 释放全部锁与占坑 → 同 id 重试成功（无状态毒化）。
#[test]
fn interact_wouldblock_same_id_retry_after_replace() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 首锁成功（动态仲裁占坑 + 物理锁，undo 入栈）。
    rt.run_blocking(syscall(DataOp::MutexLock { id: 888 }, vec![], Action::Pure))
        .unwrap();
    assert_eq!(rt.undo_stack().len(), 1);

    // 同 id 二次加锁 → 仲裁有限重试耗尽 → WouldBlock（绝不挂死/死锁）。
    let e = rt
        .run_blocking(syscall(DataOp::MutexLock { id: 888 }, vec![], Action::Pure))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::WouldBlock,
        "胜者持有期同 id 竞争 → WouldBlock（合法占有，非占坑泄漏）"
    );

    // 不毒化（一）：不同 id 锁立即可用（arbiter 无全局残留）。
    rt.run_blocking(syscall(DataOp::MutexLock { id: 889 }, vec![], Action::Pure))
        .unwrap();

    // Replace（recover + clear）释放全部锁与占坑。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());

    // 不毒化（二）：WouldBlock 后同 id 重试成功。
    rt.run_blocking(syscall(DataOp::MutexLock { id: 888 }, vec![], Action::Pure))
        .unwrap();

    // 收尾。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
}

/// flush 后 undo 恢复内容与游标（R1 游标撤销 + R2 flush 契约的交互回归）：
/// Seek(5) → Write("XY") 后内容与游标（7）立即可观察；Replace（recover）后
/// 内容恢复为原文、游标回到写前位置 5（A6 双态：内容+长度+游标全复原）。
#[test]
fn interact_flush_undo_restores_content_and_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("fl-cursor.txt");
    std::fs::write(&pa, b"hello world").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd = open_fd(&mut rt, pa.clone());
    // Seek(Start 5) → Write("XY")：写前位置 5、写后游标 7。
    rt.run_blocking(syscall(
        DataOp::Seek {
            fd,
            offset: 5,
            whence: std::io::SeekFrom::Start(0),
        },
        vec![rd(fd)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"XY".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    // flush 契约：Write op 返回 ⇔ OS 落盘可观察。
    assert_eq!(std::fs::read(&pa).unwrap(), b"helloXYorld", "写入立即可见");
    let pos = rt
        .run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Current(0),
            },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(pos, Value::U64(7), "写后游标在 7");

    // Replace（D10 = recover + clear）：undo 恢复内容**与游标**。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"hello world", "undo 恢复写前内容");
    let pos = rt
        .run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Current(0),
            },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        pos,
        Value::U64(5),
        "undo 恢复游标到写前位置 5（非写后 7，A6 双态 w;w̄=1）"
    );
}
