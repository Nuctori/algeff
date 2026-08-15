//! R1 对抗审计（第 1 轮，范畴论视角的端到端攻击验证）。
//!
//! 攻击方法论：不从代码推理出发，而从**外部行为**出发寻找破坏点——在
//! 真实执行路径（`Runtime::run_blocking` + `TokioExecutor` + `interpret`
//! 全链路，不 mock）上检验契约律：
//! - 可逆性（A6 双态：w;w̄ = 1，含深链与游标恢复）
//! - 线性（A4：Write/Own 恰好一次；绕过 TypedResource 的直构 ResourceUsage）
//! - 并发（A3/D13/D17：并行 Fork、嵌套 Fork 回退边界、重复执行确定性）
//! - 错误路径（错误不毒化状态：put_back 恢复、ConnectionRefused、Timeout）
//! - 值流（and_then 5 层嵌套、Scope 3 层 cwd、Alloc 1MB + Replace）
//! - 确定性（同一蓝图两次执行 op 轨迹一致、GetTime 类型稳定）
//!
//! 驱动方式：普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。
//! 唯一例外：`rev_undo_restores_file_cursor` 中经外部 runtime 驱动
//! `Runtime::recover()`（recover 不创建 reactor，不违反 D9）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use algeff_core::{
    AccessMode, Action, BoxFuture, DataOp, OpenFlags, PipeFlags, Resource, ResourceHandle,
    ResourceRegistry, ResourceUsage, Runtime, SysError, SyscallExecutor, TypedResource, UndoOp,
    Value,
};
use algeff_std::adapters::{and_then, open_file};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制）────────────────────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<algeff_core::ReadOnly>::new_read(algeff_core::ResourceInner::Fd(fd))
        .into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<algeff_core::WriteOnly>::new_write(algeff_core::ResourceInner::Fd(fd))
        .into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<algeff_core::Owned>::new_owned(algeff_core::ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<algeff_core::WriteOnly>::new_write(algeff_core::ResourceInner::Path(path))
        .into_usage()
}
fn rd_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<algeff_core::ReadOnly>::new_read(algeff_core::ResourceInner::Path(path))
        .into_usage()
}

/// 线性对抗：绕过 TypedResource 手工构造 ResourceUsage（pdr §18 用户责任
/// 边界：类型状态包装是推荐 API，不能完全阻止绕过——绕过后的运行时拦截
/// 是 A4 的工程载体，本套件攻击它）。
fn wu(fd: u64) -> ResourceUsage {
    ResourceUsage {
        resource: Resource::Fd(fd),
        mode: AccessMode::Write,
    }
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

fn read_only_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        ..Default::default()
    }
}

/// Open → Write(payload) → Pure(Fd)。
fn open_write_pure(path: PathBuf, payload: &'static [u8]) -> Action {
    syscall(
        DataOp::Open {
            path: path.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(path)],
        move |v| {
            let fd = fd_of(&v);
            syscall(
                DataOp::Write {
                    fd,
                    data: payload.to_vec(),
                },
                vec![wr(fd)],
                move |_| Action::Pure(Value::Fd(fd)),
            )
        },
    )
}

/// Seek(0) → Read(len) → Pure(Bytes)。
fn seek_read_all(fd: u64, len: usize) -> Action {
    syscall(
        DataOp::Seek {
            fd,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        },
        vec![rd(fd)],
        move |_| syscall(DataOp::Read { fd, len }, vec![rd(fd)], Action::Pure),
    )
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1：可逆性（A6 双态 w;w̄ = 1 的深链与线性复位）
// ══════════════════════════════════════════════════════════════════════

/// 写→撤销→再写→再撤销 的深链（3 轮 × 2 文件）：每轮 Replace（D10：
/// 先 recover 再 clear）后文件恢复原内容、撤销栈清空、句柄释放；且
/// clear 后线性复位——下一轮同路径 Open+Write 真实可用。
#[test]
fn rev_deep_write_undo_chain() {
    let dir = tempfile::tempdir().unwrap();
    let p1 = dir.path().join("d1.txt");
    let p2 = dir.path().join("d2.txt");
    let original: Vec<u8> = b"original-content".to_vec();
    std::fs::write(&p1, &original).unwrap();
    std::fs::write(&p2, &original).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    for round in 0..3u8 {
        // payload 必须 ≥ 原内容长度（16 字节）：POSIX 游标写，短 payload 会
        // 在尾部留下原内容残留
        let payload = format!("round{round}-write!!-pad").into_bytes();
        let payload1 = payload.clone();
        let (fd1, fd2) = {
            // 顺序执行两条 open+write 链（值流经 CPS 闭包贯穿）
            let v = rt
                .run_blocking(syscall(
                    DataOp::Open {
                        path: p1.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(p1.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        syscall(
                            DataOp::Write {
                                fd,
                                data: payload1.clone(),
                            },
                            vec![wr(fd)],
                            move |_| Action::Pure(Value::Fd(fd)),
                        )
                    },
                ))
                .unwrap();
            let fd1 = fd_of(&v);
            let payload2 = payload.clone();
            let v = rt
                .run_blocking(syscall(
                    DataOp::Open {
                        path: p2.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(p2.clone())],
                    move |v| {
                        let fd = fd_of(&v);
                        syscall(
                            DataOp::Write { fd, data: payload2 },
                            vec![wr(fd)],
                            move |_| Action::Pure(Value::Fd(fd)),
                        )
                    },
                ))
                .unwrap();
            let fd2 = fd_of(&v);
            (fd1, fd2)
        };
        assert_eq!(
            rt.undo_stack().len(),
            2,
            "第 {round} 轮两个 Write 的 undo 已压栈"
        );
        assert_eq!(std::fs::read(&p1).unwrap(), payload, "写已生效");

        // Replace = recover + reg.clear()（D10）：内容恢复、栈清空、句柄释放
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap();
        assert!(rt.undo_stack().is_empty(), "第 {round} 轮撤销栈清空");
        assert_eq!(std::fs::read(&p1).unwrap(), original);
        assert_eq!(std::fs::read(&p2).unwrap(), original);
        assert!(rt.registry().lookup(fd1).is_none(), "句柄经 clear 释放");
        assert!(rt.registry().lookup(fd2).is_none());
    }
    // D10 clear 后线性复位：下一轮同路径 Open+Write 未被 A4 残留标记拦截
    assert_eq!(std::fs::read(&p1).unwrap(), original);
}

/// A6 双态可观察性对抗：Write 撤销后**文件游标**也必须恢复（w;w̄ = 1 的
/// 可观察状态包含游标位置——经 Seek(Current) 可观察）。写前游标 0 →
/// 写后游标 2 → 撤销后应回 0，而非停留在写后位置。
#[test]
fn rev_undo_restores_file_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("cursor.txt");
    std::fs::write(&pa, b"hello world").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd = {
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: OpenFlags {
                        read: true,
                        write: true,
                        ..Default::default()
                    },
                },
                vec![wr_path(pa.clone())],
                Action::Pure,
            ))
            .unwrap();
        fd_of(&v)
    };
    // 游标定位到 0 再写 "XY"（覆盖 "he"）
    rt.run_blocking(syscall(
        DataOp::Seek {
            fd,
            offset: 0,
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
    assert_eq!(std::fs::read(&pa).unwrap(), b"XYllo world");
    assert_eq!(rt.undo_stack().len(), 1);

    // recoverΓ（D4 异步逆操作）：经外部 runtime 驱动（不违反 D9——recover
    // 不创建 reactor）
    let outer = tokio::runtime::Runtime::new().unwrap();
    outer.block_on(rt.recover());
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"hello world", "内容恢复");

    // 游标必须是写前位置 0（A6：w;w̄ = 1——游标是经 Seek 可观察的状态）
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
    assert_eq!(pos, Value::U64(0), "撤销后游标应回到写前位置（A6 双态）");
}

/// R1 flaky 根因回归（写后可见性）：tokio::fs::File 的 `write_all` 是**异步落盘**——
/// `poll_write` 把数据拷入内部缓冲后立即返回 Ready，OS 写经后台 blocking 任务完成。
/// executor 若不在 Write op 返回前 `flush`，紧接的同步 `std::fs::read` 会读到写前
/// 旧内容（并行负载下 blocking pool 饱和拉宽在飞窗口 → 复现率 ~10-17%，见
/// `rev_undo_restores_file_cursor` 与 `lin_stale_fd_write_after_replace_fails`）。
/// 本测试多轮 Write+立即同步读：任一轮读到旧内容即触发（修复前并行负载下
/// 复现率 6~17%，本测试放大后 30 跑 13 跑触发）。
#[test]
fn rev_write_effect_immediately_observable_via_sync_read() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("obs.txt");
    std::fs::write(&pa, b"0000000000").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let fd = {
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: OpenFlags {
                        read: true,
                        write: true,
                        ..Default::default()
                    },
                },
                vec![wr_path(pa.clone())],
                Action::Pure,
            ))
            .unwrap();
        fd_of(&v)
    };
    for i in 0..64u8 {
        let payload = [b'A' + (i % 26); 4];
        rt.run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: 0,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
        rt.run_blocking(syscall(
            DataOp::Write {
                fd,
                data: payload.to_vec(),
            },
            vec![wr(fd)],
            Action::Pure,
        ))
        .unwrap();
        // Write op 返回后不得依赖任何中间操作兜底——同步读必须立即可见新内容
        // （修复前此处读到的是上一轮 payload 或初始内容）。
        let got = std::fs::read(&pa).unwrap();
        assert_eq!(
            &got[0..4],
            &payload[..],
            "第 {i} 轮：Write op 完成后效果必须立即可观察"
        );
        // Replace（D10 = recover + reg.clear）复位 A4 线性标记并撤销本轮 Write，
        // 供下一轮复用同一 fd（Write 的 WriteOnly 资源每轮只允许一次）。
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap();
        assert_eq!(
            std::fs::read(&pa).unwrap(),
            b"0000000000",
            "第 {i} 轮：Replace 撤销后内容恢复"
        );
    }
}

/// Rename 撤销后原路径可再 Open：a→b，Replace（undo 反向 Rename b→a），
/// 原路径 a 重新可 Open 且内容保留。
#[test]
fn rev_rename_undo_then_reopen_original() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("orig.txt");
    let b = dir.path().join("moved.txt");
    std::fs::write(&a, b"rename me").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    rt.run_blocking(syscall(
        DataOp::Rename {
            from: a.clone(),
            to: b.clone(),
        },
        vec![wr_path(a.clone()), wr_path(b.clone())],
        Action::Pure,
    ))
    .unwrap();
    assert!(!a.exists() && b.exists(), "Rename 生效");
    assert_eq!(rt.undo_stack().len(), 1);

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(a.exists() && !b.exists(), "undo 反向 Rename 恢复原路径");

    // 原路径可再 Open（非 NotFound），内容读回
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: a.clone(),
                flags: read_only_flags(),
            },
            vec![rd_path(a)],
            move |v| {
                let fd = fd_of(&v);
                syscall(
                    DataOp::Read {
                        fd,
                        len: "rename me".len(),
                    },
                    vec![rd(fd)],
                    Action::Pure,
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"rename me".to_vec()));
}

/// Mkdir 撤销（尽力 Rmdir 的边界）后父目录可再 Mkdir 同名：空目录 → undo
/// remove_dir 成功 → 同名 Mkdir 不再 AlreadyExists。
#[test]
fn rev_mkdir_undo_then_remkdir_same_name() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().join("sub");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    rt.run_blocking(syscall(
        DataOp::Mkdir {
            path: d.clone(),
            mode: 0o755,
        },
        vec![wr_path(d.clone())],
        Action::Pure,
    ))
    .unwrap();
    assert!(d.is_dir());
    assert_eq!(rt.undo_stack().len(), 1);

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(!d.exists(), "空目录撤销（remove_dir）应成功");

    // 尽力 Rmdir 边界：空目录撤销后同名再建可用
    rt.run_blocking(syscall(
        DataOp::Mkdir {
            path: d.clone(),
            mode: 0o755,
        },
        vec![wr_path(d.clone())],
        Action::Pure,
    ))
    .unwrap();
    assert!(d.is_dir(), "撤销后同名 Mkdir 不再 AlreadyExists");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2：线性（A4 的绕过与拦截）
// ══════════════════════════════════════════════════════════════════════

/// 线性绕过对抗（手工 ResourceUsage，绕过 TypedResource）：同一 fd 声明
/// Write 于冲突 Fork 两分支 → 静态冲突检测 → 顺序路径（D14）；两分支各持
/// 隔离 registry 副本（D13）→ 两次写都物理发生；随后**父级同资源 Write
/// 必须被 A4 拦截**（38bca67 F2 声称已修——本测试在真实 Runtime +
/// TokioExecutor 全链路上验证）。
#[test]
fn lin_fork_conflict_double_write_then_parent_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("lin.txt");
    std::fs::write(&pa, b"").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa.clone())],
            move |v| {
                let fd = fd_of(&v);
                Action::Fork {
                    left: Box::new(syscall(
                        DataOp::Write {
                            fd,
                            data: b"L".to_vec(),
                        },
                        vec![wu(fd)],
                        Action::Pure,
                    )),
                    right: Box::new(syscall(
                        DataOp::Write {
                            fd,
                            data: b"R".to_vec(),
                        },
                        vec![wu(fd)],
                        Action::Pure,
                    )),
                    combine: Box::new(move |_, _| Action::Pure(Value::Fd(fd))),
                }
            },
        ))
        .unwrap();
    let fd = fd_of(&v);

    // 冲突 Fork → 顺序执行（left→right）；D13 隔离 → 两侧 Write 都物理发生
    assert_eq!(std::fs::read(&pa).unwrap(), b"LR", "顺序路径两分支写均生效");

    // F2（38bca67）：分支线性标记经 merge 并入父 → 父级同资源 Write 被 A4 拒绝
    let err = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd,
                data: b"X".to_vec(),
            },
            vec![wu(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        err,
        SysError::InvalidInput,
        "Fork 后父级同资源 Write 应被 A4 拦截"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"LR",
        "拦截发生在 execute 之前（无第三次写）"
    );
}

/// D10 泄漏对抗（RFC-05 修复后）：Replace（recover + reg.clear）后，**旧 fd
/// 应已死亡**——任何经解释器使用旧 fd 的操作都应失败（NotFound）。修复前
/// executor 侧 `files` 映射仍持旧 fd 的 Arc 强引用，旧 fd Write 仍成功且物理
/// 落盘（历史偏差测试 `lin_stale_fd_write_after_replace_succeeds` 记录；R2
/// `r1_stale_fd_write_after_replace_recheck` 复现）；修复后 executor 以
/// registry 为 fd 活性唯一真相，旧 fd 任何操作（Write/Read/Close）一律
/// NotFound。
#[test]
fn lin_stale_fd_write_after_replace_fails() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("stale.txt");
    std::fs::write(&pa, b"seed-data").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd = {
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: OpenFlags {
                        read: true,
                        write: true,
                        ..Default::default()
                    },
                },
                vec![wr_path(pa.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"XXXX".to_vec(),
                        },
                        vec![wr(fd)],
                        move |_| Action::Pure(Value::Fd(fd)),
                    )
                },
            ))
            .unwrap();
        fd_of(&v)
    };
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();

    // Replace 后：registry 句柄已释放、内容已恢复
    assert!(rt.registry().lookup(fd).is_none(), "registry 句柄已释放");
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"seed-data", "内容已恢复");

    // 修复后（RFC-05）：旧 fd 写必须失败（NotFound）——executor 不得绕过
    // registry 直接使用残留的工作对象缓存。
    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd,
                data: b"ZZ".to_vec(),
            },
            vec![wr(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::NotFound,
        "修复后：Replace 后旧 fd Write 必须失败（registry 已失效）"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"seed-data",
        "旧 fd 写失败 → 恢复后的内容不再被破坏"
    );
}

/// RFC-05 修复配套：Replace 后旧 fd 的 Read / Close 同样失败（NotFound）。
/// 修复前 Read 经 `self.files` 直达物理句柄仍可读；Close 经 executor 内部
/// 映射 remove 分支「成功关闭」已失效 fd（registry remove 为 no-op）——修复
/// 后统一以 registry 活性判定：旧 fd 任何操作均 NotFound，恢复后的状态不再
/// 被触碰。
#[test]
fn lin_stale_fd_read_close_fail_after_replace() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("stale-rc.txt");
    std::fs::write(&pa, b"seed-data").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd = {
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: pa.clone(),
                    flags: OpenFlags {
                        read: true,
                        write: true,
                        ..Default::default()
                    },
                },
                vec![wr_path(pa.clone())],
                Action::Pure,
            ))
            .unwrap();
        fd_of(&v)
    };
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();

    // Replace 后：registry 句柄已释放、内容已恢复
    assert!(rt.registry().lookup(fd).is_none(), "registry 句柄已释放");
    assert_eq!(std::fs::read(&pa).unwrap(), b"seed-data", "内容已恢复");

    // 旧 fd Read → NotFound（修复前经 self.files 直达仍可读）
    let e = rt
        .run_blocking(syscall(
            DataOp::Read { fd, len: 4 },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::NotFound,
        "Replace 后旧 fd Read 应失败（registry 已失效）"
    );
    // 旧 fd Close → NotFound（修复前经 executor 内部映射 remove 会成功）
    let e = rt
        .run_blocking(syscall(
            DataOp::Close { fd },
            vec![ow(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::NotFound,
        "Replace 后旧 fd Close 应失败（registry 已失效）"
    );
    // 内容未被任何失败操作破坏
    assert_eq!(std::fs::read(&pa).unwrap(), b"seed-data", "内容保持恢复态");
}

/// RFC-05 修复配套：Replace 后同路径重开正常（D10「资源状态恢复至执行前」+
/// A4 复位）：新 fd 完全可用（Write 物理生效），旧 fd 彻底失效。既有
/// `conc_repeat_blueprint_100_rounds_deterministic` 覆盖多轮 fd 序列，本测试
/// 做单轮内容级验证。
#[test]
fn lin_replace_then_reopen_same_path_ok() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("reopen.txt");
    std::fs::write(&pa, b"seed-data").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd0 = {
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
        fd_of(&v)
    };
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();

    // 同路径重开：新 fd（D1 单调不复用），完整可用
    let fd1 = {
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
        fd_of(&v)
    };
    assert_ne!(fd0, fd1, "重开分配新 fd（D1 不复用）");
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd1,
            data: b"new".to_vec(),
        },
        vec![wr(fd1)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"new-data",
        "重开句柄写生效（游标 0 覆写 seed 前 3 字节）"
    );
    // 旧 fd 仍彻底失效
    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: fd0,
                data: b"ZZ".to_vec(),
            },
            vec![wr(fd0)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "旧 fd 仍失效（NotFound）");
    assert_eq!(std::fs::read(&pa).unwrap(), b"new-data", "旧 fd 写未落盘");
}

/// RFC-05 修复配套：Fork 并行分支内 Replace 的隔离性（D13）。左分支
/// Replace 只清**分支 registry**——其旧 fd 写失败（NotFound）；父级 registry
/// 与右分支的同一逻辑 fd 仍存活可用。修复不得以共享缓存（self.files）的
/// 全局失效为代价（只读校验，不删共享条目）。左分支资源与右分支不相交 →
/// can_parallel=true → 真并行路径；run_fork_parallel 在两分支均完成后才
/// 返回错误 → 右分支效果与两分支 registry 均已合并回父。
#[test]
fn conc_fork_parallel_branch_replace_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("iso-a.txt");
    let pb = dir.path().join("iso-b.txt");
    std::fs::write(&pa, b"seed").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 父级打开 pa → fd_a（Fork 后两分支 registry 克隆均含 fd_a）
    let fda = {
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
        fd_of(&v)
    };

    // 并行 Fork：左分支 Replace + 旧 fd 写（应 NotFound，左优先传播错误）；
    // 右分支 Open pb + Write（独立资源）。
    let bp = Action::Fork {
        left: Box::new(Action::Sequential {
            current: Box::new(Action::Replace {
                target: Box::new(Action::Pure(Value::Unit)),
            }),
            next: Box::new(move |_| {
                syscall(
                    DataOp::Write {
                        fd: fda,
                        data: b"L".to_vec(),
                    },
                    vec![wu(fda)],
                    Action::Pure,
                )
            }),
        }),
        right: Box::new(open_write_pure(pb.clone(), b"R")),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    };
    let e = rt.run_blocking(bp).unwrap_err();
    assert_eq!(
        e,
        SysError::NotFound,
        "左分支 Replace 后旧 fd Write 应 NotFound（左优先传播）"
    );

    // 隔离性证据：分支级 Replace 未波及父级与右分支
    assert!(
        rt.registry().lookup(fda).is_some(),
        "父级 fd_a 仍存活（D13：分支级 Replace 只清分支 registry）"
    );
    assert_eq!(std::fs::read(&pb).unwrap(), b"R", "右分支写物理生效");
    assert_eq!(std::fs::read(&pa).unwrap(), b"seed", "左分支旧 fd 写未落盘");

    // 父级继续使用 fd_a 正常（executor 共享缓存未被分支级 Replace 破坏）
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fda,
            data: b"P".to_vec(),
        },
        vec![wr(fda)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&pa).unwrap(), b"Peed", "父级 fd_a 写生效");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3：并发（D13/D14/D17 的真实并行路径）
// ══════════════════════════════════════════════════════════════════════

/// 并行 Fork 两分支各 Open+Write 不同文件 → combine 后父 registry 双句柄
/// 可读（D17：子任务隔离、合并回父；fd 区间预分割 F1 后两分支 fd 不相撞）。
#[test]
fn conc_fork_parallel_two_files_both_handles_readable() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("pa.txt");
    let pb = dir.path().join("pb.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let bp = || Action::Fork {
        left: Box::new(open_write_pure(pa.clone(), b"AAA")),
        right: Box::new(open_write_pure(pb.clone(), b"BBB")),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    };
    let v = rt.run_blocking(bp()).unwrap();
    let (lfd, rfd) = match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
    };
    assert_ne!(lfd, rfd, "两分支 fd 不得相撞（F1 区间预分割）");
    assert!(rt.registry().lookup(lfd).is_some(), "左分支句柄合并回父");
    assert!(rt.registry().lookup(rfd).is_some(), "右分支句柄合并回父");

    // 双句柄在父级真实可读（共享执行器 files 映射与合并后 registry 一致）
    let a = rt.run_blocking(seek_read_all(lfd, 3)).unwrap();
    let b = rt.run_blocking(seek_read_all(rfd, 3)).unwrap();
    assert_eq!(a, Value::Bytes(b"AAA".to_vec()), "左文件内容");
    assert_eq!(b, Value::Bytes(b"BBB".to_vec()), "右文件内容");
}

/// 确定性对抗：同一蓝图重复 100 次 → 结果与 fd 序列完全一致。
/// - 100 个全新 Runtime：每轮 fd 序列 [0,1]（D1 单调、起点确定）；
/// - 同一 Runtime 100 轮：fd 序列严格单调递增（2i, 2i+1），无重用。
#[test]
fn conc_repeat_blueprint_100_rounds_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("a.txt");
    let pb = dir.path().join("b.txt");

    fn open_two_bp(pa: PathBuf, pb: PathBuf) -> Action {
        syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pa)],
            move |v| {
                let a = fd_of(&v);
                syscall(
                    DataOp::Open {
                        path: pb.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(pb)],
                    move |v| {
                        let b = fd_of(&v);
                        Action::Pure(Value::List(vec![Value::Fd(a), Value::Fd(b)]))
                    },
                )
            },
        )
    }
    let pair = |v: &Value| match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("{other:?}"),
    };

    // 100 个独立 Runtime：fd 序列必须逐轮一致
    for i in 0..100 {
        let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
        let v = rt
            .run_blocking(open_two_bp(pa.clone(), pb.clone()))
            .unwrap();
        assert_eq!(pair(&v), (0, 1), "第 {i} 个新 Runtime 的 fd 序列");
    }

    // 同一 Runtime 100 轮：fd 严格单调（D1 永不复用）；每轮 Replace 复位
    // 线性标记（D10 clear），使下一轮同路径 Open+Write 合法（A4 复位）
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    for i in 0..100u64 {
        let v = rt
            .run_blocking(open_two_bp(pa.clone(), pb.clone()))
            .unwrap();
        assert_eq!(pair(&v), (2 * i, 2 * i + 1), "单 Runtime 第 {i} 轮 fd 序列");
        rt.run_blocking(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        })
        .unwrap();
    }
}

/// 嵌套 Fork（并行路径的回退边界）：外层并行（资源不相交），内层冲突
/// （同一 fd 双写）→ 内层在并行分支内回退顺序执行；全部效果合并回父，
/// 随后 Replace 全链撤销恢复。
#[test]
fn conc_nested_fork_mixed_parallel_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("nested-a.txt");
    let pb = dir.path().join("nested-b.txt");
    std::fs::write(&pa, b"seed").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 父级预置 fd（对 pa 的句柄）
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            vec![wr_path(pa.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fda = fd_of(&v);

    let bp = Action::Fork {
        // 内层：同一 fd 双写 → 冲突 → 即使外层并行，内层也回退顺序
        left: Box::new(Action::Fork {
            left: Box::new(syscall(
                DataOp::Write {
                    fd: fda,
                    data: b"L1".to_vec(),
                },
                vec![wu(fda)],
                Action::Pure,
            )),
            right: Box::new(syscall(
                DataOp::Write {
                    fd: fda,
                    data: b"L2".to_vec(),
                },
                vec![wu(fda)],
                Action::Pure,
            )),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        }),
        // 右分支：不同路径 → 与外层左分支不相交 → 外层真并行
        right: Box::new(open_write_pure(pb.clone(), b"R")),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    };
    rt.run_blocking(bp).unwrap();

    // 顺序语义：内层 left 写 "L1"（游标 0→2）、right 写 "L2"（游标 2→4）
    // 顺序语义：内层 left 写 "L1"（游标 0→2）、right 写 "L2"（游标 2 覆写
    // "ed"——POSIX 游标写，非追加）
    assert_eq!(std::fs::read(&pa).unwrap(), b"L1L2", "内层顺序双写落盘");
    assert_eq!(std::fs::read(&pb).unwrap(), b"R", "外层右分支落盘");
    assert_eq!(
        rt.undo_stack().len(),
        3,
        "内层 2 + 外层右 1 个 undo 合并回父"
    );

    // 全链 Replace → 全部撤销恢复（LIFO：右分支先、内层 L2、L1 后）
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), b"seed", "嵌套链撤销恢复 pa");
    // pb 的 undo（写前读为空 → 恢复空内容 + 截断回 0）不删除文件本身
    assert_eq!(std::fs::read(&pb).unwrap(), b"", "嵌套链撤销恢复 pb 内容");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4：错误路径（错误不毒化状态）
// ══════════════════════════════════════════════════════════════════════

/// A5 批 4 声称已修（blocker-3）的 E2E 验证：Dup 后 IO 错误 → put_back 恢复
/// → 原 fd 连续 10 次操作仍可寻址（每次都是预期的 InvalidInput 而非
/// NotFound——句柄未被吞）；关闭 dup 释放共享后原 fd 真实可读；全部 fd 可
/// 正常 Close。注：同一 fd 的重复 Write 被 A4 线性（每资源至多一次）拦截，
/// 故“连续 10 次操作”以读端错误循环 + 写端单次写验证。
#[test]
fn err_dup_io_error_10_consecutive_ops() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let (rfd, wfd) = match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("{other:?}"),
    };
    let v = rt
        .run_blocking(syscall(
            DataOp::Dup { fd: rfd },
            vec![wr(rfd)],
            Action::Pure,
        ))
        .unwrap();
    let rfd2 = fd_of(&v);

    // 连续 10 次：读被 Dup 共享的读端 → InvalidInput（非 NotFound），
    // 每次错误后 put_back 恢复注册表条目与内部映射
    for i in 0..10 {
        let e = rt
            .run_blocking(syscall(
                DataOp::Read { fd: rfd, len: 1 },
                vec![rd(rfd)],
                Action::Pure,
            ))
            .unwrap_err();
        assert_eq!(
            e,
            SysError::InvalidInput,
            "第 {i} 次：Dup 共享读应 InvalidInput（句柄可寻址）而非 NotFound"
        );
    }

    // 未 Dup 的写端不受影响：一次写成功（A4 线性：同一 fd Write 至多一次）
    let v = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: wfd,
                data: b"ping".to_vec(),
            },
            vec![wr(wfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Unit, "写端写成功");

    // dup 端同样稳定可寻址
    let e = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd2, len: 1 },
            vec![rd(rfd2)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::InvalidInput);

    // 关闭 dup 释放共享 → 原 fd 恢复真实可读（10 次 put_back 轮换后映射仍正确）
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd2 },
        vec![ow(rfd2)],
        Action::Pure,
    ))
    .unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Read {
                fd: rfd,
                len: "ping".len(),
            },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        v,
        Value::Bytes(b"ping".to_vec()),
        "原 fd 经 10 次错误循环后仍可读"
    );

    // 全部 fd 正常 Close（映射未被错误路径吞掉）
    for fd in [rfd, wfd] {
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
            .unwrap();
    }
}

/// TcpConnect 到未监听端口 → ConnectionRefused → 错误不毒化状态：
/// undo 栈空、registry 无残留、后续蓝图（管道写读）完全正常。
#[test]
fn err_tcp_connect_refused_no_state_poison() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 绑定临时端口并关闭监听（释放端口）
    let v = rt
        .run_blocking(syscall(
            DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let lfd = fd_of(&v);
    let addr = match rt.registry().lookup(lfd).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener，得到 {other:?}"),
    };
    rt.run_blocking(syscall(
        DataOp::Close { fd: lfd },
        vec![ow(lfd)],
        Action::Pure,
    ))
    .unwrap();

    // 连接已关闭端口 → ConnectionRefused（Windows 经 Other(errno) 兜底）
    let e = rt
        .run_blocking(syscall(DataOp::TcpConnect { addr }, vec![], Action::Pure))
        .unwrap_err();
    match e {
        SysError::ConnectionRefused | SysError::Other(_) => {}
        other => panic!("期望 ConnectionRefused，得到 {other:?}"),
    }

    // 错误不毒化：undo 栈空、registry 无新句柄
    assert!(rt.undo_stack().is_empty(), "失败的 connect 不产生 undo");
    assert!(
        rt.registry().lookup(lfd).is_none(),
        "listener 已关闭，无残留句柄"
    );

    // 后续蓝图完全正常
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = match v {
                    Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
                    other => panic!("{other:?}"),
                };
                syscall(
                    DataOp::Write {
                        fd: wfd,
                        data: b"still-alive".to_vec(),
                    },
                    vec![wr(wfd)],
                    move |_| {
                        syscall(
                            DataOp::Read {
                                fd: rfd,
                                len: "still-alive".len(),
                            },
                            vec![rd(rfd)],
                            Action::Pure,
                        )
                    },
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"still-alive".to_vec()));
}

/// Timeout 触发后 undo 栈/registry 状态完整：Timeout 前的 Write 已生效且
/// undo 保留；Timeout 不吞效果、不破坏状态；随后 Replace 全链恢复。
#[test]
fn err_timeout_keeps_undo_stack_and_registry() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("tmo.txt");
    std::fs::write(&pa, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            vec![wr_path(pa.clone())],
            move |v| {
                let fd = fd_of(&v);
                syscall(
                    DataOp::Write {
                        fd,
                        data: b"CHANGED!".to_vec(),
                    },
                    vec![wr(fd)],
                    move |_| Action::Timeout {
                        action: Box::new(Action::Sleep {
                            duration: Duration::from_secs(10),
                            next: Box::new(Action::Pure),
                        }),
                        duration: Duration::from_millis(50),
                        on_timeout: Box::new(Action::Pure(Value::U64(42))),
                    },
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::U64(42), "Timeout 触发后执行 on_timeout");

    // 状态完整：Write 的 undo 未被 Timeout 吞掉、写已生效
    assert_eq!(rt.undo_stack().len(), 1, "Write 的 undo 保留");
    assert_eq!(std::fs::read(&pa).unwrap(), b"CHANGED!", "写已生效");

    // 随后 Replace：先 recover（恢复文件）再执行 target
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"original",
        "Timeout 后撤销链仍完整"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 5：值流
// ══════════════════════════════════════════════════════════════════════

/// and_then 5 层嵌套：fd 值经 5 层闭包贯穿（open→write→seek→read→close），
/// 最终读回内容与写入一致——值流组合子在真实执行路径上无丢失。
#[test]
fn flow_and_then_5_level_fd_chain() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("flow.txt");
    let payload: Vec<u8> = b"five-level-fd-flow".to_vec();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let plen = payload.len();
    let payload_in = payload.clone();
    let bp = and_then(open_file(pa, rw_flags()), move |v| {
        let fd = fd_of(&v);
        and_then(
            syscall(
                DataOp::Write {
                    fd,
                    data: payload_in.clone(),
                },
                vec![wr(fd)],
                Action::Pure,
            ),
            move |_| {
                and_then(
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
                        and_then(
                            syscall(DataOp::Read { fd, len: plen }, vec![rd(fd)], Action::Pure),
                            move |v| {
                                and_then(
                                    syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure),
                                    move |_| Action::Pure(v),
                                )
                            },
                        )
                    },
                )
            },
        )
    });
    let v = rt.run_blocking(bp).unwrap();
    assert_eq!(v, Value::Bytes(payload), "5 层 and_then 后 fd 值流无丢失");
}

/// Scope 嵌套 3 层：最内层出错（NotFound）→ 3 层 cwd 全部恢复（finally
/// 语义）；成功路径同样恢复。
#[test]
fn flow_scope_3_level_cwd_restore() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let before = rt.context().cwd.clone();

    let inner_err = syscall(
        DataOp::Read {
            fd: 999_999,
            len: 1,
        },
        vec![rd(999_999)],
        Action::Pure,
    );
    let bp = Action::Scope {
        base: PathBuf::from("lvl1"),
        inner: Box::new(Action::Scope {
            base: PathBuf::from("lvl2"),
            inner: Box::new(Action::Scope {
                base: PathBuf::from("lvl3"),
                inner: Box::new(inner_err),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    let e = rt.run_blocking(bp).unwrap_err();
    assert!(matches!(e, SysError::NotFound));
    assert_eq!(
        rt.context().cwd,
        before,
        "3 层嵌套 Scope 出错后 cwd 全部恢复"
    );

    // 成功路径
    let inner_ok = syscall(DataOp::GetTime, vec![], Action::Pure);
    let bp = Action::Scope {
        base: PathBuf::from("a"),
        inner: Box::new(Action::Scope {
            base: PathBuf::from("b"),
            inner: Box::new(Action::Scope {
                base: PathBuf::from("c"),
                inner: Box::new(inner_ok),
                next: Box::new(|_| Action::Pure(Value::Unit)),
            }),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    };
    rt.run_blocking(bp).unwrap();
    assert_eq!(rt.context().cwd, before, "成功路径 3 层 cwd 同样恢复");
}

/// Alloc 大块（1MB）后 Replace 释放：Replace 前后运行时状态干净、可继续分配。
#[test]
fn flow_alloc_1mb_then_replace() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Alloc {
            len: 1024 * 1024,
            next: Box::new(|v| match v {
                Value::Bytes(b) => Action::Pure(Value::U64(b.len() as u64)),
                _other => Action::Pure(Value::Unit),
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1024 * 1024), "1MB 分配返回正确长度");

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "Replace 后撤销栈空");

    // 释放后运行时仍可继续分配与执行
    let v = rt
        .run_blocking(Action::Alloc {
            len: 8,
            next: Box::new(|v| match v {
                Value::Bytes(b) => Action::Pure(Value::U64(b.len() as u64)),
                _other => Action::Pure(Value::Unit),
            }),
        })
        .unwrap();
    assert_eq!(v, Value::U64(8));
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 6：确定性
// ══════════════════════════════════════════════════════════════════════

/// 轨迹观察执行器：真实 TokioExecutor + 逐 op 记录（非 mock——全部行为
/// 委托真实执行器，仅追加只读观察）。
struct LoggingExecutor {
    inner: TokioExecutor,
    log: Arc<Mutex<Vec<String>>>,
}

impl SyscallExecutor for LoggingExecutor {
    fn execute<'a>(
        &'a mut self,
        op: &'a DataOp,
        registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push(format!("{op:?}"));
            self.inner.execute(op, registry).await
        })
    }
}

fn traj_blueprint(pa: PathBuf, flags: OpenFlags) -> Action {
    syscall(
        DataOp::Open {
            path: pa.clone(),
            flags,
        },
        vec![wr_path(pa.clone())],
        move |v| {
            let fd = fd_of(&v);
            // 冲突 Fork（同 fd 双写）→ 顺序路径：轨迹确定（left→right）
            Action::Fork {
                left: Box::new(syscall(
                    DataOp::Write {
                        fd,
                        data: b"L".to_vec(),
                    },
                    vec![wu(fd)],
                    Action::Pure,
                )),
                right: Box::new(syscall(
                    DataOp::Write {
                        fd,
                        data: b"R".to_vec(),
                    },
                    vec![wu(fd)],
                    Action::Pure,
                )),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }
        },
    )
}

fn run_traced(pa: &Path, flags: &OpenFlags) -> (Vec<String>, Result<Value, SysError>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let ex = LoggingExecutor {
        inner: TokioExecutor::new(),
        log: Arc::clone(&log),
    };
    let mut rt = Runtime::new(Box::new(ex));
    let v = rt.run_blocking(traj_blueprint(pa.to_path_buf(), *flags));
    let traj = log.lock().unwrap().clone();
    (traj, v)
}

/// 同一含 Fork 的蓝图两次执行（各自全新 Runtime）：op 轨迹逐位一致、
/// 结果一致——顺序路径执行确定性（无哈希序/集合序/线程序泄漏）。
#[test]
fn det_fork_sequential_trajectory_identical_twice() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("traj.txt");
    std::fs::write(&pa, b"seed").unwrap();
    let flags = OpenFlags {
        read: true,
        write: true,
        ..Default::default()
    };

    let (t1, r1) = run_traced(&pa, &flags);
    let (t2, r2) = run_traced(&pa, &flags);

    assert_eq!(t1, t2, "含 Fork 蓝图两次执行 op 轨迹逐位一致（顺序路径）");
    assert_eq!(r1, r2, "两次执行结果一致");
    assert_eq!(t1.len(), 3, "轨迹 = Open + 两次 Write");
    // 物理效果：写入发生在游标处（open 后 0）——"L"@0 → "Led"，"R"@1 → "LRed"
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"LRed",
        "顺序路径物理效果确定（L 后 R）"
    );
}

/// GetTime 存在性：两次调用值可以不同（墙上时钟非确定性在契约内），
/// 但类型必须稳定一致（U64）。
#[test]
fn det_gettime_type_stable() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v1 = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    let v2 = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v1, Value::U64(_)), "第一次 GetTime 返回 U64");
    assert!(matches!(v2, Value::U64(_)), "第二次 GetTime 返回 U64");
    assert!(v1 != Value::Unit && v2 != Value::Unit, "存在性：非 Unit");
}
