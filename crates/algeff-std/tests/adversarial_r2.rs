//! R2 对抗审计（第 2 轮，E2E 外部行为攻击）。
//!
//! 攻击方法论：与 R1 相同——不 mock、全部经真实 `Runtime` +
//! `TokioExecutor` 全链路（`run_blocking` → `interpret` → 共享执行器通道）。
//! R1 已覆盖：可逆深链/游标、线性绕过、并发 Fork、错误路径 put_back、
//! 值流、确定性。**R2 攻击 R1 未覆盖的新边界**：
//!
//! 1. **新 fd 区间分配**（A2 批 6 修复的边界）：
//!    - 深度 5 不规则 Fork 树 → 全 fd 两两不相交 + 执行器映射读回正确；
//!    - 同一 Runtime 50 轮 Fork 连续执行 → fd 紧凑单调（归一化生效，无爆涨）；
//!    - 1000 个顺序冲突 Fork → 区间序号单调、fd 不碰撞、next_fd 不爆涨；
//!    - **RFC-06（已修复）**：右分支有分配的连续 Fork 曾使 next_fd 二次增长
//!      （每轮 +k·2^48），~360 轮后 u64 溢出（debug panic / release 回绕）——
//!      `ResourceRegistry::merge` 锚点吸收修复（分支 fork_region 根基线回灌父，
//!      后续轮次偏移锚定根基线，父 next_fd 线性增长；见 spec §10 RFC-06）；
//!    - **RFC-07（已修复，迭代 3-A2；攻击面 7 验证）**：管道半端经 Fork registry
//!      Clone（D13）共享 Arc，修复前分支内对管道 IO 时 executor 的 `Arc::get_mut`
//!      失败 → InvalidInput（文件工作对象是 Arc<Mutex<File>>，不受影响）。修复：
//!      executor 管道工作表文件式双表（registry 句柄与工作表存同一
//!      `Arc<Mutex<半端>>`，lock 下 IO 合法、管道不轮换），见
//!      spec/resource-notes.md §10 RFC-07 段与攻击面 7 测试。
//! 2. **arbiter-MutexLock 接入**（A5 批 7 / D16）：
//!    - 声明 Resource::Fd(id) → 静态冲突 → 顺序化 → 两分支都成功；
//!    - 未声明（pdr §18 用户责任边界）→ 真并行 → 竞争失败方 WouldBlock 而非死锁；
//!    - undo 链释放顺序：lock→write→recover → 锁与占坑都释放 → 可重入；
//!    - 显式 MutexUnlock 幂等（双 unlock / undo 后 unlock 均 no-op）。
//! 3. **R1 修复回归**：
//!    - 游标撤销在嵌套 Sequential + Replace 组合下仍成立；
//!    - put_back 错误循环（TcpShutdown try_unwrap 分支）10 次后 fd 仍可用；
//!    - RFC-05 已修复（Replace 后旧 fd 写失败 NotFound）+ 分支级 Replace
//!      不清父级 A4 状态的隔离语义。
//! 4. **Fork 顺序/并行错误路径**：左分支 Err → 右分支副作用发生 → 错误传播
//!    → Catch 捕获后右分支 merge 的句柄仍可见。
//!    （注意：右分支 Open 目标文件必须带 create 能力，否则 Open 自身 NotFound。）
//! 5. **时间面**：Timeout 内 Fork（完成 vs 超时）、**Timeout 内并行 Fork 的
//!    孤儿分支副作用不可撤销（已知缺陷，RFC-05 先例式记录）**、Sleep 0、
//!    GetTime 多上下文类型稳定。
//! 6. **资源计数**：Mmap len 截断边界（len > 文件长、len = 0、大文件、NotFound）。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use algeff_core::{
    AccessMode, Action, DataOp, MmapProt, OpenFlags, Owned, PipeFlags, ReadOnly, Resource,
    ResourceHandle, ResourceInner, ResourceRegistry, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1 相同约定）──────────────

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}
fn rd_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Path(path)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

/// 线性绕过（pdr §18：类型状态包装不能完全阻止绕过，运行时拦截是防线）。
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

/// List([Fd, Fd]) → (fd0, fd1)。
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

/// Open(path, read-only) → Pure(Fd)。
fn open_pure(path: PathBuf) -> Action {
    syscall(
        DataOp::Open {
            path: path.clone(),
            flags: read_only_flags(),
        },
        vec![rd_path(path)],
        Action::Pure,
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

/// 经 Runtime 读回 fd 内容（executor 文件映射）。
fn read_back(rt: &mut Runtime, fd: u64, len: usize) -> Vec<u8> {
    match rt.run_blocking(seek_read_all(fd, len)) {
        Ok(Value::Bytes(b)) => b,
        other => panic!("fd {fd} 读回失败: {other:?}"),
    }
}

/// 断言 fd 列表两两不同且全部可 lookup。
fn assert_disjoint_lookupable(fds: &[u64], reg: &mut ResourceRegistry) {
    let mut uniq = std::collections::HashSet::new();
    for fd in fds {
        assert!(uniq.insert(*fd), "fd 重复: {fd}，全部: {fds:?}");
        assert!(reg.lookup(*fd).is_some(), "fd {fd} 句柄不可 lookup");
    }
    assert_eq!(uniq.len(), fds.len());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1a：深层嵌套（深度 5）不规则 Fork 树 → 全 fd 两两不相交 +
// 执行器映射正确（读回内容）。R1/批 6 只验证了 disjoint+lookup（Mock），
// 本测试在真实 TokioExecutor + 真实文件上验证 fd→文件 映射未被并发覆盖。
// ══════════════════════════════════════════════════════════════════════

enum Shape {
    Leaf,
    Fork(Box<Shape>, Box<Shape>),
}

/// 固定不规则形状，深度 5、11 个叶（DFS 序 = 叶 id 0..10）：
/// Fork( Fork( Fork(a,b), Fork(c, Fork(d,e)) ),
///       Fork( Fork(f,g), Fork(h, Fork(Fork(i,j), k)) ) )
fn deep_shape() -> Shape {
    Shape::Fork(
        Box::new(Shape::Fork(
            Box::new(Shape::Fork(Box::new(Shape::Leaf), Box::new(Shape::Leaf))),
            Box::new(Shape::Fork(
                Box::new(Shape::Leaf),
                Box::new(Shape::Fork(Box::new(Shape::Leaf), Box::new(Shape::Leaf))),
            )),
        )),
        Box::new(Shape::Fork(
            Box::new(Shape::Fork(Box::new(Shape::Leaf), Box::new(Shape::Leaf))),
            Box::new(Shape::Fork(
                Box::new(Shape::Leaf),
                Box::new(Shape::Fork(
                    Box::new(Shape::Fork(Box::new(Shape::Leaf), Box::new(Shape::Leaf))),
                    Box::new(Shape::Leaf),
                )),
            )),
        )),
    )
}

/// 形状 → Action：DFS 分配叶路径（全树叶路径互异 → 所有 Fork 均并行），
/// combine 逐层 List 拼接（保持 DFS 序）。
fn shape_to_action(shape: &Shape, files: &[PathBuf], next: &mut usize) -> Action {
    match shape {
        Shape::Leaf => {
            let p = files[*next].clone();
            *next += 1;
            open_pure(p)
        }
        Shape::Fork(l, r) => Action::Fork {
            left: Box::new(shape_to_action(l, files, next)),
            right: Box::new(shape_to_action(r, files, next)),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        },
    }
}

/// DFS 展平 combine 结果，收集叶 Fd（保持叶序）。
fn flatten_fds(v: &Value, out: &mut Vec<u64>) {
    match v {
        Value::Fd(f) => out.push(*f),
        Value::List(l) => {
            for x in l {
                flatten_fds(x, out);
            }
        }
        other => panic!("期望 Fd/List，得到 {other:?}"),
    }
}

#[test]
fn fd_deep_fork_tree_depth5_disjoint_and_readback() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut contents = Vec::new();
    for i in 0..11u8 {
        let p = dir.path().join(format!("leaf-{i}.txt"));
        let c = format!("content-{i:02}").into_bytes();
        std::fs::write(&p, &c).unwrap();
        files.push(p);
        contents.push(c);
    }

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut next = 0usize;
    let action = shape_to_action(&deep_shape(), &files, &mut next);
    assert_eq!(next, 11, "11 个叶");

    let v = rt.run_blocking(action).unwrap();
    let mut fds = Vec::new();
    flatten_fds(&v, &mut fds);
    assert_eq!(fds.len(), 11, "全部叶 fd 到达 combine");
    assert_disjoint_lookupable(&fds, rt.registry());

    // 执行器映射正确：每 fd 读回内容与其路径文件一致（并发分配无覆盖）。
    for (fd, content) in fds.iter().zip(contents.iter()) {
        let got = read_back(&mut rt, *fd, content.len());
        assert_eq!(&got, content, "fd {fd} 应指向对应文件内容（映射未被覆盖）");
    }

    // D1：合并后父继续分配不与任何分支 fd 冲突。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(fds.iter().all(|&f| n > f), "父 next_fd 高于全部已分配 fd");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1b：同一 Runtime 50 轮 Fork 连续执行 → fd 紧凑单调 + 无碰撞 +
// next_fd 不爆涨（右分支未分配时归一化生效）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn fd_multi_round_fork_50_rounds_monotonic_no_blowup() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut contents = Vec::new();
    for i in 0..50u8 {
        let p = dir.path().join(format!("round-{i:02}.txt"));
        let c = format!("round-{i:02}").into_bytes();
        std::fs::write(&p, &c).unwrap();
        files.push(p);
        contents.push(c);
    }

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut fds = Vec::new();
    for i in 0..50u64 {
        let path = files[i as usize].clone();
        let v = rt
            .run_blocking(Action::Fork {
                left: Box::new(open_pure(path)),
                right: Box::new(Action::Pure(Value::Unit)),
                combine: Box::new(|l, _| Action::Pure(l)),
            })
            .unwrap();
        let fd = fd_of(&v);
        assert_eq!(fd, i, "第 {i} 轮 fd 应紧凑单调（归一化生效，不爆涨）");
        fds.push(fd);
    }
    // next_fd 未被任何预留区间抬高：下一分配恰为 50。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert_eq!(n, 50, "50 轮后 next_fd 应为 50（右分支未分配区间全部收敛）");

    for (fd, content) in fds.iter().zip(contents.iter()) {
        assert_eq!(&read_back(&mut rt, *fd, content.len()), content);
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1c：连续 1000 个顺序冲突 Fork → 区间序号单调消耗（无溢出/回绕）、
// fd 不碰撞、next_fd 不爆涨（归一化：右分支未分配 → 区间收敛回基线）。
// 每轮：父级 PipeOpen（2 fd）+ Open 文件（1 fd）→ 冲突 Fork（同文件 fd
// 双写，顺序路径）。分支冲突负载仍用文件（RFC-07 修复前为绕开管道半端共享
// Arc 的 `Arc::get_mut` 失败而改用；修复后管道亦可作负载，本测试保持文件
// 以保留既有 fd 记账断言；管道分支内 IO 由攻击面 7 覆盖）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn fd_1000_conflict_forks_region_seq_and_fd_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut files = Vec::new();
    for i in 0..1000u64 {
        let p = dir.path().join(format!("cf-{i:04}.txt"));
        let p_in = p.clone();
        let v = rt
            .run_blocking(syscall(
                DataOp::PipeOpen {
                    flags: PipeFlags::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    // 每轮再开一个文件：分支以文件 fd 为冲突资源。
                    syscall(
                        DataOp::Open {
                            path: p_in.clone(),
                            flags: rw_flags(),
                        },
                        vec![wr_path(p_in.clone())],
                        move |v| {
                            let ffd = fd_of(&v);
                            // 同文件 fd 双写 → 静态冲突 → 顺序路径（left→right）
                            Action::Fork {
                                left: Box::new(syscall(
                                    DataOp::Write {
                                        fd: ffd,
                                        data: b"L".to_vec(),
                                    },
                                    vec![wu(ffd)],
                                    Action::Pure,
                                )),
                                right: Box::new(syscall(
                                    DataOp::Write {
                                        fd: ffd,
                                        data: b"R".to_vec(),
                                    },
                                    vec![wu(ffd)],
                                    Action::Pure,
                                )),
                                combine: Box::new(move |_, _| {
                                    Action::Pure(Value::List(vec![
                                        Value::Fd(rfd),
                                        Value::Fd(wfd),
                                        Value::Fd(ffd),
                                    ]))
                                }),
                            }
                        },
                    )
                },
            ))
            .unwrap();
        let (rfd, wfd, ffd) = match &v {
            Value::List(l) if l.len() == 3 => (fd_of(&l[0]), fd_of(&l[1]), fd_of(&l[2])),
            other => panic!("期望 List([Fd, Fd, Fd])，得到 {other:?}"),
        };
        assert_eq!(
            (rfd, wfd, ffd),
            (3 * i, 3 * i + 1, 3 * i + 2),
            "第 {i} 轮 fd 应紧凑（右分支未分配，区间归一化；管道 2 + 文件 1）"
        );
        files.push(ffd);
    }
    // next_fd 不爆涨：恰为 3000（1000 轮 × 3 fd）。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert_eq!(n, 3000, "1000 轮后 next_fd 应为 3000（无区间爆涨/回绕）");

    // 第 0 号与第 999 号文件内容 "LR"（顺序路径 left 写 L、right 写 R），读回
    // 验证 1000 轮区间序号消耗后映射与数据无碰撞。
    let c0 = read_back(&mut rt, files[0], 2);
    assert_eq!(c0, b"LR".to_vec(), "第 0 轮文件顺序双写");
    let c999 = read_back(&mut rt, files[999], 2);
    assert_eq!(c999, b"LR".to_vec(), "第 999 轮文件顺序双写");
    // 每轮左/右分支各一个 Write undo 经顺序路径直接压入父栈（A2 批 6 merge）。
    assert_eq!(
        rt.undo_stack().len(),
        2000,
        "1000 轮 × 2 个 Write undo 入父栈"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1d【RFC-06 已修复】：右分支有分配的连续 Fork —— `ResourceRegistry`
// merge 锚点吸收（分支 fork_region 根基线回灌父，后续轮次偏移锚定根基线）
// 后，父 next_fd 线性增长（每轮恰跨一个 2^48 区间），不再 Σk·2^48 二次增长
// （~362 轮 u64 溢出，debug panic / release 回绕 → fd 复用，违反 D1）。
// 确定性回归门（精确线性公式）见 resource.rs 单测
// `merge_right_branch_alloc_anchors_base_no_quadratic_growth`。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn fd_region_right_branch_alloc_linear_not_quadratic() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut seen = std::collections::HashSet::new();
    for _ in 0..30u64 {
        let v = rt
            .run_blocking(syscall(
                DataOp::PipeOpen {
                    flags: PipeFlags::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    // 左分支声明 wu(wfd) 与右分支 rd(wfd) 冲突 → 顺序路径；
                    // 左分支 op 改用 PipeOpen（新建管道，不与父管道 Arc 共享——
                    // 分支内对父管道半端 IO 会触发 RFC-07 的 Arc::get_mut 失败）；
                    // 右分支 PipeOpen 在 k<<48 区间**实际分配 2 个 fd**（修复后
                    // 偏移锚定根基线，父 next_fd 线性增长）。
                    Action::Fork {
                        left: Box::new(syscall(
                            DataOp::PipeOpen {
                                flags: PipeFlags::default(),
                            },
                            vec![wu(wfd)],
                            Action::Pure,
                        )),
                        right: Box::new(syscall(
                            DataOp::PipeOpen {
                                flags: PipeFlags::default(),
                            },
                            vec![rd(wfd)],
                            Action::Pure,
                        )),
                        combine: Box::new(move |_, _| {
                            Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)]))
                        }),
                    }
                },
            ))
            .unwrap();
        let (rfd, wfd) = pair_of(&v);
        assert!(seen.insert(rfd), "父管道 rfd 碰撞（D1）");
        assert!(seen.insert(wfd), "父管道 wfd 碰撞（D1）");
    }
    // 30 轮实际分配 30×4 = 120 个 fd；修复后右分支 fd 落在锚定根基线的独立
    // 区间（[k·2^48, (k+1)·2^48)），父 next_fd 线性量级（≈ k_max·2^48；
    // 修复前 Σk·2^48 ≥ 2^56 且随轮次发散）。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(
        n < (1u64 << 62),
        "30 轮右分支实际分配后 next_fd 应保持线性量级（< 2^62），实测 {n}"
    );
}

/// RFC-06 已修复（回归）：右分支有分配的连续 Fork 400 轮 —— debug 不再
/// ~362 轮溢出 panic，release 不再回绕复用 fd。修复前 Σ_{k≤n} k·2^48 在
/// n≈362 轮溢出 u64（debug panic / release 回绕 → fd 碰撞，违反 D1）；修复
/// 后 merge 锚点吸收使每轮父 next_fd 线性 +2^48（区间锚定根基线），400 轮
/// ≈ 2^57 无溢出，父管道 fd 逐轮唯一。
#[test]
fn fd_region_seq_no_overflow_or_reuse_400_rounds() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut seen = std::collections::HashSet::new();
    for _ in 0..400u64 {
        let v = rt
            .run_blocking(syscall(
                DataOp::PipeOpen {
                    flags: PipeFlags::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    // 左分支与右分支同 PipeOpen（分支内新建管道不与父管道半端
                    // 共享；两分支都实际分配 → 右分支 k<<48 区间分配后，merge
                    // 锚点吸收保证下轮偏移锚定根基线，父 next_fd 线性增长）。
                    Action::Fork {
                        left: Box::new(syscall(
                            DataOp::PipeOpen {
                                flags: PipeFlags::default(),
                            },
                            vec![wu(wfd)],
                            Action::Pure,
                        )),
                        right: Box::new(syscall(
                            DataOp::PipeOpen {
                                flags: PipeFlags::default(),
                            },
                            vec![rd(wfd)],
                            Action::Pure,
                        )),
                        combine: Box::new(move |_, _| {
                            Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)]))
                        }),
                    }
                },
            ))
            .expect("round");
        let (rfd, wfd) = pair_of(&v);
        assert!(seen.insert(rfd), "父管道 rfd 碰撞（D1 违反）");
        assert!(seen.insert(wfd), "父管道 wfd 碰撞（D1 违反）");
    }
    // 400 轮右分支实际分配后 next_fd 线性量级（≈ k_max·2^48 ≈ 2^57）：修复
    // 前 Σk·2^48 在 ~362 轮即溢出（debug panic / release 回绕 fd 复用）。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(
        n < (1u64 << 62),
        "400 轮后 next_fd 应保持线性量级（< 2^62），实测 {n}"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2a：双任务争用同一 MutexLock id 且**声明了** Resource::Fd(id)
// （Write）→ 静态冲突检测 → 顺序化（两分支都成功，无 WouldBlock 无死锁）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn arb_declared_fork_same_lock_id_serialized() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // 分支内锁 id=7 声明 wu(Fd(7))（与执行器仲裁键 Resource::Fd(7) 对齐）。
    // 注意：unlock 必须声明 rd —— Write 声明会被 A4 消费（每资源至多一次）。
    let lock_unlock = |id: u64| Action::Sequential {
        current: Box::new(syscall(
            DataOp::MutexLock { id },
            vec![wu(id)],
            Action::Pure,
        )),
        next: Box::new(move |_| syscall(DataOp::MutexUnlock { id }, vec![rd(id)], Action::Pure)),
    };
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(lock_unlock(7)),
            right: Box::new(lock_unlock(7)),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap();
    assert_eq!(
        v,
        Value::Unit,
        "声明冲突 → 顺序化：两分支 lock+unlock 均成功"
    );
    assert_eq!(rt.undo_stack().len(), 2, "两把锁的 undo 均入栈");

    // 锁与占坑都已释放（显式 unlock）→ 同一 id 可重入。
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 7 },
        vec![rd(7)],
        Action::Pure,
    ))
    .unwrap();

    // Replace：两个 undo 均为幂等 no-op（显式 unlock 已释放）→ 栈清空。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    // 重入仍然成立（undo 释放顺序不破坏可重入性）。
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 7 },
        vec![rd(7)],
        Action::Pure,
    ))
    .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2b：双任务争用同一 MutexLock id 且**未声明**（pdr §18 用户责任
// 边界：绕过声明）→ 静态层放行（真并行）→ 动态层仲裁：竞争失败方有限
// 重试后 WouldBlock 而非死锁（A7 / R-1 缓解验证）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn arb_undeclared_contention_wouldblock_no_deadlock() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // 两分支 MutexLock(9) 资源声明为空 → fork_conflict 放行 → 真并行 →
    // 共享执行器的 arbiter 独占占坑：至多一个分支成功，败者 8×1ms 有限
    // 重试后 WouldBlock（绝不挂起等待）。
    let err = rt
        .run_blocking(Action::Fork {
            left: Box::new(syscall(DataOp::MutexLock { id: 9 }, vec![], Action::Pure)),
            right: Box::new(syscall(DataOp::MutexLock { id: 9 }, vec![], Action::Pure)),
            combine: Box::new(|_, _| Action::Pure(Value::Unit)),
        })
        .unwrap_err();
    assert_eq!(
        err,
        SysError::WouldBlock,
        "竞争失败方应 WouldBlock（非死锁）"
    );

    // 胜者的 undo 已合并回父（含占坑释放逆操作）。
    assert_eq!(rt.undo_stack().len(), 1, "胜者锁的 undo 合并回父");

    // recover（Replace 路径）→ 物理锁与占坑都释放 → 同 id 可重入。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 9 },
        vec![rd(9)],
        Action::Pure,
    ))
    .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2c：undo 链中 MutexLock 释放顺序 —— lock→write→recover →
// 锁与占坑都释放 → 可重入；文件内容同步恢复。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn arb_lock_write_recover_releases_all_reentrant() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("lock-write.txt");
    let original: Vec<u8> = b"hello world".to_vec();
    std::fs::write(&pa, &original).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

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

    // lock → write：undo 栈序 [write_undo, lock_undo]（LIFO recover 先写后锁）。
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 5 },
        vec![rd(5)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"X".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    eprintln!(
        "DBG arb: fd={fd} disk={:?} via_executor={:?}",
        String::from_utf8_lossy(&std::fs::read(&pa).unwrap()),
        String::from_utf8_lossy(&read_back(&mut rt, fd, 11))
    );
    assert_eq!(rt.undo_stack().len(), 2);
    assert_eq!(std::fs::read(&pa).unwrap(), b"Xello world");

    // Replace → recover：文件恢复 + 锁 undo 释放物理锁与 arbiter 占坑。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), original, "写撤销生效");

    // 可重入：同 id 再次加锁成功（占坑已释放）+ 文件重开可写。
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 5 },
        vec![rd(5)],
        Action::Pure,
    ))
    .unwrap();
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
    let fd2 = fd_of(&v);
    assert!(fd2 > fd, "fd 单调不复用（D1）");
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd2,
            data: b"Y".to_vec(),
        },
        vec![wr(fd2)],
        Action::Pure,
    ))
    .unwrap();
    eprintln!(
        "DBG arb2: fd2={fd2} disk={:?} via_executor={:?} undo_len={}",
        String::from_utf8_lossy(&std::fs::read(&pa).unwrap()),
        String::from_utf8_lossy(&read_back(&mut rt, fd2, 11)),
        rt.undo_stack().len()
    );
    assert_eq!(std::fs::read(&pa).unwrap(), b"Yello world");
}

/// 显式 MutexUnlock 幂等链：lock → unlock → lock → unlock → undo（recover
/// 路径）全链 no-op，锁不泄漏、可反复重入。
#[test]
fn arb_explicit_unlock_reentrant_and_idempotent() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    for _ in 0..2 {
        rt.run_blocking(syscall(
            DataOp::MutexLock { id: 6 },
            vec![rd(6)],
            Action::Pure,
        ))
        .unwrap();
        rt.run_blocking(syscall(
            DataOp::MutexUnlock { id: 6 },
            vec![rd(6)],
            Action::Pure,
        ))
        .unwrap();
    }
    // 双 unlock（无对应锁）幂等 no-op（第 3 次 unlock）。
    rt.run_blocking(syscall(
        DataOp::MutexUnlock { id: 6 },
        vec![rd(6)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        rt.undo_stack().len(),
        2,
        "两把锁的 undo 在栈（unlock 不压栈）"
    );

    // Replace → recover：undo 的 slot take 与 arbiter release 均为幂等 no-op。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    // 全链后仍可重入。
    rt.run_blocking(syscall(
        DataOp::MutexLock { id: 6 },
        vec![rd(6)],
        Action::Pure,
    ))
    .unwrap();
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3a：R1 游标撤销回归 —— 嵌套 Sequential + Replace 组合下，
// 两文件的写撤销必须同时恢复**内容与游标**（A6 双态：w;w̄ = 1）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn r1_cursor_undo_nested_seq_replace() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("cur-a.txt");
    let pb = dir.path().join("cur-b.txt");
    let orig_a: Vec<u8> = b"hello world".to_vec();
    let orig_b: Vec<u8> = b"0123456789".to_vec();
    std::fs::write(&pa, &orig_a).unwrap();
    std::fs::write(&pb, &orig_b).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let pb_in = pb.clone();
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
                let fda = fd_of(&v);
                syscall(
                    DataOp::Open {
                        path: pb_in.clone(),
                        flags: OpenFlags {
                            read: true,
                            write: true,
                            ..Default::default()
                        },
                    },
                    vec![wr_path(pb_in.clone())],
                    move |v| Action::Pure(Value::List(vec![Value::Fd(fda), v])),
                )
            },
        ))
        .unwrap();
    let (fda, fdb) = pair_of(&v);

    // 嵌套 Sequential：内层 current = Sequential{ Seek(fda,0) → Write "XY" }，
    // 外层 next = Write(fdb, "PQ") → Replace。RFC-05 修复后 Replace =
    // recover + reg.clear() 在 target 执行**前**完成——旧 fd 已失效，target
    // 不得再触碰（旧实现 target 内 Seek 旧 fd 观察游标；修复后该路径 NotFound）。
    rt.run_blocking(Action::Sequential {
        current: Box::new(Action::Sequential {
            current: Box::new(syscall(
                DataOp::Seek {
                    fd: fda,
                    offset: 0,
                    whence: std::io::SeekFrom::Start(0),
                },
                vec![rd(fda)],
                move |_| {
                    syscall(
                        DataOp::Write {
                            fd: fda,
                            data: b"XY".to_vec(),
                        },
                        vec![wr(fda)],
                        Action::Pure,
                    )
                },
            )),
            next: Box::new(move |_| {
                syscall(
                    DataOp::Write {
                        fd: fdb,
                        data: b"PQ".to_vec(),
                    },
                    vec![wr(fdb)],
                    move |_| Action::Replace {
                        target: Box::new(Action::Pure(Value::Unit)),
                    },
                )
            }),
        }),
        next: Box::new(Action::Pure),
    })
    .unwrap();

    // 双写生效后 Replace：内容恢复 + 旧 fd 失效（RFC-05）+ 路径重开游标为 0
    // （世界恢复至执行前，A6 双态）。
    assert_eq!(std::fs::read(&pa).unwrap(), orig_a, "a 内容恢复");
    assert_eq!(std::fs::read(&pb).unwrap(), orig_b, "b 内容恢复");
    assert!(rt.undo_stack().is_empty(), "Replace 后撤销栈空");

    // RFC-05：Replace 后旧 fd 任何操作均 NotFound（registry 活性唯一真相，
    // lin_stale_fd_read_close_fail_after_replace 的单 fd 变体；本测试为嵌套
    // Sequential + 双文件变体）。
    let e = rt
        .run_blocking(syscall(
            DataOp::Seek {
                fd: fda,
                offset: 0,
                whence: std::io::SeekFrom::Current(0),
            },
            vec![rd(fda)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound, "Replace 后旧 fd 失效（NotFound）");

    // A6 游标维：重开新 fd 后游标为写前位置 0（世界恢复至执行前）。
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
                let fda2 = fd_of(&v);
                syscall(
                    DataOp::Open {
                        path: pb.clone(),
                        flags: OpenFlags {
                            read: true,
                            write: true,
                            ..Default::default()
                        },
                    },
                    vec![wr_path(pb.clone())],
                    move |v| {
                        let fdb2 = fd_of(&v);
                        Action::Sequential {
                            current: Box::new(syscall(
                                DataOp::Seek {
                                    fd: fda2,
                                    offset: 0,
                                    whence: std::io::SeekFrom::Current(0),
                                },
                                vec![rd(fda2)],
                                move |pos_a| {
                                    syscall(
                                        DataOp::Seek {
                                            fd: fdb2,
                                            offset: 0,
                                            whence: std::io::SeekFrom::Current(0),
                                        },
                                        vec![rd(fdb2)],
                                        move |pos_b| Action::Pure(Value::List(vec![pos_a, pos_b])),
                                    )
                                },
                            )),
                            next: Box::new(Action::Pure),
                        }
                    },
                )
            },
        ))
        .unwrap();
    let (pos_a, pos_b) = match &v {
        Value::List(l) => (
            match l[0] {
                Value::U64(x) => x,
                ref other => panic!("{other:?}"),
            },
            match l[1] {
                Value::U64(x) => x,
                ref other => panic!("{other:?}"),
            },
        ),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        pos_a, 0,
        "重开后 a 游标为写前位置 0（A6 双态：Seek(Current) 可观察）"
    );
    assert_eq!(pos_b, 0, "重开后 b 游标为写前位置 0（A6 双态）");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3b：put_back 错误循环（TcpShutdown 的 Arc::try_unwrap 分支，与
// R1 的管道 get_mut 分支不同路径）10 次后 fd 仍可寻址、映射不吞句柄。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn r1_putback_tcp_shutdown_10_rounds_fd_still_usable() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
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
    let v = rt
        .run_blocking(syscall(DataOp::TcpConnect { addr }, vec![], Action::Pure))
        .unwrap();
    let cfd = fd_of(&v);
    // Dup 共享 Arc → 后续 IO 均 InvalidInput（共享后无法 &mut / try_unwrap）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Dup { fd: cfd },
            vec![rd(cfd)],
            Action::Pure,
        ))
        .unwrap();
    let cfd2 = fd_of(&v);

    // 连续 10 次 TcpShutdown：共享 → try_unwrap 失败 → put_back 恢复
    // （每次错误后注册表条目与 stream_fds 映射轮换重分配）。
    for i in 0..10 {
        let e = rt
            .run_blocking(syscall(
                DataOp::TcpShutdown {
                    fd: cfd,
                    how: std::net::Shutdown::Both,
                },
                vec![rd(cfd)],
                Action::Pure,
            ))
            .unwrap_err();
        assert_eq!(
            e,
            SysError::InvalidInput,
            "第 {i} 次：Dup 共享下 TcpShutdown 应 InvalidInput（可寻址）"
        );
    }

    // 关闭 dup 释放共享 → 原 fd 恢复真实可 shutdown（10 次 put_back 轮换
    // 后逻辑映射仍指向正确句柄）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: cfd2 },
        vec![ow(cfd2)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::TcpShutdown {
            fd: cfd,
            how: std::net::Shutdown::Both,
        },
        vec![rd(cfd)],
        Action::Pure,
    ))
    .unwrap();

    // 全部 fd 正常 Close（映射未被错误路径吞掉）；随后管道读写正常（状态未毒化）。
    for fd in [cfd, lfd] {
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
            .unwrap();
    }
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                syscall(
                    DataOp::Write {
                        fd: wfd,
                        data: b"ok".to_vec(),
                    },
                    vec![wr(wfd)],
                    move |_| {
                        syscall(
                            DataOp::Read { fd: rfd, len: 2 },
                            vec![rd(rfd)],
                            Action::Pure,
                        )
                    },
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"ok".to_vec()));
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3c：RFC-05 回归 —— Replace 后旧 fd 写失败（修复验证，与 R1
// lin_stale_fd_write_after_replace_fails 一致）；另验证分支级 Replace 只
// 清分支 registry（父级 A4 状态隔离保留）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn r1_stale_fd_write_after_replace_recheck() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("stale-r2.txt");
    let seed: Vec<u8> = b"seed-data".to_vec();
    std::fs::write(&pa, &seed).unwrap();
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
                            data: b"WXYZ".to_vec(),
                        },
                        vec![wr(fd)],
                        move |_| Action::Pure(Value::Fd(fd)),
                    )
                },
            ))
            .unwrap();
        fd_of(&v)
    };

    // 分支级 Replace（并行分支内）：只清分支 registry/undo，父级 undo 与
    // A4 消费状态保留（D13 隔离语义）。
    rt.run_blocking(Action::Fork {
        left: Box::new(Action::Replace {
            target: Box::new(Action::Pure(Value::Unit)),
        }),
        right: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
        combine: Box::new(|_, _| Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        rt.undo_stack().len(),
        1,
        "父级 Write undo 未被分支级 Replace 吞掉"
    );
    // 父级 A4：同资源再 Write 仍被拦截（分支级 Replace 不清父 consumed）。
    let e = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd,
                data: b"XX".to_vec(),
            },
            vec![wr(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::InvalidInput,
        "分支级 Replace 后父级 A4 消费标记仍生效"
    );

    // 父级 Replace（D10：recover + reg.clear）→ 旧 fd 写不再被 A4 拦截。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(std::fs::read(&pa).unwrap(), seed, "父级 Replace 已恢复内容");

    // RFC-05 已修复：父级 Replace 后旧 fd Write 必须失败（NotFound）——registry
    // 是 fd 活性唯一真相，executor 侧 files 缓存不得绕过（与 R1
    // lin_stale_fd_write_after_replace_fails 一致）。
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
        "RFC-05 修复后：父级 Replace 后旧 fd Write 必须失败"
    );
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        seed,
        "旧 fd 写失败 → 恢复后的内容不再被破坏"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4：Fork 顺序错误路径（Runtime 路径 + Catch）—— 左分支 Err →
// 右分支副作用发生（Open 真实执行）→ 错误传播 → Catch 捕获后右分支 merge
// 的句柄在父 registry 仍可见；recover 链完整。顺序与并行两条路径各一。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn err_fork_left_error_catch_merged_handle_visible_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("seq-a.txt");
    let pb = dir.path().join("seq-b.txt");
    std::fs::write(&pa, b"seed-a").unwrap();
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
            Action::Pure,
        ))
        .unwrap();
    let fda = fd_of(&v);

    let got = Arc::new(std::sync::Mutex::new(None::<u64>));
    let got_r = Arc::clone(&got);
    // 左分支：先 Write（副作用 + undo）再确定性 Err（Read 不存在的 fd）。
    // 右分支声明 rd(fda) 与左分支 wu(fda) 冲突 → 顺序路径（left→right）。
    let left = Action::Sequential {
        current: Box::new(syscall(
            DataOp::Write {
                fd: fda,
                data: b"L".to_vec(),
            },
            vec![wu(fda)],
            Action::Pure,
        )),
        next: Box::new(|_| {
            syscall(
                DataOp::Read {
                    fd: 999_999,
                    len: 1,
                },
                vec![rd(999_999)],
                Action::Pure,
            )
        }),
    };
    let pb_in = pb.clone();
    let right = Action::Sequential {
        // 右分支 current 用无副作用 Seek（Current(0)，游标不动）：不能 Read——
        // 会消费左分支刚写入的字节（文件内容被破坏，读回断言失败）。
        current: Box::new(syscall(
            DataOp::Seek {
                fd: fda,
                offset: 0,
                whence: std::io::SeekFrom::Current(0),
            },
            vec![rd(fda)],
            Action::Pure,
        )),
        next: Box::new(move |_| {
            syscall(
                DataOp::Open {
                    path: pb_in.clone(),
                    // 必须 create 能力：pb 不存在，read-only Open 会 NotFound，
                    // 右分支副作用不发生（构造错误，A2 批 6 语义要求右分支 Open
                    // 成功 → 文件创建 + 句柄 merge）。
                    flags: rw_flags(),
                },
                vec![wr_path(pb_in.clone())],
                move |v| {
                    *got_r.lock().unwrap() = Some(fd_of(&v));
                    Action::Pure(Value::Unit)
                },
            )
        }),
    };
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Fork {
                left: Box::new(left),
                right: Box::new(right),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "左分支 Read(999999) 错误传播");
                Action::Pure(Value::Unit)
            }),
        })
        .unwrap();
    assert_eq!(v, Value::Unit, "Catch 捕获 Fork 错误");

    // 右分支副作用发生：file_b 被真实 Open（执行器 files 映射 + 注册表）。
    assert!(pb.exists(), "右分支 Open 副作用发生");
    let right_fd = got.lock().unwrap().expect("右分支 fd 经 next 闭包记录");
    assert!(
        rt.registry().lookup(right_fd).is_some(),
        "Catch 后右分支 merge 的句柄在父 registry 可见"
    );
    // 左分支 Write 在 pos 0 覆盖原首字节："seed-a" → "Leed-a"（写是覆盖不是插入）。
    assert_eq!(std::fs::read(&pa).unwrap(), b"Leed-a", "左分支 Write 生效");
    assert_eq!(rt.undo_stack().len(), 1, "左分支 Write undo 合并回父");

    // recover（Replace）→ 左分支写撤销、文件恢复；registry 清空。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::read(&pa).unwrap(),
        b"seed-a",
        "错误路径后撤销链完整"
    );

    // 状态未毒化：同路径重开可写。
    let v = rt
        .run_blocking(syscall(
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
                        data: b"ok".to_vec(),
                    },
                    vec![wr(fd)],
                    Action::Pure,
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Unit);
    // "seed-a"(6 字节) 写 "ok" 覆盖前 2 字节 → "oked-a"（覆盖语义）。
    assert_eq!(std::fs::read(&pa).unwrap(), b"oked-a", "重写仍是覆盖语义");
}

/// 并行路径版本：左分支确定性 Err（Read 不存在 fd）、右分支 Open 成功 →
/// 两分支并发执行 → 错误传播 → Catch → 右分支句柄仍可见。
#[test]
fn err_fork_left_error_catch_merged_handle_visible_parallel() {
    let dir = tempfile::tempdir().unwrap();
    let pb = dir.path().join("par-b.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let got = Arc::new(std::sync::Mutex::new(None::<u64>));
    let got_r = Arc::clone(&got);
    let v = rt
        .run_blocking(Action::Catch {
            action: Box::new(Action::Fork {
                left: Box::new(syscall(
                    DataOp::Read {
                        fd: 999_999,
                        len: 1,
                    },
                    vec![rd(999_999)],
                    Action::Pure,
                )),
                right: Box::new(syscall(
                    DataOp::Open {
                        path: pb.clone(),
                        // 同顺序路径：pb 不存在须 create（read-only Open → NotFound）。
                        flags: rw_flags(),
                    },
                    vec![wr_path(pb.clone())],
                    move |v| {
                        *got_r.lock().unwrap() = Some(fd_of(&v));
                        Action::Pure(Value::Fd(fd_of(&v)))
                    },
                )),
                combine: Box::new(|_, _| Action::Pure(Value::Unit)),
            }),
            handler: Box::new(|e| {
                assert_eq!(e, SysError::NotFound, "并行分支左错误传播");
                Action::Pure(Value::Unit)
            }),
        })
        .unwrap();
    assert_eq!(v, Value::Unit);
    assert!(pb.exists(), "右分支 Open 副作用发生（并行路径）");
    let right_fd = got.lock().unwrap().expect("右分支 fd 记录");
    assert!(
        rt.registry().lookup(right_fd).is_some(),
        "并行错误路径下右分支句柄仍合并可见"
    );
    assert!(rt.undo_stack().is_empty(), "Read/Open 均无 undo");
    // 状态未毒化：后续蓝图完全正常。
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                syscall(
                    DataOp::Write {
                        fd: wfd,
                        data: b"alive".to_vec(),
                    },
                    vec![wr(wfd)],
                    move |_| {
                        syscall(
                            DataOp::Read { fd: rfd, len: 5 },
                            vec![rd(rfd)],
                            Action::Pure,
                        )
                    },
                )
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"alive".to_vec()));
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 5：时间面 —— Timeout 内 Fork（完成）、Timeout 内嵌套 Timeout、
// 【已知缺陷】Timeout 内并行 Fork 的孤儿分支副作用、Sleep 0、GetTime。
// ══════════════════════════════════════════════════════════════════════

/// Timeout 内并行 Fork 快速完成（结果 = Fork 值而非 on_timeout）；
/// Fork 分支内再嵌 Timeout（Sleep 10s 被 50ms 超时打断 → on_timeout 42）。
#[test]
fn time_timeout_inner_parallel_fork_completes() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("t-a.txt");
    let pb = dir.path().join("t-b.txt");
    let pc = dir.path().join("t-c.txt");
    std::fs::write(&pa, b"AAA").unwrap();
    std::fs::write(&pb, b"BBB").unwrap();
    std::fs::write(&pc, b"CCC").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // Timeout(2s) 内 Fork(open a ∥ open b)：Fork 毫秒级完成 → 返回 List(fda, fdb)。
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Fork {
                left: Box::new(open_pure(pa.clone())),
                right: Box::new(open_pure(pb.clone())),
                combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
            }),
            duration: Duration::from_secs(2),
            on_timeout: Box::new(Action::Pure(Value::U64(42))),
        })
        .unwrap();
    let (fda, fdb) = pair_of(&v);
    assert_ne!(fda, fdb, "Timeout 内并行 Fork 两 fd 不相撞");
    assert_eq!(read_back(&mut rt, fda, 3), b"AAA");
    assert_eq!(read_back(&mut rt, fdb, 3), b"BBB");

    // Fork 分支内嵌 Timeout：左分支 Sleep(10s) 被 50ms 超时打断 → 42；
    // 右分支正常 Open → 合并回父。
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(Action::Timeout {
                action: Box::new(Action::Sleep {
                    duration: Duration::from_secs(10),
                    next: Box::new(Action::Pure),
                }),
                duration: Duration::from_millis(50),
                on_timeout: Box::new(Action::Pure(Value::U64(42))),
            }),
            right: Box::new(open_pure(pc.clone())),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    match &v {
        Value::List(l) => {
            assert_eq!(l[0], Value::U64(42), "分支内 Timeout 触发 on_timeout");
            let fdc = fd_of(&l[1]);
            assert_eq!(read_back(&mut rt, fdc, 3), b"CCC");
        }
        other => panic!("{other:?}"),
    }
}

/// 【RFC-08 修复后断言翻转（迭代 1 取消传播协议）】Timeout 内并行 Fork
/// 超时后：取消广播（watch 令牌）→ 孤儿分支在下一 op 边界中止 → Open
/// 副作用不再发生（修复前：spawn_blocking 分支成为孤儿继续执行、副作用
/// 不可撤销、句柄泄漏）。
///
/// 语义分叉（审计 R2 适配）：virtual-clock 下 Sleep 瞬时完成——左分支在
/// 超时前即执行 Open（无「飞行中窗口」，post-check 语义、无取消），文件
/// **被创建**是正确行为（见 `time_timeout_parallel_fork_virtual_clock_postcheck`）；
/// 本测试（墙钟语义）依赖 Sleep 提供真实超时窗口。
#[cfg(not(feature = "virtual-clock"))]
#[test]
fn time_timeout_parallel_fork_orphan_effects_unrecoverable() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("orphan-a.txt");
    let pb = dir.path().join("orphan-b.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 左分支 Sleep(400ms) 后 Open —— 超时（100ms）后分支应响应取消中止。
    let pa_in = pa.clone();
    let v = rt
        .run_blocking(Action::Timeout {
            action: Box::new(Action::Fork {
                left: Box::new(Action::Sequential {
                    current: Box::new(Action::Sleep {
                        duration: Duration::from_millis(400),
                        next: Box::new(Action::Pure),
                    }),
                    // 孤儿 Open 必须 create 能力：pa 不存在，read-only Open 会
                    // NotFound → 文件不会创建（原构造错误，副作用断言必然失败）。
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
            duration: Duration::from_millis(100),
            on_timeout: Box::new(Action::Pure(Value::U64(42))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(42), "Timeout 触发");

    // 等待原孤儿窗口：取消传播后分支在 Sleep 后下一 op 边界中止，
    // Open 不执行 → 文件不被创建（修复前被创建）。
    std::thread::sleep(Duration::from_millis(900));
    assert!(
        !pa.exists(),
        "RFC-08 修复后：孤儿分支响应取消，Open 副作用不发生（文件未被创建）"
    );
    assert!(rt.undo_stack().is_empty(), "取消路径不产生 undo");
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();

    // 父级继续执行不受影响（孤儿 fd 未并入父 registry，父 next_fd 未抬高）。
    std::fs::write(&pb, b"BBB").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: pb.clone(),
                flags: read_only_flags(),
            },
            vec![rd_path(pb.clone())],
            move |v| {
                let fd = fd_of(&v);
                syscall(DataOp::Read { fd, len: 3 }, vec![rd(fd)], Action::Pure)
            },
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"BBB".to_vec()), "父级执行正常");
}

/// virtual-clock 语义（审计 R2 适配）：Sleep 瞬时完成 → 分支无「飞行中窗口」。
/// 结果时序依赖：分支在超时前完成 Open → 文件创建（post-check 效果保留）；
/// 分支晚启动（spawn 排队 > 超时）→ 循环顶取消检查拦截 → 文件不创建
/// （取消传播在 VC 下同样生效）。两种结果均正确，故只断言确定性部分：
/// 虚拟通道判定超时（on_timeout 触发）。
#[cfg(feature = "virtual-clock")]
#[test]
fn time_timeout_parallel_fork_virtual_clock_postcheck() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("orphan-vc.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let pa_in = pa.clone();
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
            duration: Duration::from_millis(100),
            on_timeout: Box::new(Action::Pure(Value::U64(42))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(42), "虚拟通道判定超时（post-check）");
    // 注：pa.exists() 为时序依赖（见函数注释），不断言。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
}

/// Sleep(0)：立即完成（不挂起、next 正常执行）。
#[test]
fn time_sleep_zero_immediate() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let t0 = Instant::now();
    let v = rt
        .run_blocking(Action::Sleep {
            duration: Duration::ZERO,
            next: Box::new(|_| Action::Pure(Value::U64(1))),
        })
        .unwrap();
    assert_eq!(v, Value::U64(1));
    // 收紧断言（独立审查 MEDIUM-2）：修复前 Windows tick 15.6ms 下 Sleep(0)
    // 吃满一个 tick（实测 ~15.6ms）——<100ms 可分离 tick 与 µs 级实现且抗 CI
    // 抖动（修复后实测 ~µs）；原 <1s 断言修复前后都过、无回归捕获力。
    assert!(
        t0.elapsed() < Duration::from_millis(100),
        "Sleep(0) 应立即完成（修复前 ~15.6ms/tick；修复后 µs 级），实测 {:?}",
        t0.elapsed()
    );
}

/// GetTime 多上下文类型稳定：普通 / 并行 Fork 分支内 / Replace 后。
#[test]
fn time_gettime_types_stable_contexts() {
    let dir = tempfile::tempdir().unwrap();
    let pa = dir.path().join("gt.txt");
    std::fs::write(&pa, b"g").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "普通 GetTime → U64");

    // 并行 Fork 分支内 GetTime（与 Open 不相交 → 真并行）。
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(syscall(DataOp::GetTime, vec![], Action::Pure)),
            right: Box::new(open_pure(pa.clone())),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    match &v {
        Value::List(l) => {
            assert!(matches!(l[0], Value::U64(_)), "Fork 分支内 GetTime → U64");
            assert!(rt.registry().lookup(fd_of(&l[1])).is_some());
        }
        other => panic!("{other:?}"),
    }

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    let v = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v, Value::U64(_)), "Replace 后 GetTime → U64");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 6：资源计数 —— Mmap len 截断边界（len > 文件长、len = 0、
// 大文件、NotFound 状态未毒化）。经真实 Runtime 全链路。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn mem_mmap_bounds_through_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let big = dir.path().join("big.bin");
    // 2MB 大文件（> FULL_UNDO_MAX_BYTES 的规模，验证大文件路径）。
    let payload: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&big, &payload).unwrap();

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // len < 文件长：按 len 截断（1MB）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Mmap {
                path: big.clone(),
                len: 1024 * 1024,
                prot: MmapProt::default(),
            },
            vec![rd_path(big.clone())],
            Action::Pure,
        ))
        .unwrap();
    match &v {
        Value::Bytes(b) => {
            assert_eq!(b.len(), 1024 * 1024, "Mmap len=1MB 截断");
            assert_eq!(&b[..8], &payload[..8], "截断保留前缀");
        }
        other => panic!("{other:?}"),
    }

    // len = 0：当前实现返回空（POSIX mmap(len=0) 应 EINVAL —— 行为偏差，
    // medium-7 实现未校验；记录当前行为）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Mmap {
                path: big.clone(),
                len: 0,
                prot: MmapProt::default(),
            },
            vec![rd_path(big.clone())],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(vec![]), "len=0 → 空（POSIX 偏差记录）");

    // len > 文件长：返回全部内容（不补零）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Mmap {
                path: big.clone(),
                len: 3 * 1024 * 1024,
                prot: MmapProt::default(),
            },
            vec![rd_path(big.clone())],
            Action::Pure,
        ))
        .unwrap();
    match &v {
        Value::Bytes(b) => {
            assert_eq!(b.len(), payload.len(), "len > 文件长 → 全部内容");
            assert_eq!(b, &payload);
        }
        other => panic!("{other:?}"),
    }

    // Munmap no-op。
    rt.run_blocking(syscall(
        DataOp::Munmap { addr: 0, len: 0 },
        vec![],
        Action::Pure,
    ))
    .unwrap();

    // NotFound：错误不毒化状态（undo 空、后续蓝图正常）。
    let missing = dir.path().join("missing.bin");
    let e = rt
        .run_blocking(syscall(
            DataOp::Mmap {
                path: missing,
                len: 16,
                prot: MmapProt::default(),
            },
            vec![rd_path(dir.path().join("missing.bin"))],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e, SysError::NotFound);
    assert!(rt.undo_stack().is_empty(), "失败的 Mmap 不产生 undo");
    let v = rt
        .run_blocking(syscall(DataOp::GetTime, vec![], Action::Pure))
        .unwrap();
    assert!(matches!(v, Value::U64(_)));
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 7：RFC-07 修复 —— 管道半端文件式双表（Arc<Mutex>），D13 Fork 分支
// 隔离/Dup 共享下分支内管道 IO 成功。
// 修复前：registry 经 D13 Clone 做分支隔离时 PipeReader/PipeWriter 的 Arc 被
// 共享（strong_count > 1），executor 管道路径依赖 Arc::get_mut（take/put_back
// 轮换）→ 共享下必然失败 → InvalidInput。修复后：registry 句柄与 executor
// 工作表存同一 Arc<tokio::sync::Mutex<半端>>，lock 下 IO 合法；管道不轮换，
// 避免共享执行器 + 分支隔离 registry 下轮换映射互相污染（右分支按原 fd
// 取句柄）。
// ══════════════════════════════════════════════════════════════════════

/// RFC-07 冲突（顺序）路径：父级 PipeOpen 后，Fork 左/右分支对同一写端 wfd
/// 双写（Write∥Write 静态冲突 → 顺序路径）。修复前两分支对父管道半端 IO 时
/// Arc::get_mut 因 D13 registry Clone 共享而失败 → InvalidInput；修复后两分支
/// 写均成功，combine 读回 "LR"（顺序路径观察序）。
#[test]
fn rfc07_pipe_io_inside_conflict_fork_sequential() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                // 同 wfd 双写 → Write∥Write 冲突 → 顺序路径（left 写 L、right 写 R）。
                Action::Fork {
                    left: Box::new(syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"L".to_vec(),
                        },
                        vec![wr(wfd)],
                        Action::Pure,
                    )),
                    right: Box::new(syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"R".to_vec(),
                        },
                        vec![wr(wfd)],
                        Action::Pure,
                    )),
                    combine: Box::new(move |_, _| {
                        syscall(
                            DataOp::Read { fd: rfd, len: 2 },
                            vec![rd(rfd)],
                            Action::Pure,
                        )
                    }),
                }
            },
        ))
        .unwrap();
    assert_eq!(
        v,
        Value::Bytes(b"LR".to_vec()),
        "顺序路径两分支写均成功且数据有序（RFC-07 修复）"
    );
}

/// RFC-07 并行路径：父级 PipeOpen 后，Fork 左分支读读端、右分支写写端（不同 fd
/// → 无冲突 → 真并行）。修复前共享 Arc 下分支内管道 IO 必然 InvalidInput；修复
/// 后并行两分支各自 lock（读端与写端各自独立 Arc<Mutex>）成功，数据跨分支流动。
#[test]
fn rfc07_pipe_io_inside_parallel_fork() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                Action::Fork {
                    left: Box::new(syscall(
                        DataOp::Read { fd: rfd, len: 4 },
                        vec![rd(rfd)],
                        Action::Pure,
                    )),
                    right: Box::new(syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"ping".to_vec(),
                        },
                        vec![wr(wfd)],
                        Action::Pure,
                    )),
                    combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
                }
            },
        ))
        .unwrap();
    let (lv, rv) = match v {
        Value::List(l) if l.len() == 2 => (l[0].clone(), l[1].clone()),
        other => panic!("期望 List([Bytes, Unit])，得到 {other:?}"),
    };
    assert_eq!(
        lv,
        Value::Bytes(b"ping".to_vec()),
        "并行左分支读端 lock 读成功（数据跨分支到达，RFC-07 修复）"
    );
    assert_eq!(rv, Value::Unit, "并行右分支写端 lock 写成功");
}

/// RFC-07 分支本地管道：两并行分支各自 PipeOpen + 写入（fd 区间预分割不相撞），
/// merge 后父级经共享执行器管道表按逻辑 fd 读回分支数据；Close 分支管道不回归。
#[test]
fn rfc07_branch_local_pipe_open_io_merge_readback() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(Action::Fork {
            left: Box::new(syscall(
                DataOp::PipeOpen {
                    flags: PipeFlags::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    // 分支本地管道：Open → Write → 返回 (rfd, wfd) 供 combine。
                    syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"LL".to_vec(),
                        },
                        vec![wr(wfd)],
                        move |_| Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)])),
                    )
                },
            )),
            right: Box::new(syscall(
                DataOp::PipeOpen {
                    flags: PipeFlags::default(),
                },
                vec![],
                move |v| {
                    let (rfd, wfd) = pair_of(&v);
                    syscall(
                        DataOp::Write {
                            fd: wfd,
                            data: b"RR".to_vec(),
                        },
                        vec![wr(wfd)],
                        move |_| Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(wfd)])),
                    )
                },
            )),
            combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
        })
        .unwrap();
    // v = List([List([Fd(lr), Fd(lw)]), List([Fd(rr), Fd(rw)])]).
    let mut pairs: Vec<(u64, u64)> = Vec::new();
    match v {
        Value::List(outer) if outer.len() == 2 => {
            for inner in outer {
                match inner {
                    Value::List(p) if p.len() == 2 => {
                        pairs.push((fd_of(&p[0]), fd_of(&p[1])));
                    }
                    other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
                }
            }
        }
        other => panic!("期望 List([List, List])，得到 {other:?}"),
    }
    let (lr, lw) = pairs[0];
    let (rr, rw) = pairs[1];
    assert_ne!(
        (lr, lw),
        (rr, rw),
        "两分支管道 fd 不相撞（右分支 k<<48 区间预分割）"
    );
    // 分支内写入的数据经 merge 后由父读回：共享执行器管道表按逻辑 fd 寻址，
    // 父级 Read 直接命中（RFC-07 修复）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: lr, len: 2 },
            vec![rd(lr)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"LL".to_vec()), "左分支管道数据读回");
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rr, len: 2 },
            vec![rd(rr)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(b"RR".to_vec()), "右分支管道数据读回");
    // Close 不回归：父级可关分支管道（写端关闭 → 读端后续 EOF）。
    for fd in [lw, rw] {
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
            .unwrap();
    }
    // 写端全关后读端返回 EOF（空 Bytes），语义不回归。
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: lr, len: 2 },
            vec![rd(lr)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(v, Value::Bytes(Vec::new()), "写端关闭后读端 EOF");
}

/// RFC-07 Dup 独占场景（Fork 内）：父级 PipeOpen 后，分支内 Dup 写端（registry
/// Clone + Dup 双重共享 Arc<Mutex<WriteHalf>>），经 dup fd 写入 → 父级经原读端
/// 读回。修复前 get_mut 独占下任何共享（Dup 或 Fork Clone）都 InvalidInput；
/// 修复后 lock 下 IO 合法，dup 与原 fd 共享同一工作对象（文件式双表）。
#[test]
fn rfc07_pipe_dup_inside_fork_shared_write_succeeds() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let v = rt
        .run_blocking(syscall(
            DataOp::PipeOpen {
                flags: PipeFlags::default(),
            },
            vec![],
            move |v| {
                let (rfd, wfd) = pair_of(&v);
                Action::Fork {
                    left: Box::new(syscall(DataOp::Dup { fd: wfd }, vec![], move |v| {
                        let dup = fd_of(&v);
                        syscall(
                            DataOp::Write {
                                fd: dup,
                                data: b"dup".to_vec(),
                            },
                            vec![wr(dup)],
                            move |_| {
                                Action::Pure(Value::List(vec![Value::Fd(rfd), Value::Fd(dup)]))
                            },
                        )
                    })),
                    right: Box::new(Action::Pure(Value::Unit)),
                    combine: Box::new(|l, _| Action::Pure(l)),
                }
            },
        ))
        .unwrap();
    let (rfd, dup) = match v {
        Value::List(l) if l.len() == 2 => (fd_of(&l[0]), fd_of(&l[1])),
        other => panic!("期望 List([Fd, Fd])，得到 {other:?}"),
    };
    // 父级经原读端读回分支经 dup 写入的数据（Fork Clone + Dup 双重共享下 IO 成功）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Read { fd: rfd, len: 3 },
            vec![rd(rfd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        v,
        Value::Bytes(b"dup".to_vec()),
        "分支内 dup 写端 lock 写成功，数据可达父读端（RFC-07 修复）"
    );
    // Close 不回归：父级可关 dup 与读端（双表条目与注册表一致）。
    rt.run_blocking(syscall(
        DataOp::Close { fd: dup },
        vec![ow(dup)],
        Action::Pure,
    ))
    .unwrap();
    rt.run_blocking(syscall(
        DataOp::Close { fd: rfd },
        vec![ow(rfd)],
        Action::Pure,
    ))
    .unwrap();
}
