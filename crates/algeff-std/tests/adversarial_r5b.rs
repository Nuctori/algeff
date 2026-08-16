//! R5 对抗审计（终轮，分块 B —— 最后新颖面）：错误恢复长链、Fork×Scope
//! 并行隔离、Timeout 三层嵌套、默认 ENOSYS 多轮、蓝图四路复用。
//!
//! 攻击方法论：与 R1-R4 相同——不 mock、全部经真实 `Runtime` + `TokioExecutor`
//! 全链路（`run_blocking` → `interpret` → 共享执行器通道）。本文件攻击
//! R1-R4 仍未覆盖的五个最后新颖面：
//!
//! 1. **错误恢复后长链继续**：Catch 处理错误后，同 Runtime 继续执行 100 节点
//!    链（错误不中断后续蓝图）；5 轮错误-恢复循环后状态干净（undo 空、文件
//!    复原、线性标记复位、fd 单调）。
//! 2. **Fork 并行 × Scope 互不干扰**：并行两分支各带不同 base 的 Scope（互不
//!    干扰）；顺序冲突路径下分支共享父 ctx——Scope 必须逐层 finally 恢复 cwd
//!    （父 cwd 不被泄漏）；嵌套并行 Fork × 多层 Scope + 外层活跃 Scope。
//! 3. **Timeout 链式嵌套 3 层**：Timeout{Timeout{Timeout{}}} 每层不同 duration
//!    ——最内层超时触发 on_timeout（其内再 Timeout），外层不误触发；错误经
//!    三层不被遮蔽；成功内层外层不误触发。
//! 4. **默认路径多轮**：WatchSignal/Invoke 默认 ENOSYS 透传连续 10 轮（错误
//!    码稳定 Other(38)）+ 不压 undo + 之后执行器健康（真实 undo 照常）。
//! 5. **蓝图复用（pdr §1.1 缓存/重放）**：同一 Action 蓝图（含 Syscall）经
//!    两个不同 Runtime 各执行一次 + 经同一 Runtime 两次——四次结果一致、
//!    每 Runtime 的 fd/undo 状态独立单调。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求 `Runtime::new`
//! 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::Arc;

use algeff_core::{
    Action, DataOp, OpenFlags, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R4 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn rd_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Path(path)).into_usage()
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

fn read_only_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        ..Default::default()
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

/// `n` 节点 GetTime 链：每节点断言收到 U64，计数递增；链尾返回 U64(n)。
fn gettime_chain(remaining: usize, acc: usize) -> Action {
    if remaining == 0 {
        return Action::Pure(Value::U64(acc as u64));
    }
    Action::Sequential {
        current: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
        next: Box::new(move |v| {
            assert!(matches!(v, Value::U64(_)), "GetTime 节点返回 U64");
            gettime_chain(remaining - 1, acc + 1)
        }),
    }
}

/// `n` 节点逐字节 Read 链：fd 游标从 0 起，第 k 节点断言读到 content[k]；
/// 读满后链尾返回 U64(n)。
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
            assert_eq!(b.len(), 1, "第 {k} 层恰好读 1 字节");
            assert_eq!(b[0], expected, "第 {k} 层字节值正确");
            read_chain(fd, k + 1, Arc::clone(&content))
        }),
    }
}

/// 读回 fd 全部内容（Seek(0) → Read(len)）。
fn read_back(rt: &mut Runtime, fd: u64, len: usize) -> Vec<u8> {
    let v = rt
        .run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![rd(fd)],
            move |_| syscall(DataOp::Read { fd, len }, vec![rd(fd)], Action::Pure),
        ))
        .unwrap();
    match v {
        Value::Bytes(b) => b,
        other => panic!("fd {fd} 读回失败: {other:?}"),
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1：错误恢复后长链继续 —— Catch 处理错误，同 Runtime 继续执行
// 100 节点链；多次错误-恢复循环后状态干净。
// ══════════════════════════════════════════════════════════════════════

/// 1a：Catch 内 action 部分副作用（Open+Write 已落盘）后出错 → handler 处理
/// → Sequential 的 next 继续执行 **100 节点**逐字节 Read 链（真实 IO，每节点
/// 断言读到预期字节）→ 链尾 U64(100)。错误不中断后续蓝图；失败 Write 的
/// undo 按契约保留（Catch 不触碰撤销栈），运行时未被毒化。
#[test]
fn catch_error_handled_then_100_node_read_chain_continues() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r5b-caught.txt");
    let pb = dir.path().join("r5b-chain.txt");
    std::fs::write(&pa, b"AAAA-original").unwrap();
    let content: Vec<u8> = (0..100u8).map(|i| b'0' + (i % 10)).collect();
    std::fs::write(&pb, &content).unwrap();

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let content_arc: Arc<Vec<u8>> = Arc::new(content.clone());

    let bp = Action::Sequential {
        // 前段：Open(pa) → Write("TMP") → Read(不存在 fd) → NotFound。
        current: Box::new(Action::Catch {
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
                Action::Pure(Value::Unit)
            }),
        }),
        // 后段：错误处理后，同 Runtime 继续 100 节点链（Open(pb) → 逐字节读）。
        next: Box::new({
            let content_arc2 = Arc::clone(&content_arc);
            move |_| {
                syscall(
                    DataOp::Open {
                        path: pb.clone(),
                        flags: read_only_flags(),
                    },
                    vec![rd_path(pb.clone())],
                    move |v| read_chain(fd_of(&v), 0, content_arc2),
                )
            }
        }),
    };

    let v = rt.run_blocking(bp).unwrap();
    assert_eq!(
        v,
        Value::U64(100),
        "错误处理后 100 节点链完整执行（错误不中断后续蓝图）"
    );
    assert_eq!(
        rt.undo_stack().len(),
        1,
        "失败 action 的 Write undo 按契约保留"
    );
    assert_eq!(
        &std::fs::read(&pa).unwrap()[0..3],
        b"TMP",
        "失败 action 的部分副作用落盘（Catch 不撤销，供外层 recover）"
    );
    // 运行时未被毒化：链后继续执行正常蓝图。
    rt.run_blocking(gettime_chain(3, 0)).unwrap();
}

/// 1b：5 轮「错误 → handler 内 Replace（recover+clear 状态复位）→ 100 节点
/// GetTime 链继续」循环，同 Runtime 串行执行。每轮断言：undo 空、文件内容
/// 复原（Write 被撤销）、fd 单调；下轮同路径 Open+Write 成功本身即证明线性
/// 标记已随 clear 复位（无残留毒化）。循环后最终一次同路径 Open+Write+Replace
/// 再证状态干净。
#[test]
fn error_recovery_5_cycles_state_clean_and_chain_continues() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r5b-cycle.txt");
    std::fs::write(&pa, b"cycle-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut fds: Vec<u64> = Vec::new();

    for round in 0..5u64 {
        // 记录本轮 Open 的 fd（Replace 清句柄后仍可查值）。
        let slot: Arc<std::sync::Mutex<Option<u64>>> = Arc::new(std::sync::Mutex::new(None));
        let s = slot.clone();
        let bp = Action::Sequential {
            current: Box::new(Action::Catch {
                action: Box::new(syscall(
                    DataOp::Open {
                        path: pa.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pa.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        *s.lock().unwrap() = Some(fd);
                        syscall(
                            DataOp::Write {
                                fd,
                                data: b"ZZ".to_vec(),
                            },
                            vec![wr(fd)],
                            move |_| read_missing(999_998),
                        )
                    },
                )),
                // 恢复：Replace（recover 撤销 Write + clear 线性标记/句柄）。
                handler: Box::new(move |e| {
                    assert_eq!(e, SysError::NotFound, "第 {round} 轮错误类型稳定");
                    Action::Replace {
                        target: Box::new(Action::Pure(Value::Unit)),
                    }
                }),
            }),
            // 错误-恢复后长链继续（100 节点）。
            next: Box::new(|_| gettime_chain(100, 0)),
        };
        let v = rt.run_blocking(bp).unwrap();
        assert_eq!(v, Value::U64(100), "第 {round} 轮恢复后 100 节点链完成");
        assert!(
            rt.undo_stack().is_empty(),
            "第 {round} 轮 handler Replace 已清空撤销栈"
        );
        assert_eq!(
            std::fs::read(&pa).unwrap(),
            b"cycle-original",
            "第 {round} 轮 Write 被恢复撤销（状态干净）"
        );
        let fd = slot.lock().unwrap().expect("本轮 Open 已执行");
        if let Some(prev) = fds.last() {
            assert!(
                fd > *prev,
                "fd 跨错误-恢复循环单调（D1 不被 clear 破坏）：{prev} → {fd}"
            );
        }
        fds.push(fd);
        // 下轮同路径 Open+Write 成功 = 线性标记已随本轮 Replace 复位
        // （若残留毒化，下轮 Open 的 wr_path 会被 A4 拦截）。
    }
    assert_eq!(fds.len(), 5, "5 轮各完成一次错误-恢复循环");

    // 循环后状态干净实证：同路径 Open+Write 成功 + Replace 完整复原。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);
    assert!(fd > *fds.last().unwrap(), "最终 Open fd 继续单调");
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"P".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        &std::fs::read(&pa).unwrap()[0..1],
        b"P",
        "循环后同路径重写成功（无线性残留毒化）"
    );
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"cycle-original");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：Fork 并行 × Scope 互不干扰。
// ══════════════════════════════════════════════════════════════════════

/// 2a：3 轮并行 Fork——左右分支各带不同 base 的 Scope（branch-L/branch-R），
/// 分支内真实 Open+Write+读回。分支资源不相交 → 真并行（D14 阶段 3）。
/// 每轮断言：父 cwd 始终不变（并行分支 ctx 隔离，Scope 不出圈）、两分支 fd
/// 互异、分支内读回值保真、文件已写、undo 按轮累积（每轮 2 条 Write undo）。
/// 末尾一次 Replace 逆序恢复全部 6 个文件。
#[test]
fn fork_parallel_two_scopes_distinct_bases_3_rounds_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();
    let mut files = Vec::new();
    let mut round_pairs = Vec::new();

    for round in 0..3u64 {
        let pl = dir.path().join(format!("r5b-sp-l{round}.txt"));
        let pr = dir.path().join(format!("r5b-sp-r{round}.txt"));
        let orig_l = format!("original-L{round}");
        let orig_r = format!("original-R{round}");
        std::fs::write(&pl, &orig_l).unwrap();
        std::fs::write(&pr, &orig_r).unwrap();
        files.push((pl.clone(), orig_l.clone(), pr.clone(), orig_r.clone()));

        let bp = Action::Fork {
            left: Box::new(Action::Scope {
                base: PathBuf::from("branch-L"),
                inner: Box::new(syscall(
                    DataOp::Open {
                        path: pl.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pl.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        syscall(
                            DataOp::Write {
                                fd,
                                data: b"LL".to_vec(),
                            },
                            vec![wr(fd)],
                            move |_| Action::Pure(Value::Fd(fd)),
                        )
                    },
                )),
                next: Box::new(|v| Action::Pure(v)),
            }),
            right: Box::new(Action::Scope {
                base: PathBuf::from("branch-R"),
                inner: Box::new(syscall(
                    DataOp::Open {
                        path: pr.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pr.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        syscall(
                            DataOp::Write {
                                fd,
                                data: b"RRR".to_vec(),
                            },
                            vec![wr(fd)],
                            move |_| Action::Pure(Value::Fd(fd)),
                        )
                    },
                )),
                next: Box::new(|v| Action::Pure(v)),
            }),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        };
        let v = rt.run_blocking(bp).unwrap();
        let (lfd, rfd) = match v {
            Value::List(l) if l.len() == 2 => (fd_of(&l[0]), fd_of(&l[1])),
            other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
        };
        assert_ne!(lfd, rfd, "第 {round} 轮并行分支 fd 互异（右分支区间）");
        assert_eq!(
            rt.context().cwd,
            before,
            "第 {round} 轮：并行分支各自 Scope 后父 cwd 不变（隔离）"
        );
        // 分支副作用落盘 + 读回保真。
        assert_eq!(
            &std::fs::read(&pl).unwrap()[0..2],
            b"LL",
            "第 {round} 轮左分支写生效"
        );
        assert_eq!(
            &std::fs::read(&pr).unwrap()[0..3],
            b"RRR",
            "第 {round} 轮右分支写生效"
        );
        let len_l = orig_l.len();
        let len_r = orig_r.len();
        assert_eq!(
            read_back(&mut rt, lfd, len_l),
            format!("LLiginal-L{round}").as_bytes(),
            "第 {round} 轮左分支 fd 读回保真"
        );
        assert_eq!(
            read_back(&mut rt, rfd, len_r),
            format!("RRRginal-R{round}").as_bytes(),
            "第 {round} 轮右分支 fd 读回保真"
        );
        round_pairs.push((lfd, rfd));
        assert_eq!(
            rt.undo_stack().len(),
            (round as usize + 1) * 2,
            "每轮 2 条 Write undo 累积"
        );
    }

    // 一次 Replace 逆序恢复全部 6 个文件（LIFO：右先左后，跨轮交错）。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    for (pl, orig_l, pr, orig_r) in &files {
        assert_eq!(&std::fs::read(pl).unwrap(), orig_l.as_bytes(), "左文件恢复");
        assert_eq!(&std::fs::read(pr).unwrap(), orig_r.as_bytes(), "右文件恢复");
    }
    assert_eq!(rt.context().cwd, before, "Replace 后 cwd 仍不变");
}

/// 2b：嵌套并行 Fork × 多层 Scope + 外层活跃 Scope。外层 Scope{outer} 包
/// Fork{ Fork{Scope L1, Scope L2}, Scope R1 }——全部 GetTime（无资源 → 全
/// 并行，含嵌套层）。整树跑两遍，父 cwd 每遍都回到初始值（外层 Scope 恢复
/// + 分支 ctx 隔离叠加成立），值结构完整。
#[test]
fn fork_nested_parallel_scopes_under_active_outer_scope_twice() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let scope_time = |base: &'static str| Action::Scope {
        base: PathBuf::from(base),
        inner: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
        next: Box::new(|v| Action::Pure(v)),
    };

    for round in 0..2u64 {
        let v = rt
            .run_blocking(Action::Scope {
                base: PathBuf::from("outer"),
                inner: Box::new(Action::Fork {
                    left: Box::new(Action::Fork {
                        left: Box::new(scope_time("L1")),
                        right: Box::new(scope_time("L2")),
                        combine: Box::new(|a, b| Action::Pure(Value::List(vec![a, b]))),
                    }),
                    right: Box::new(scope_time("R1")),
                    combine: Box::new(|a, b| Action::Pure(Value::List(vec![a, b]))),
                }),
                next: Box::new(|v| Action::Pure(v)),
            })
            .unwrap();
        // 值结构：List([List([t, t]), t]) —— 三个 GetTime 值全到达。
        match &v {
            Value::List(l) if l.len() == 2 => match &l[0] {
                Value::List(inner) if inner.len() == 2 => {
                    assert!(matches!(inner[0], Value::U64(_)));
                    assert!(matches!(inner[1], Value::U64(_)));
                }
                other => panic!("期望 List([t, t])，得到 {other:?}"),
            },
            other => panic!("期望 List([List, t])，得到 {other:?}"),
        }
        assert_eq!(
            rt.context().cwd,
            before,
            "第 {round} 遍：嵌套并行 Scope + 外层 Scope 后 cwd 完整恢复"
        );
        assert!(rt.undo_stack().is_empty(), "只读路径不产生 undo");
    }
}

/// 2c：顺序冲突 Fork（两分支 Write 同一 fd → 冲突 → 串行路径）——分支共享
/// 父 ctx（非隔离副本），Scope 必须逐层 finally 恢复，父 cwd 不泄漏。两分支
/// base 不同（seq-L/seq-R）；成功后父 cwd 恢复初始值；两次 Write 的 undo
/// 按观察序压栈（left 先 right 后），Replace 逆序恢复。
#[test]
fn fork_sequential_conflict_two_scopes_cwd_not_leaked() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r5b-seq.txt");
    std::fs::write(&pa, b"seq-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    // 父级先 Open 一次（fd 在 fork 外分配；分支内不再声明 Path 资源 →
    // 分支 Write 只在各自隔离 registry 消费 Fd，无跨分支线性冲突）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    let _ = rt
        .run_blocking(Action::Fork {
            left: Box::new(Action::Scope {
                base: PathBuf::from("seq-L"),
                inner: Box::new(syscall(
                    DataOp::Write {
                        fd,
                        data: b"L".to_vec(),
                    },
                    vec![wr(fd)],
                    move |_| Action::Pure(Value::Unit),
                )),
                next: Box::new(|v| Action::Pure(v)),
            }),
            right: Box::new(Action::Scope {
                base: PathBuf::from("seq-R"),
                inner: Box::new(syscall(
                    DataOp::Write {
                        fd,
                        data: b"RR".to_vec(),
                    },
                    vec![wr(fd)],
                    move |_| Action::Pure(Value::Unit),
                )),
                next: Box::new(|v| Action::Pure(v)),
            }),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap();
    assert_eq!(
        rt.context().cwd,
        before,
        "顺序冲突 Fork：两分支不同 base Scope 后父 cwd 不泄漏（各自 finally 恢复）"
    );
    assert_eq!(rt.undo_stack().len(), 2, "两分支 Write undo 按观察序压栈");
    // 两分支共享同一 fd 与执行器游标：left 在 0 写 1 字节（游标→1），
    // right 在游标 1 写 "RR" → 内容 "LRR-original"（观察序 left→right 生效）。
    assert_eq!(
        &std::fs::read(&pa).unwrap()[0..3],
        b"LRR",
        "顺序分支写按观察序落盘（left L + right RR，共享游标）"
    );
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(std::fs::read(&pa).unwrap(), b"seq-original", "逆序恢复");
    assert_eq!(rt.context().cwd, before);
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：Timeout 链式嵌套 3 层。
// ══════════════════════════════════════════════════════════════════════

/// 3a：Timeout{Timeout{Timeout{Sleep}}} 三层不同 duration——最内层（40ms）
/// 打断 Sleep(10s) 触发其 on_timeout；on_timeout 内**再 Timeout**（30ms 打断
/// Sleep(10s)）收敛 U64(777)；中层（3s）/外层（5s）不误触发（哨兵 555/333
/// 不得出现）。真实墙钟等待 ≈ 70ms。
///
/// 注意（审计 R1 语义裁决）：本测试为**墙钟时序语义**（各层独立 deadline、
/// 内层真实消耗毫秒级墙钟时间、外层 3s/5s 不触发）。virtual-clock 下虚拟时间
/// 全局累计（内层 Sleep 瞬时推进 10s+10s，中层 3s/外层 5s 均超限 → 级联触发
/// 最外层 333）——见 `timeout_nested_3_levels_virtual_clock_cascades`。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn timeout_nested_3_levels_only_innermost_fires_ontimeout_has_timeout() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Timeout {
                action: Box::new(Action::Timeout {
                    action: Box::new(Action::Sleep {
                        duration: std::time::Duration::from_secs(10),
                        next: Box::new(|_| Action::Pure(Value::U64(999))),
                    }),
                    duration: std::time::Duration::from_millis(40),
                    // 最内层 on_timeout 链内再 Timeout（30ms 打断 10s Sleep）。
                    on_timeout: Box::new(Action::Timeout {
                        action: Box::new(Action::Sleep {
                            duration: std::time::Duration::from_secs(10),
                            next: Box::new(|_| Action::Pure(Value::U64(888))),
                        }),
                        duration: std::time::Duration::from_millis(30),
                        on_timeout: Box::new(Action::Pure(Value::U64(777))),
                    }),
                }),
                duration: std::time::Duration::from_secs(3),
                on_timeout: Box::new(Action::Pure(Value::U64(555))),
            }),
            duration: std::time::Duration::from_secs(5),
            on_timeout: Box::new(Action::Pure(Value::U64(333))),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::U64(777),
        "仅最内层超时触发（含 on_timeout 内再 Timeout）；中层/外层不误触发"
    );
    assert!(rt.undo_stack().is_empty(), "超时路径不产生 undo");
    rt.run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
}

/// 3a-VC：virtual-clock 下同一三层嵌套蓝图的**级联语义**（审计 R1 语义裁决，
/// 双通道判定）。虚拟时间全局累计：最内层 Sleep 瞬时推进 10s（>40ms → 触发
/// on_timeout 链）、其内再推进 10s（>30ms → 777），此时虚拟流逝 20s —— 中层
/// 3s 与外层 5s 的 deadline 均已被内层消耗的超限虚拟时间越过，级联触发直至
/// 最外层 on_timeout（333）胜出。这是确定性语义（虚拟时间如实累计，与墙钟
/// 「内层真实耗时计入外层」同构）；墙钟路径的独立 deadline 语义见上方 3a。
#[cfg(feature = "virtual-clock")]
#[test]
fn timeout_nested_3_levels_virtual_clock_cascades() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Timeout {
                action: Box::new(Action::Timeout {
                    action: Box::new(Action::Sleep {
                        duration: std::time::Duration::from_secs(10),
                        next: Box::new(|_| Action::Pure(Value::U64(999))),
                    }),
                    duration: std::time::Duration::from_millis(40),
                    on_timeout: Box::new(Action::Timeout {
                        action: Box::new(Action::Sleep {
                            duration: std::time::Duration::from_secs(10),
                            next: Box::new(|_| Action::Pure(Value::U64(888))),
                        }),
                        duration: std::time::Duration::from_millis(30),
                        on_timeout: Box::new(Action::Pure(Value::U64(777))),
                    }),
                }),
                duration: std::time::Duration::from_secs(3),
                on_timeout: Box::new(Action::Pure(Value::U64(555))),
            }),
            duration: std::time::Duration::from_secs(5),
            on_timeout: Box::new(Action::Pure(Value::U64(333))),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::U64(333),
        "虚拟时钟全局累计：内层消耗 20s 虚拟时间，中/外层 deadline 均超限 → 级联至最外层"
    );
}

/// 3b：三层 Timeout 下最内层 action 立即出错（Read 不存在 fd）——错误必须
/// 原样穿透三层（`Ok(Err) → return Err`，不被 Timeout 遮蔽/改写），外层 Catch
/// 收到 NotFound。R4b 测过错误透传执行器，但**经三层 Timeout 嵌套**的穿透
/// 路径未见覆盖。
#[test]
fn timeout_nested_3_levels_error_passthrough_not_masked() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Timeout {
                action: Box::new(Action::Timeout {
                    action: Box::new(Action::Timeout {
                        action: Box::new(read_missing(999_997)),
                        duration: std::time::Duration::from_secs(5),
                        on_timeout: Box::new(Action::Pure(Value::U64(11))),
                    }),
                    duration: std::time::Duration::from_secs(5),
                    on_timeout: Box::new(Action::Pure(Value::U64(22))),
                }),
                duration: std::time::Duration::from_secs(5),
                on_timeout: Box::new(Action::Pure(Value::U64(33))),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "错误穿透三层 Timeout 原样到达");
                Action::Pure(Value::U64(1))
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1), "Catch 捕获穿透错误");
    assert!(rt.undo_stack().is_empty());
}

/// 3c：三层 Timeout 下最内层立即成功（GetTime）——外层两个 Timeout 不得
/// 误触发（结果必须是 GetTime 值而非哨兵）。零等待（成功路径即时返回）。
#[test]
fn timeout_nested_3_levels_success_outer_not_fired() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Timeout {
                action: Box::new(Action::Timeout {
                    action: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
                    duration: std::time::Duration::from_millis(40),
                    on_timeout: Box::new(Action::Pure(Value::U64(44))),
                }),
                duration: std::time::Duration::from_secs(3),
                on_timeout: Box::new(Action::Pure(Value::U64(55))),
            }),
            duration: std::time::Duration::from_secs(5),
            on_timeout: Box::new(Action::Pure(Value::U64(66))),
        })
        .unwrap();
    assert!(
        matches!(v, Value::U64(_)),
        "内层成功时三层 Timeout 均不触发（得到 GetTime 值 {v:?}）"
    );
    assert_ne!(v, Value::U64(44), "最内层成功不触发其 on_timeout");
    assert_ne!(v, Value::U64(55), "中层不误触发");
    assert_ne!(v, Value::U64(66), "外层不误触发");
    assert!(rt.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4：默认路径多轮 —— WatchSignal/Invoke 默认 ENOSYS 透传连续 10 轮
// （错误码稳定 Other(38)）+ 不压 undo + 之后执行器健康（真实 undo 照常）。
// TokioExecutor 未覆写 watch_signal/invoke → trait 默认实现。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn default_enosys_watchsignal_invoke_10_rounds_stable_no_undo_pressure() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r5b-enosys.txt");
    std::fs::write(&pa, b"enosys-original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    for i in 0..10u64 {
        let e = rt
            .run_blocking(Action::WatchSignal {
                signal: (i + 1) as i32,
                next: Box::new(Action::Pure),
            })
            .unwrap_err();
        assert_eq!(e, SysError::Other(38), "第 {i} 轮 watch_signal ENOSYS");
        assert_eq!(e.code(), 38, "第 {i} 轮 errno 稳定");
        assert!(
            rt.undo_stack().is_empty(),
            "第 {i} 轮 watch_signal 不压 undo"
        );

        let e = rt
            .run_blocking(Action::Invoke {
                foreign_id: i,
                captures: vec![],
                yields: vec![],
                deterministic: true,
                next: Box::new(Action::Pure),
            })
            .unwrap_err();
        assert_eq!(e, SysError::Other(38), "第 {i} 轮 invoke ENOSYS");
        assert_eq!(e.code(), 38, "第 {i} 轮 errno 稳定");
        assert!(rt.undo_stack().is_empty(), "第 {i} 轮 invoke 不压 undo");
    }

    // ENOSYS 10 轮后执行器健康：正常 syscall + 真实 undo 照常工作。
    let v = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "ENOSYS 后 GetTime 正常");
    let fd = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&fd);
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"W".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 1, "ENOSYS 轮不污染真实 undo 机制");
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(std::fs::read(&pa).unwrap(), b"enosys-original");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 5：蓝图复用（pdr §1.1 缓存/重放）——同一 Action 蓝图（含 Syscall）
// 经两个不同 Runtime 各执行一次 + 经同一 Runtime 两次：四次结果一致。
// ══════════════════════════════════════════════════════════════════════

/// 只读蓝图构建器：Open(rd) → Seek(0) → Read(len) → List([Fd, Bytes])。
/// 每次调用构造结构完全相同的 Action 树（Action 含 FnOnce 闭包不可 Clone，
/// 蓝图复用 = 同一构建逻辑重放，pdr §1.1「不可变代数数据类型，可缓存重放」）。
fn read_blueprint(path: PathBuf, len: usize, fd_out: Arc<std::sync::Mutex<Option<u64>>>) -> Action {
    let fo = fd_out.clone();
    syscall(
        DataOp::Open {
            path: path.clone(),
            flags: read_only_flags(),
        },
        vec![rd_path(path)],
        move |v| {
            let fd = fd_of(&v);
            *fo.lock().unwrap() = Some(fd);
            syscall(
                DataOp::Seek {
                    fd,
                    offset: 0,
                    whence: std::io::SeekFrom::Start(0),
                },
                vec![rd(fd)],
                move |_| syscall(DataOp::Read { fd, len }, vec![rd(fd)], Action::Pure),
            )
        },
    )
}

#[test]
fn blueprint_reuse_two_runtimes_twice_each_four_identical_results() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r5b-reuse.txt");
    let content: Vec<u8> = b"reuse-blueprint-0123456789".to_vec();
    std::fs::write(&pa, &content).unwrap();

    let mut rt_a = Runtime::new(Box::new(TokioExecutor::new()));
    let mut rt_b = Runtime::new(Box::new(TokioExecutor::new()));
    let len = content.len();

    let mut results = Vec::new();
    let mut fds_a = Vec::new();
    let mut fds_b = Vec::new();

    for run in 0..4u64 {
        let (rt, fds): (&mut Runtime, &mut Vec<u64>) = if run < 2 {
            (&mut rt_a, &mut fds_a)
        } else {
            (&mut rt_b, &mut fds_b)
        };
        let slot: Arc<std::sync::Mutex<Option<u64>>> = Arc::new(std::sync::Mutex::new(None));
        let v = rt
            .run_blocking(read_blueprint(pa.clone(), len, slot.clone()))
            .unwrap();
        let fd = slot.lock().unwrap().expect("Open 已执行");
        fds.push(fd);
        match &v {
            Value::Bytes(b) => results.push(b.clone()),
            other => panic!("期望 Bytes，得到 {other:?}"),
        }
        assert!(rt.undo_stack().is_empty(), "第 {run} 次：只读蓝图无 undo");
        assert!(
            rt.registry().lookup(fd).is_some(),
            "第 {run} 次：fd {fd} 句柄可见"
        );
    }

    // 四次结果一致（蓝图不可变可重放）。
    for (i, r) in results.iter().enumerate().skip(1) {
        assert_eq!(
            results[0], *r,
            "第 {i} 次执行与第 0 次结果一致（跨 Runtime/同 Runtime 复用）"
        );
    }
    // 每 Runtime 的 fd 独立单调（D1）：A/B 各自从 0 起分配（D9 隔离：
    // executor/registry 每 Runtime 独立），同 Runtime 两次执行 fd 递增。
    assert_eq!(fds_a, vec![0, 1], "A 同 Runtime 两次：fd 从 0 起单调");
    assert_eq!(
        fds_b,
        vec![0, 1],
        "B 同 Runtime 两次：fd 从 0 起单调（独立注册表）"
    );
    assert!(fds_a[1] > fds_a[0] && fds_b[1] > fds_b[0], "各自单调");

    // 蓝图四路复用后文件未被任何一次执行改动（只读）。
    assert_eq!(std::fs::read(&pa).unwrap(), content);
}

/// 带 Write（undo 产生）的蓝图双 Runtime 复用：同一蓝图结构在两个 Runtime
/// 各执行一次——结果一致、各 Runtime 的 undo 栈独立；对 A 做 Replace 只撤销
/// A 的文件效果，B 的写入保留（D9/D13 隔离在复用场景下同样成立）。
#[test]
fn write_blueprint_reuse_two_runtimes_undo_independent() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("r5b-wreuse.txt");
    std::fs::write(&pa, b"wreuse-original").unwrap();

    let mut rt_a = Runtime::new(Box::new(TokioExecutor::new()));
    let mut rt_b = Runtime::new(Box::new(TokioExecutor::new()));

    // 蓝图：Open(pa, rw) → Write("XX") → Fd 收敛（两个 Runtime 各执行一次）。
    let bp = || {
        syscall(
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
                        data: b"XX".to_vec(),
                    },
                    vec![wr(fd)],
                    move |_| Action::Pure(Value::Fd(fd)),
                )
            },
        )
    };

    let va = rt_a.run_blocking(bp()).unwrap();
    let vb = rt_b.run_blocking(bp()).unwrap();
    // 两 Runtime 各自从 0 起独立分配 fd（D9 隔离）——值相同正说明注册表独立。
    assert_eq!(va, Value::Fd(0), "A 首 fd 从 0 起");
    assert_eq!(vb, Value::Fd(0), "B 首 fd 从 0 起（独立注册表）");
    assert_eq!(rt_a.undo_stack().len(), 1, "A 的 undo 独立");
    assert_eq!(rt_b.undo_stack().len(), 1, "B 的 undo 独立");
    assert_eq!(
        &std::fs::read(&pa).unwrap()[0..2],
        b"XX",
        "两 Runtime 各写一次（同物理文件同偏移，内容幂等）"
    );

    // 仅对 A 做 Replace：只清 A 的撤销栈与注册表（Runtime 状态私有）。
    rt_a.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt_a.undo_stack().is_empty(), "A 已恢复");
    assert_eq!(
        rt_b.undo_stack().len(),
        1,
        "B 的 undo 不受 A 的 Replace 影响"
    );
    // A 的 undo 把 0-1 字节恢复为写前 "wr"（undo 日志捕获的是 A 写时的物理
    // 状态）——文件物理上回到写前内容；B 的运行时状态（undo/注册表）未动。
    assert_eq!(
        &std::fs::read(&pa).unwrap()[0..2],
        b"wr",
        "A 的 undo 物理恢复其写前字节（B 状态不动）"
    );
    let fa = match va {
        Value::Fd(f) => f,
        _ => unreachable!(),
    };
    let fb = match vb {
        Value::Fd(f) => f,
        _ => unreachable!(),
    };
    assert!(
        rt_a.registry().lookup(fa).is_none(),
        "A Replace 释放 A 句柄"
    );
    assert!(rt_b.registry().lookup(fb).is_some(), "B 句柄保留（隔离）");
    // B 自行 Replace：undo 日志捕获的是 B 写时的物理状态（当时 0-1 已是 A 的
    // "XX"）→ 写回 "XX"。记录偏差：**跨 Runtime 物理别名（两 Runtime 写同一
    // 文件同一区域）时 undo 日志按物理写前状态捕获，两次撤销不构成恒等**
    // （后撤者覆盖先撤者）——Runtime 状态隔离（D9）不含物理文件隔离，
    // 别名文件属用户责任；同 Runtime 内无此现象（R4a 已验证）。
    rt_b.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt_b.undo_stack().is_empty(), "B 已恢复");
    assert_eq!(
        &std::fs::read(&pa).unwrap()[0..2],
        b"XX",
        "B 的 undo 写回其写前状态（跨 Runtime 别名：后撤覆盖先撤，非恒等）"
    );
    eprintln!(
        "R5B 记录偏差：跨 Runtime 物理别名下 undo 日志按物理写前状态捕获，\
         两 Runtime 先后 Replace 后文件停在后者 undo 快照（非原始内容）——\
         Runtime 状态隔离不延伸至物理文件别名"
    );
}
