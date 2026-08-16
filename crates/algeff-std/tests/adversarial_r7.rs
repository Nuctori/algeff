//! R7 对抗审计（迭代 2-A2，第 2 轮）—— 迭代 1 六项修复的新攻击面：
//! RFC-05 句柄回收 / RFC-06 锚点吸收 / RFC-07 管道双表 / RFC-08/09 取消传播 /
//! R-6 fork_snapshot / DX 层（do_! 宏 + infer_usage）。真实 TokioExecutor 全链路。
//!
//! 审计方法：基线 cb2bbf1（iter2/it2-r7）代码走读 + 行为探测 → 以下断言锁定
//! **当前行为**。审计发现见交付报告 findings（本文件仅做行为锁定，src/ 冻结
//! 不修，CTO 派发修复轮）：
//!
//! - **F-R7-1 [真实缺陷] RFC-07 管道双表修复未合入基线**：迭代 1 声称「未
//!   Dup 管道在 Fork 分支内 IO 成功」，但 `iter1/it1-rfc07` 的 executor 文件式
//!   双表改造（aa0c2bf）从未合入本基线——当前 executor.rs 管道路径仍为
//!   take/put_back 轮换 + `Arc::get_mut`（RFC-07 登记 §9.3.1 描述的原缺陷
//!   形态）。实测：父级 PipeOpen 的管道（未 Dup），Fork 并行/顺序分支内
//!   Read/Write 均 `Err(InvalidInput)`（registry Clone 使 Arc strong_count>1 →
//!   get_mut 失败）。且分支内失败路径的 put_back 把共享 `pipe_reader_fds`/
//!   `pipe_writer_fds` 映射重定向到分支新 fd → 父级同一管道随后 IO 持续
//!   InvalidInput（映射毒化）。§3 两测试锁定该行为。
//! - **F-R7-2 [已登记 R7-A 复现] Timeout 取消飞行中管道 Write → 句柄泄漏**：
//!   轮换型句柄 take→await→put_back 窗口内 future 被取消丢弃 → 注册表条目
//!   丢失（实测 wfd lookup → None）、管道写端被物理关闭。线性标记本身已由
//!   取消传播协议回滚（`rollback_linear_to`，RFC-08/09/12 残余修复生效：
//!   取消后同 fd Write 声明通过 A4）——§4 测试锁定「标记回滚 OK + 句柄泄漏
//!   （R7-A 待修）」两半。
//! - **F-R7-3 [疑似/低] 快照通道 + 轮换型句柄的跨分支映射污染**：分支内
//!   TcpRead 的 put_back 会把共享 `stream_fds` 映射重定向到分支新 fd（父未
//!   合并前不可见）→ 父级原 fd 的 Close 在分支完成后可能 NotFound（时序依赖，
//!   未做确定性断言，注释记录）。
//! - **F-R7-4 [疑似/设计取舍] DX 层 `Dup → Write(fd)` 推断消费 A4 写许可**：
//!   `dx::dup` 的推断默认声明 Write(fd)（对齐冲突检测的保守安全），但天然序列
//!   「dup → 同 fd 写」被 A4 拒绝（InvalidInput）——推断默认与常见用法冲突。
//!   文档承诺「显式声明永远可以覆盖」成立（syscall_with 改 Read 后同 fd 写
//!   可用，§6 已锁定两半）；是否属缺陷由 CTO 裁决（推断默认可否改 Read）。
//! - **F-R7-5 [语义澄清] Close(Own) 后同 fd 任何 usage → A4 InvalidInput，
//!   Replace 后同 fd → 执行器 NotFound**：两错误面均正确（Own 终结 vs
//!   reg.clear），但错误码不同——调用方需区分「Own 终结拒绝」与「句柄失效」
//!   （§3/§1 分别锁定）。
//!
//! 其余五项（RFC-05/RFC-06/RFC-08/09 取消传播/R-6 快照/DX）当前行为**符合
//! 文档承诺**，本文件补既有套件盲区：§1 Replace 后旧 fd 的 Seek/Dup/SendFile/
//! 网络操作面、§2 并行路径多轮右分支分配线性、§4 CANCEL_JOIN_GRACE 两路径 +
//! 飞行中 Write 标记回滚、§5 快照通道 A4/undo 语义、§6 DX 等价性与覆盖冲突。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceHandle, ResourceInner, ResourceUsage,
    Runtime, SysError, TypedResource, Value, WriteOnly,
};
use algeff_macro::do_;
use algeff_std::{dx, TokioExecutor};

// ── 本地辅助（src/ 冻结不可改，测试内复制；与既有对抗套件同约定）──────────

fn rw_flags() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(f) => *f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

fn pair_of(v: &Value) -> (u64, u64) {
    match v {
        Value::List(l) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
    }
}

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(p: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(p)).into_usage()
}
fn rd_path(p: PathBuf) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Path(p)).into_usage()
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

/// 断言 run_blocking 返回 NotFound（失效 fd 的物理操作统一错误面）。
fn expect_not_found(rt: &mut Runtime, action: Action) {
    match rt.run_blocking(action) {
        Err(SysError::NotFound) => {}
        other => panic!("期望 NotFound，得到 {other:?}"),
    }
}

/// 以 Write(path) 声明打开（rw+create），返回 fd。
fn open_write(rt: &mut Runtime, path: PathBuf) -> u64 {
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

// ══════════════════════════════════════════════════════════════════════
// §1 RFC-05 句柄回收盲区：Replace 后旧 fd 的 Seek/Dup/SendFile/网络操作
//    （既有套件已覆盖 Write/Read/Close 与同路径重开、Fork 分支隔离）
// ══════════════════════════════════════════════════════════════════════

/// Replace（recover + reg.clear）后，旧 fd 的**全部**操作面统一失效：
/// 文件面 Seek/Dup/SendFile(input)、网络面 UdpRecvFrom 均 NotFound
/// （registry 是 fd 活性唯一真相；executor 内部映射仍残留旧条目，但任何
/// 经映射直达物理句柄的路径先过 registry 活性校验，RFC-05 修复覆盖全操作面）。
#[test]
fn rfc05_stale_fd_all_ops_fail_after_replace() {
    let dir = tempfile::tempdir().unwrap();
    let fa = dir.path().join("a.txt");
    let fb = dir.path().join("b.txt");
    std::fs::write(&fa, b"AAAA").unwrap();
    std::fs::write(&fb, b"BBBB").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 建句柄：文件 A、文件 B、UDP socket（直存型）。
    let fda = open_write(&mut rt, fa.clone());
    let fdb = open_write(&mut rt, fb.clone());
    let v = rt
        .run_blocking(syscall(
            DataOp::UdpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let udp = fd_of(&v);

    // Replace：recover + reg.clear → 全部句柄失效、next_fd 保留。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.registry().lookup(fda).is_none(), "Replace 后句柄已清");

    // 文件面：Seek（file_fd_stale 路径）、Dup（reg.lookup）、
    // SendFile（input 侧 file_fd_stale）→ NotFound。
    expect_not_found(
        &mut rt,
        syscall(
            DataOp::Seek {
                fd: fda,
                offset: 0,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![rd(fda)],
            Action::Pure,
        ),
    );
    expect_not_found(
        &mut rt,
        syscall(DataOp::Dup { fd: fda }, vec![wr(fda)], Action::Pure),
    );
    expect_not_found(
        &mut rt,
        syscall(
            DataOp::SendFile {
                out: fdb,
                input: fda,
                offset: 0,
                len: 4,
            },
            vec![wr(fdb), rd(fda)],
            Action::Pure,
        ),
    );

    // 网络面：UdpRecvFrom（reg.lookup 直查）→ NotFound。
    expect_not_found(
        &mut rt,
        syscall(
            DataOp::UdpRecvFrom { fd: udp, len: 4 },
            vec![rd(udp)],
            Action::Pure,
        ),
    );

    // 不粘滞：Replace 后同路径重开 + 完整 IO 正常（RFC-05 配套承诺）。
    // 注：文件预置 "AAAA"，重开（rw+create，无 truncate）游标 0 写 "NEW"
    // → "NEWA"（覆盖前三字节，余 A）。
    let fd2 = open_write(&mut rt, fa.clone());
    let v = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd: fd2,
                data: b"NEW".to_vec(),
            },
            vec![wr(fd2)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Unit);
    assert_eq!(
        std::fs::read(&fa).unwrap(),
        b"NEWA",
        "重开后真实写入（游标 0 覆盖）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// §2 RFC-06 锚点吸收：并行路径多轮右分支实际分配 → next_fd 线性
//    （既有套件：registry 级 400 轮精确公式 + Runtime 级顺序路径 400 轮；
//   本测试补**并行路径**（快照通道）多轮右分支真实文件分配）
// ══════════════════════════════════════════════════════════════════════

/// 200 轮**并行** Fork（左右分支开不同文件，无静态冲突 → 真并行 + 快照
/// 通道），右分支每次在全局唯一区间（k<<48）**实际分配**：merge 锚点吸收
/// 后父 next_fd 线性量级（≈ k_max·2^48，无 Σk·2^48 二次项）；全部 fd 全局
/// 唯一（D1 无复用）；末轮 fd 仍可读（映射无覆盖）。
#[test]
fn rfc06_parallel_200_rounds_right_branch_alloc_linear() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut seen = std::collections::HashSet::new();
    for i in 0..200u64 {
        let lp = dir.path().join(format!("par-l-{i:04}.txt"));
        let rp = dir.path().join(format!("par-r-{i:04}.txt"));
        let v = rt
            .run_blocking(Action::Fork {
                left: Box::new(syscall(
                    DataOp::Open {
                        path: lp.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(lp)],
                    Action::Pure,
                )),
                right: Box::new(syscall(
                    DataOp::Open {
                        path: rp.clone(),
                        flags: rw_flags(),
                    },
                    vec![wr_path(rp)],
                    Action::Pure,
                )),
                combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
            })
            .unwrap();
        let (lfd, rfd) = pair_of(&v);
        assert!(seen.insert(lfd), "第 {i} 轮左分支 fd 碰撞（D1 违反）");
        assert!(seen.insert(rfd), "第 {i} 轮右分支 fd 碰撞（D1 违反）");
        assert_ne!(lfd, rfd, "同轮左右分支 fd 互斥（区间预分割）");
    }
    // 线性量级：200 轮右分支实际分配后 next_fd ≈ 200·2^48（< 2^62；
    // 修复前 Σk·2^48 在 ~362 轮即溢出 u64：debug panic / release 回绕复用）。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(
        n < (1u64 << 62),
        "200 轮并行右分支分配后 next_fd 应线性（< 2^62），实测 {n}"
    );

    // 末轮文件读回（映射无覆盖；快照合并后句柄真实可用）。
    let last_l = dir.path().join("par-l-0199.txt");
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: last_l.clone(),
                flags: OpenFlags {
                    read: true,
                    ..Default::default()
                },
            },
            vec![rd_path(last_l)],
            Action::Pure,
        ))
        .unwrap();
    let _ = fd_of(&v);
}

// ══════════════════════════════════════════════════════════════════════
// §3 RFC-07 管道：缺陷锁定（F-R7-1）+ 非分支回归
// ══════════════════════════════════════════════════════════════════════

/// 父级已建管道（**未 Dup**）在**并行** Fork 分支内 Read：
/// 当前行为 = `Err(InvalidInput)`（RFC-07 登记 §9.3.1 的原缺陷形态——分支
/// registry Clone 使管道半端 Arc strong_count>1，`Arc::get_mut` 失败）。
/// 迭代 1 声称的「未 Dup 管道在 Fork 分支内 IO 成功」修复（executor 文件式
/// 双表）**未合入本基线**（F-R7-1）。
#[test]
fn rfc07_pipe_read_in_parallel_fork_locked_invalid_input() {
    let dir = tempfile::tempdir().unwrap();
    let pb = dir.path().join("p-b.txt");
    std::fs::write(&pb, b"BBB").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 父级建好管道（含数据）与对照文件。
    let (rfd, wfd, fdb) = {
        let v = rt
            .run_blocking(syscall(
                DataOp::PipeOpen {
                    flags: Default::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"PIPE".to_vec(),
                        },
                        vec![wr(wfd)],
                        move |_| Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)])),
                    )
                },
            ))
            .unwrap();
        let (rfd, wfd) = pair_of(&v);
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: pb.clone(),
                    flags: OpenFlags {
                        read: true,
                        ..Default::default()
                    },
                },
                vec![rd_path(pb.clone())],
                Action::Pure,
            ))
            .unwrap();
        (rfd, wfd, fd_of(&v))
    };
    let _ = wfd;

    // 并行 Fork：左分支读管道 rfd（与右分支 Read(fdb) 不同资源、无冲突 →
    // 真并行 + 快照通道）。F-R7-1：左分支管道读 → InvalidInput（缺陷锁定）。
    let r = rt.run_blocking(Action::Fork {
        left: Box::new(syscall(
            DataOp::Read { fd: rfd, len: 4 },
            vec![rd(rfd)],
            Action::Pure,
        )),
        right: Box::new(syscall(
            DataOp::Read { fd: fdb, len: 3 },
            vec![rd(fdb)],
            Action::Pure,
        )),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    });
    match r {
        Err(SysError::InvalidInput) => {}
        other => panic!("F-R7-1 行为锁定：并行分支内管道 IO 应 InvalidInput，得到 {other:?}"),
    }
}

/// 父级已建管道（未 Dup）在**顺序**（冲突）Fork 分支内 Read：
/// 同样 `Err(InvalidInput)`（F-R7-1；顺序路径分支 registry 同样 Clone）。
#[test]
fn rfc07_pipe_read_in_sequential_fork_locked_invalid_input() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("s-a.txt");
    std::fs::write(&pa, b"AAA").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let r = rt.run_blocking(syscall(
        DataOp::PipeOpen {
            flags: Default::default(),
        },
        vec![],
        move |v| {
            let (rfd, wfd) = pair_of(&v);
            syscall(
                DataOp::Write {
                    fd: wfd,
                    data: b"S".to_vec(),
                },
                vec![wr(wfd)],
                move |_| Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)])),
            )
        },
    ));
    // 拆两段：先取 rfd/wfd，再构造冲突 Fork。
    let (rfd, wfd) = match r {
        Ok(Value::List(l)) => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("PipeOpen 失败：{other:?}"),
    };
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pa.clone(),
                flags: OpenFlags {
                    read: true,
                    ..Default::default()
                },
            },
            vec![rd_path(pa.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fda = fd_of(&v);
    // 冲突 Fork：左分支声明 wr(fda)（与右分支 Write(fda) 同资源冲突 → 顺序
    // 路径），实际操作是读父管道 rfd → F-R7-1 缺陷触发。
    let r = rt.run_blocking(Action::Fork {
        left: Box::new(syscall(
            DataOp::Read { fd: rfd, len: 4 },
            vec![wr(fda), rd(rfd)],
            Action::Pure,
        )),
        right: Box::new(syscall(
            DataOp::Write {
                fd: fda,
                data: b"R".to_vec(),
            },
            vec![wr(fda)],
            Action::Pure,
        )),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    });
    match r {
        Err(SysError::InvalidInput) => {}
        other => panic!("F-R7-1 行为锁定：顺序分支内管道 IO 应 InvalidInput，得到 {other:?}"),
    }
    let _ = wfd;
}

/// 非分支回归：管道在**父级**全链路 IO + Close 不回归（RFC-07 修复范围外的
/// 正常路径必须保持）：写→读回、Close 后 fd 释放、undo 不回归、关闭后旧
/// fd 操作 NotFound（RFC-05 活性语义对管道同样生效）。
#[test]
fn rfc07_pipe_roundtrip_close_no_regression() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: Default::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                syscall(
                    DataOp::Write {
                        fd: wfd,
                        data: b"hello pipe".to_vec(),
                    },
                    vec![wr(wfd)],
                    move |_| Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)])),
                )
            },
        ))
        .unwrap();
    let (rfd, wfd) = pair_of(&v);
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd, len: 10 },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"hello pipe".to_vec()), "管道写读回");

    // Close 两端：句柄释放、无 undo（关闭不可逆）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Close { fd: wfd },
        vec![ow(wfd)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt.registry().lookup(rfd).is_none(), "读端已释放");
    assert!(rt.registry().lookup(wfd).is_none(), "写端已释放");
    assert!(rt.undo_stack().is_empty(), "管道 IO/Close 不产生 undo");

    // Close 后旧 fd 操作：A4 Own 终结语义——Close 声明 Own(fd) 后该资源任何
    // usage（含 Read）都被 check_linear 拒绝（InvalidInput；区别于 Replace
    // 后 reg.clear 清标记 → 执行器侧 NotFound，见 §1）。两错误面都正确。
    let r = rt.run_blocking(syscall(
        DataOp::Read { fd: rfd, len: 1 },
        vec![rd(rfd)],
        Action::Pure,
    ));
    assert_eq!(
        r.unwrap_err(),
        SysError::InvalidInput,
        "Close(Own) 后同 fd 任何 usage 应被 A4 Own 终结拒绝"
    );
}

// ══════════════════════════════════════════════════════════════════════
// §4 RFC-08/09 取消传播盲区：飞行中 Write 线性标记回滚 + CANCEL_JOIN_GRACE
//    宽限两路径（既有套件：孤儿 Open 不创建、锁立即可重入；本测试补
//    「Timeout 取消飞行中 Write」与宽限 join/丢弃）
// ══════════════════════════════════════════════════════════════════════

/// Timeout 取消**飞行中**管道 Write（256KB ≫ 64KB 缓冲区、无读者 → 阻塞）：
/// - 线性标记已回滚（RFC-08/09/12 残余修复，`rollback_linear_to`）：取消后
///   同 fd 的 Write 声明通过 A4（`check_linear` Ok）——「同路径可重试」的
///   A4 面恢复（resource-notes §12 登记的残余缺口已修复）；
/// - undo 栈无残留（阻塞写未入 undo）；
/// - **句柄泄漏锁定（F-R7-2 = 已登记 R7-A）**：take→await→put_back 窗口内
///   future 被取消丢弃 → wfd 注册表条目丢失（lookup None）——物理句柄随
///   Arc 唯一引用 drop 关闭。记录不修。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn rfc0809_timeout_cancels_inflight_write_linear_rollback() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: Default::default(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let (rfd, wfd) = pair_of(&v);
    let _ = rfd; // 无读者：写端阻塞窗口的唯一前提。

    let inner = syscall(
        DataOp::Write {
            fd: wfd,
            data: vec![0xabu8; 256 * 1024],
        },
        vec![wr(wfd)],
        Action::Pure,
    );
    let blueprint = Action::Timeout {
        action: Box::new(inner),
        duration: Duration::from_millis(30),
        on_timeout: Box::new(Action::Pure(Value::U64(1))),
    };
    let t0 = Instant::now();
    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::U64(1), "on_timeout 生效");
    assert!(
        t0.elapsed() >= Duration::from_millis(450),
        "飞行中 Write 不可取消 → 宽限耗尽后丢弃（elapsed={:?}）",
        t0.elapsed()
    );

    // 线性标记回滚（RFC-08/09/12 残余修复生效）。
    assert!(
        rt.undo_stack().is_empty(),
        "取消路径不产生 undo（阻塞写未入栈）"
    );
    let u = wr(wfd);
    assert!(
        rt.registry().check_linear(&u).is_ok(),
        "取消后 Write 声明应通过 A4（线性标记已回滚，同路径可重试）"
    );

    // F-R7-2（R7-A 已登记）：句柄泄漏锁定——取消丢弃 take 后的管道写端。
    assert!(
        rt.registry().lookup(wfd).is_none(),
        "F-R7-2 行为锁定：取消飞行中轮换型 IO → 注册表句柄泄漏（R7-A 待修）"
    );
}

/// Timeout 取消子树内含 Write 声明（Open rw+create 声明 Write(path)）的
/// 文件路径：取消后同路径以 Write 模式重开成功——「同路径可重试」在文件面
/// 的 A4 语义（线性标记随取消子树回滚，非仅 exec 失败路径）。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn rfc0809_timeout_cancel_file_write_mark_rolled_back_same_path_retry() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("cancel-write.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // inner：Open(rw+create)（声明 Write(path)）→ Sleep(400ms) 阻塞窗口。
    // 30ms 超时 → 取消子树 → Write(path) 标记应被回滚。
    let inner = Action::Sequential {
        current: Box::new(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        )),
        next: Box::new(move |_| Action::Sleep {
            duration: Duration::from_millis(400),
            next: Box::new(|_| Action::Pure(Value::Unit)),
        }),
    };
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(inner),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(9))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(9), "on_timeout 生效");

    // 同路径 Write 模式重开：线性标记已回滚 → 不再 InvalidInput。
    let fd = open_write(&mut rt, p.clone());
    assert!(rt.registry().lookup(fd).is_some(), "重开成功");
}

/// CANCEL_JOIN_GRACE 宽限内 join 路径：并行 Fork 分支阻塞于**可取消**的
/// Sleep（取消广播 → cancellable_sleep 立即醒来 → 循环顶检查 → 快速返回），
/// 分支在宽限（500ms）内完成 join → 整体耗时 ≪ 宽限；孤儿 Open 副作用
/// 不发生（文件不创建）。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn rfc0809_cancel_join_grace_join_path() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("grace-join.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let pa_in = pa.clone();
    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Fork {
                left: Box::new(Action::Sequential {
                    current: Box::new(Action::Sleep {
                        duration: Duration::from_millis(400),
                        next: Box::new(Action::Pure),
                    }),
                    next: Box::new(move |_| {
                        syscall(
                            DataOp::Open {
                                path: pa_in.clone(),
                                flags: rw_flags(),
                            },
                            vec![wr_path(pa_in.clone())],
                            Action::Pure,
                        )
                    }),
                }),
                right: Box::new(Action::Pure(Value::Unit)),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(42))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(42), "on_timeout 生效");
    assert!(
        t0.elapsed() < Duration::from_millis(400),
        "宽限内 join：分支响应取消快速返回（elapsed={:?} < 500ms 宽限）",
        t0.elapsed()
    );
    assert!(
        !pa.exists(),
        "孤儿 Open 不发生（分支在 Sleep 后下一 op 边界中止，文件未创建）"
    );
    assert!(rt.undo_stack().is_empty(), "取消路径不产生 undo");
}

/// CANCEL_JOIN_GRACE 超宽限丢弃路径：并行 Fork 分支阻塞于**不可取消**的
/// UdpRecvFrom（无数据 → executor 内 await，取消广播不打断）→ 耗尽宽限
/// （500ms）后 inner 被丢弃（CANCELLED_ERR）→ on_timeout 生效；父级注册表
/// 不受分支影响（分支注册表未合并）。
/// 注：不可取消分支须选 &self 型操作（UDP lookup 直用）——轮换型句柄
/// （TcpRead/管道）在分支 registry Clone 下 Arc 共享 → get_mut 失败提前
/// InvalidInput（F-R7-1 同族），构造不出阻塞窗口。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn rfc0809_cancel_join_grace_exhausted_path() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // 父级 UDP 绑定（&self 型句柄，分支内 lookup 直用无共享冲突）。
    let udp = {
        let v = rt
            .run_blocking(syscall(
                DataOp::UdpBind {
                    addr: "127.0.0.1:0".parse().unwrap(),
                },
                vec![],
                Action::Pure,
            ))
            .unwrap();
        fd_of(&v)
    };

    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Fork {
                left: Box::new(syscall(
                    DataOp::UdpRecvFrom { fd: udp, len: 8 },
                    vec![rd(udp)],
                    Action::Pure,
                )),
                right: Box::new(Action::Pure(Value::Unit)),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            duration: Duration::from_millis(30),
            on_timeout: Box::new(Action::Pure(Value::U64(7))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(7), "超宽限丢弃路径：on_timeout 生效");
    let elapsed = t0.elapsed();
    assert!(
        elapsed >= Duration::from_millis(450) && elapsed < Duration::from_millis(1400),
        "超宽限丢弃：须耗尽 500ms 宽限（elapsed={elapsed:?}）"
    );
    assert!(rt.undo_stack().is_empty(), "取消路径不产生 undo");

    // 父级不粘滞：同一 Runtime 后续真实 syscall 正常（分支注册表未合并污染），
    // 且 UDP 句柄仍存活（父级 Arc 未被分支丢弃波及）。
    assert!(rt.registry().lookup(udp).is_some(), "父级 UDP 句柄存活");
    let v = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "取消后父级可继续执行");
}

// ══════════════════════════════════════════════════════════════════════
// §5 R-6 fork_snapshot：快照共享状态表 + 分支 fd 不碰撞 + A4/undo 保持
// ══════════════════════════════════════════════════════════════════════

/// 快照与父共享状态表：父分配的文件 fd 在并行 Fork 分支内**立即可见**
/// （fork_snapshot 克隆 files 映射 Arc，O(1) 无句柄复制）。两分支经同一
/// fd 并行 Read（Read∥Read 无冲突 → 真并行），合并读回全部内容；分支 IO
/// 经共享 per-fd 锁推进**共享游标**——EOF 语义已由 executor.rs 单测
/// `fork_snapshot_shares_state_and_enables_branch_channel` 锁定（快照读后
/// 父读 EOF），此处锁定「两分支并行读共享 fd 成功 + 内容完整性」。
#[test]
fn r6_snapshot_parent_fd_visible_branch_parallel_read_shared_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("snap-share.txt");
    std::fs::write(&p, b"ABCDEF").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fd = open_readonly(&mut rt, p.clone());
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(syscall(
                DataOp::Read { fd, len: 3 },
                vec![rd(fd)],
                Action::Pure,
            )),
            right: Box::new(syscall(
                DataOp::Read { fd, len: 3 },
                vec![rd(fd)],
                Action::Pure,
            )),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    let Value::List(parts) = v else {
        panic!("期望 List，得到 {v:?}");
    };
    let mut joined = Vec::new();
    for part in parts {
        match part {
            Value::Bytes(b) => joined.extend(b),
            other => panic!("期望 Bytes，得到 {other:?}"),
        }
    }
    joined.sort();
    assert_eq!(
        joined,
        b"ABCDEF".to_vec(),
        "两分支经共享 fd 并行读，合并读回全部内容（快照可见父 fd）"
    );
    let _ = fd;
}

/// 快照通道下分支 fd 分配**不碰撞父游标**：父先建文件 fd，并行 Fork
/// 右分支在全局唯一区间实际分配 → 合并后父 next_fd 只升到 max（D1 单调），
/// 父继续分配不与任何分支 fd 冲突；分支 fd 读回内容正确（映射无覆盖）。
#[test]
fn r6_snapshot_branch_alloc_does_not_collide_parent_next_fd() {
    let dir = tempfile::tempdir().unwrap();
    let fa = dir.path().join("base.txt");
    let fb = dir.path().join("branch.txt");
    std::fs::write(&fa, b"BASE").unwrap();
    std::fs::write(&fb, b"BRANCH").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let fdbase = open_readonly(&mut rt, fa.clone()); // 父 fd 0
    let (lfd, rfd) = {
        let v = rt
            .run_blocking(Action::Fork {
                left: Box::new(syscall(
                    DataOp::Open {
                        path: fb.clone(),
                        flags: OpenFlags {
                            read: true,
                            ..Default::default()
                        },
                    },
                    vec![rd_path(fb.clone())],
                    Action::Pure,
                )),
                right: Box::new(syscall(
                    DataOp::Open {
                        path: fb.clone(),
                        flags: OpenFlags {
                            read: true,
                            ..Default::default()
                        },
                    },
                    vec![rd_path(fb.clone())],
                    Action::Pure,
                )),
                combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
            })
            .unwrap();
        pair_of(&v)
    };
    assert_ne!(lfd, rfd, "两分支 fd 互斥（全局区间预分割）");
    assert!(lfd > fdbase && rfd > fdbase, "分支 fd 高于父已有 fd");

    // 父继续分配：单调、不与任何已分配 fd 冲突（快照合并归一化）。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(
        n > lfd && n > rfd && n > fdbase,
        "父 next_fd 高于全部已分配 fd（不碰撞）"
    );

    // 分支 fd 读回正确（映射无覆盖）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd, len: 6 },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"BRANCH".to_vec()), "右分支 fd 读回");
}

/// 快照通道下 A4 线性 / undo 语义保持：并行 Fork 分支 Write（自开文件）的
/// 线性消费标记与 undo 经 merge 并入父——父级同路径 Write 声明被 A4 拒绝
/// （InvalidInput），recover（Replace）后内容恢复、标记复位可再写。
#[test]
fn r6_snapshot_a4_linear_and_undo_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let fa = dir.path().join("lin-a.txt");
    let fb = dir.path().join("lin-b.txt");
    std::fs::write(&fa, b"OLD-A").unwrap();
    std::fs::write(&fb, b"OLD-B").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 并行 Fork：左开 a 写 "NEW-A"、右开 b 写 "NEW-B"（异路径 → 真并行）。
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(syscall(
                DataOp::Open {
                    path: fa.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(fa.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"NEW-A".to_vec(),
                        },
                        vec![wr(fd)],
                        Action::Pure,
                    )
                },
            )),
            right: Box::new(syscall(
                DataOp::Open {
                    path: fb.clone(),
                    flags: rw_flags(),
                },
                vec![wr_path(fb.clone())],
                move |v| {
                    let fd = fd_of(&v);
                    syscall(
                        DataOp::Write {
                            fd,
                            data: b"NEW-B".to_vec(),
                        },
                        vec![wr(fd)],
                        Action::Pure,
                    )
                },
            )),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap();
    assert_eq!(v, Value::Unit);
    assert_eq!(std::fs::read(&fa).unwrap(), b"NEW-A", "左分支写落盘");
    assert_eq!(std::fs::read(&fb).unwrap(), b"NEW-B", "右分支写落盘");

    // A4 线性：分支 Write(path) 消费标记并入父 → 父级同路径 Write 声明被拒。
    let u = wr_path(fa.clone());
    assert_eq!(
        rt.registry().check_linear(&u),
        Err(SysError::InvalidInput),
        "分支 Write 线性标记应随 merge 并入父（A4 保持）"
    );
    assert_eq!(rt.undo_stack().len(), 2, "两分支 Write undo 并入父栈");

    // undo：recover（经 Replace）恢复两文件写前内容。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(std::fs::read(&fa).unwrap(), b"OLD-A", "recover 恢复左文件");
    assert_eq!(std::fs::read(&fb).unwrap(), b"OLD-B", "recover 恢复右文件");

    // Replace 复位标记：同路径再写声明通过 A4。
    let u2 = wr_path(fa.clone());
    assert!(
        rt.registry().check_linear(&u2).is_ok(),
        "Replace 后线性标记复位，同路径可重写"
    );
}

// ══════════════════════════════════════════════════════════════════════
// §6 DX 层：do_! 蓝图与手写等价 / infer_usage 显式覆盖冲突 / fd 操作推导
// ══════════════════════════════════════════════════════════════════════

/// do_! 宏构造的蓝图与手写 CPS 链**运行等价**：同一文件 IO 序列
/// （Open→Write→Seek→Read→Close），两构造方式结果值、undo 栈、物理文件
/// 内容全部一致（宏只做 AST 拼接，不引入新语义——DX 哲学承诺 1）。
#[test]
fn dx_do_macro_blueprint_equivalent_to_handwritten() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("dx-a.txt");
    let pb = dir.path().join("dx-b.txt");

    // 构造 A：do_! 宏（资源经 infer_usage 自动推导）。
    let blueprint_macro = do_! {
        let fd = dx::open(&pa, rw_flags());
        dx::write(&fd, b"hello dx".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    // 构造 B：手写 CPS 链（与 do_! 展开同构：and_then 嵌套 + dx::syscall
    // 自动推导——两构造的资源声明同源于 infer_usage）。
    let f = dx::syscall(DataOp::Open {
        path: pb.clone(),
        flags: rw_flags(),
    });
    let blueprint_hand = dx::and_then(f, move |fd| {
        let fd = Value::Fd(fd_of(&fd));
        let w = dx::write(&fd, b"hello dx".to_vec());
        dx::and_then(w, move |_| {
            let s = dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
            dx::and_then(s, move |_| {
                let r = dx::read(&fd, 64);
                dx::and_then(r, move |data| {
                    let c = dx::close(&fd);
                    dx::and_then(c, move |_| Action::Pure(data))
                })
            })
        })
    });

    // 各自独立 Runtime 执行，比对可观察面。
    let mut rt_a = Runtime::new(Box::new(TokioExecutor::new()));
    let va = rt_a.run_blocking(blueprint_macro).unwrap();
    let undo_a = rt_a.undo_stack().len();
    assert_eq!(va, Value::Bytes(b"hello dx".to_vec()), "do_! 结果值");
    assert_eq!(undo_a, 1, "do_! 链：Write 一条 undo");
    assert_eq!(std::fs::read(&pa).unwrap(), b"hello dx", "do_! 物理落盘");

    let mut rt_b = Runtime::new(Box::new(TokioExecutor::new()));
    let vb = rt_b.run_blocking(blueprint_hand).unwrap();
    let undo_b = rt_b.undo_stack().len();
    assert_eq!(vb, Value::Bytes(b"hello dx".to_vec()), "手写结果值");
    assert_eq!(undo_b, 1, "手写链：Write 一条 undo");
    assert_eq!(std::fs::read(&pb).unwrap(), b"hello dx", "手写物理落盘");

    // 两构造的可观察面完全一致（等价值流 + 等量撤销 + 等量副作用）。
    assert_eq!(va, vb, "do_! 与手写结果等价");
    assert_eq!(undo_a, undo_b, "do_! 与手写 undo 等价");
}

/// infer_usage 与显式声明冲突：`dx::syscall_with` 的显式 ResourceSet **完全
/// 覆盖**自动推导——Open(write) 本应推导 Write(path)，显式改声明 Read(path)
/// 后 A4 面按 Read 走（不插消费标记），物理行为不变（真实写建文件）；
/// 随后的同路径 Write 声明（自动推导）不再被 A4 误拒。对照：无覆盖时同路径
/// 二次 Write 声明被 A4 拒绝——证明覆盖确实改写了线性面（文档承诺 2）。
#[test]
fn dx_explicit_override_conflicts_with_inference() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("ovr-a.txt");
    let pb = dir.path().join("ovr-b.txt");

    // 覆盖路径：Open(write) + 显式 Read(path) 声明。
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let a = dx::syscall_with(
        DataOp::Open {
            path: pa.clone(),
            flags: rw_flags(),
        },
        vec![rd_path(pa.clone())],
    );
    let v = rt.run_blocking(a).unwrap();
    let fda = fd_of(&v);
    assert!(pa.exists(), "物理行为不变：写建文件成功");

    // 自动推导（对照面）：Open(write) → Write(path)。
    let Action::Syscall { resources, .. } = dx::syscall(DataOp::Open {
        path: pa.clone(),
        flags: rw_flags(),
    }) else {
        panic!("dx::syscall 应为 Syscall");
    };
    assert_eq!(
        resources,
        vec![wr_path(pa.clone())],
        "自动推导 Open(write) → Write(path)"
    );

    // 覆盖后：同路径自动推导 Write(path) 声明不再被 A4 误拒（覆盖改写了线性面）。
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
    let fdb = fd_of(&v);
    assert_ne!(fda, fdb, "二次打开成功（覆盖声明 Read 未插 Write 标记）");

    // 对照（无覆盖）：两个自动推导 Write(path) 打开同路径 → 第二次 A4 拒绝。
    let mut rt2 = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt2
        .run_blocking(syscall(
            DataOp::Open {
                path: pb.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pb.clone())],
            Action::Pure,
        ))
        .unwrap();
    let _ = fd_of(&v);
    let e = rt2
        .run_blocking(syscall(
            DataOp::Open {
                path: pb.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(pb.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::InvalidInput,
        "无覆盖：同路径二次 Write 声明被 A4 拒绝（对照）"
    );
}

/// do_! + dx 的 fd 操作推导：管道写读回 + **Dup 推导冲突锁定 + 显式覆盖**
/// （DX 层示例编译即语义：宏展开的管道操作真实执行；Dup 的推断默认
/// Write(fd) 会消费 A4 一次性写许可 → 后续同 fd 写被拒（F-R7-4，设计取舍：
/// 默认值可被 syscall_with 覆盖，见 dx.rs 哲学承诺 2）。
#[test]
fn dx_do_macro_pipe_and_dup_semantics() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 管道：dx::pipe_open（空集推导）→ do_! 内 dx::write/read/close。
    let (rfd, wfd) = {
        let v = rt.run_blocking(dx::pipe_open()).unwrap();
        pair_of(&v)
    };
    let blueprint = do_! {
        dx::write(&Value::Fd(wfd), b"pipe-dx".to_vec());
        let data = dx::read(&Value::Fd(rfd), 64);
        dx::close(&Value::Fd(rfd));
        dx::close(&Value::Fd(wfd));
        data
    };
    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"pipe-dx".to_vec()), "do_! 管道写读回");

    // Dup 推导冲突（F-R7-4 行为锁定）：dx::dup 推断 `Write(fd)`（A4 消费），
    // 天然序列 dup → 同 fd 写被 InvalidInput 拒绝（默认值的保守代价）。
    let dir = tempfile::tempdir().unwrap();
    let pc = dir.path().join("dup-conflict.txt");
    let blueprint = do_! {
        let fd = dx::open(&pc, rw_flags());
        let d = dx::dup(&fd);
        dx::write(&fd, b"ab".to_vec());
        dx::close(&fd);
        dx::close(&d);
        Value::Unit
    };
    let e = rt.run_blocking(blueprint).unwrap_err();
    assert_eq!(
        e,
        SysError::InvalidInput,
        "F-R7-4 行为锁定：dx::dup 推断 Write(fd) 消费写许可 → 同 fd 写被 A4 拒绝"
    );

    // 显式覆盖（文档承诺 2）：dup 改声明 Read(Fd(fd)) → 同 fd 写可用，
    // dup 共享同一句柄（写入经原 fd 生效，物理落盘）。
    let p = dir.path().join("dup-ok.txt");
    let blueprint = {
        let f = dx::syscall(DataOp::Open {
            path: p.clone(),
            flags: rw_flags(),
        });
        dx::and_then(f, move |fdv| {
            let fd = fd_of(&fdv);
            let d = dx::syscall_with(DataOp::Dup { fd }, vec![rd(fd)]);
            dx::and_then(d, move |_| {
                let w = dx::write(&Value::Fd(fd), b"ab".to_vec());
                dx::and_then(w, move |_| Action::Pure(Value::Unit))
            })
        })
    };
    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Unit);
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"ab",
        "覆盖 dup 为 Read 声明后：同 fd 写可用且物理落盘（A4 无冲突）"
    );
}

/// 以 Read(path) 声明打开（read-only），返回 fd。
fn open_readonly(rt: &mut Runtime, path: PathBuf) -> u64 {
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: OpenFlags {
                    read: true,
                    ..Default::default()
                },
            },
            vec![rd_path(path)],
            Action::Pure,
        ))
        .unwrap();
    fd_of(&v)
}
