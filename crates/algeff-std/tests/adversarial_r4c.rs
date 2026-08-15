//! R4c 对抗审计（分块 C，第 2 部分）：规模/栈深边界 —— 大文件撤销、
//! 多分支风暴、值类型矩阵（真实 IO + 真并行）。
//!
//! 攻击方法论：与 R1-R3 相同——不 mock、全部经真实 `Runtime` +
//! `TokioExecutor` 全链路（`run_blocking` → `interpret` → 共享执行器通道）。
//! R1-R3 已覆盖（不重复）：R1 单次 Alloc 1MB+Replace、5 层值流；R2 冲突型
//! 1000 Fork（右分支不分配）、RFC-06 二次增长 30 轮记录、深度 5/6 树；
//! R3b 8 叶深度 6；R3c 网络面。**本文件攻击 R1-R3 未触及的压力面**：
//!
//! 2. **大文件撤销（pdr.md §11.2 Full/BestEffort 边界）**：
//!    - 恰 <1MB（1048575B）：Write 产生 Full undo（写前读）→ Replace 完整
//!      恢复内容+长度+游标（A6 双态成立）；
//!    - **恰 1MB（1048576B）**：`orig_len < FULL_UNDO_MAX_BYTES` 不成立 →
//!      BestEffort，Write 不产生 undo → Replace 后写入**保留**（记录偏差：
//!      BestEffort 不满足 A6，pdr.md §11.2 已声明）；
//!    - **2MB**：同 BestEffort，写入保留（偏差记录）。真实 IO（tempfile +
//!      std::fs），非 mock。
//! 3. **多分支风暴**：
//!    - **Fork 16 路并行**（平衡树 15 个 Fork 节点、15 个右分支区间序号；
//!      16 叶资源两两不相交 → 全并行）：叶内 Open+Read 经共享执行器锁并发
//!      执行，16 个 fd 两两不相交、分支内读回值经 15 个 combine 保真合并；
//!    - **连续 100 个顺序并行 Fork**（每轮左右分支各开 1 文件、资源不相交
//!      → 并行路径，每轮消耗 1 个全局右分支区间序号）：200 个 fd 全不碰撞、
//!      区间序号严格单调（RFC-06 边界：100 轮 ≪ ~362 轮 u64 溢出阈值，仅
//!      记录二次增长量级，不触发溢出）。
//! 4. **值类型矩阵**：Value 全部变体（Unit/Bool/U64/I64/Bytes/Str/Fd/Pid/
//!    Addr/List）经 Sequential next → 并行 Fork combine（值跨 spawn_blocking
//!    线程边界）→ 最终保真。
//!
//! 驱动方式：全部普通 `#[test]`（非 `#[tokio::test]`）——D9 要求
//! `Runtime::new` 与 `run_blocking` 在 tokio 上下文之外调用。

use std::path::PathBuf;
use std::sync::Arc;

use algeff_core::{
    Action, DataOp, OpenFlags, ReadOnly, ResourceHandle, ResourceInner, ResourceUsage, Runtime,
    TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1-R3 相同约定）──────────────

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
fn assert_disjoint_lookupable(fds: &[u64], reg: &mut algeff_core::ResourceRegistry) {
    let mut uniq = std::collections::HashSet::new();
    for fd in fds {
        assert!(uniq.insert(*fd), "fd 重复: {fd}，全部: {fds:?}");
        assert!(reg.lookup(*fd).is_some(), "fd {fd} 句柄不可 lookup");
    }
    assert_eq!(uniq.len(), fds.len());
}

/// 确定性内容模式（非全零，可区分字节位置）。
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2a：<1MB（1048575B）Write → Full 撤销（写前读）→ Replace 完整恢复。
// pdr.md §11.2：Full 策略（小文件 <1MB）满足公理 A6。断言：undo 入栈 →
// 写入立即可观察（flush 契约）→ Replace 后内容+长度+游标全部复原。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn undo_full_strategy_sub_1mb_write_fully_restored_by_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r4c-full-under-1mb.bin");
    let orig = pattern(1024 * 1024 - 1); // 1048575B（< FULL_UNDO_MAX_BYTES）
    std::fs::write(&path, &orig).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"NEW!".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();

    // Full 策略：Write 必须产生 undo（写前读区域 → 可完整回滚）。
    assert_eq!(rt.undo_stack().len(), 1, "<1MB 文件 Write 应入 Full undo");
    // 写入立即可观察（flush 契约，R1 flaky 根因回归面）。
    assert_eq!(
        &std::fs::read(&path).unwrap()[0..4],
        b"NEW!",
        "Write op 完成后新内容必须立即可观察"
    );

    // Replace → recover（LIFO 执行 undo）→ 内容+长度复原。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty(), "recover 后撤销栈空");
    let after = std::fs::read(&path).unwrap();
    assert_eq!(after.len(), orig.len(), "Full 撤销恢复文件长度");
    assert_eq!(after, orig, "Full 撤销恢复写前内容（A6 双态 w;w̄=1）");

    // 游标复原：undo 将游标 seek 回写前位置（pos=0），故不带 Seek 的 Read
    // 应读到原内容头 4 字节（RFC-05：reg.clear 后 executor 文件映射仍可寻址）。
    let head = rt
        .run_blocking(syscall(
            DataOp::Read { fd, len: 4 },
            vec![rd(fd)],
            Action::Pure,
        ))
        .unwrap();
    assert_eq!(
        head,
        Value::Bytes(orig[0..4].to_vec()),
        "Full 撤销后游标恢复写前位置（从 pos=0 读到原内容）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2b：恰 1MB（1048576B）Write → BestEffort（边界：`orig_len <
// FULL_UNDO_MAX_BYTES` 为假）→ 无 undo → Replace 后写入**保留**。
// pdr.md §11.2 声明 BestEffort 不满足 A6 —— 本测试以断言固定该边界行为
// （修复后若 Full 策略扩展至此，本测试会失败提醒更新，R1 RFC-05 先例）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn undo_besteffort_exactly_1mb_write_persists_after_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r4c-besteffort-1mb.bin");
    let orig = pattern(1024 * 1024); // 1048576B（恰 FULL_UNDO_MAX_BYTES）
    std::fs::write(&path, &orig).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"NEW!".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();

    // BestEffort：恰 1MB 时 `orig_len < 1024*1024` 不成立 → undo=None。
    assert_eq!(
        rt.undo_stack().len(),
        0,
        "恰 1MB 文件 Write 应降级 BestEffort（不产生 undo）"
    );

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        &after[0..4],
        b"NEW!",
        "BestEffort 不恢复：恰 1MB 文件 Write 效果在 Replace 后保留"
    );
    assert_eq!(after.len(), orig.len(), "文件长度不变");
    assert_eq!(&after[4..], &orig[4..], "仅写区域偏移，其余原样");
    eprintln!(
        "R4C 记录偏差（pdr §11.2 BestEffort）：恰 1MB 文件 Write 后 Replace 不恢复，\
         违反 A6（w;w̄≠1）；Full 阈值 `orig_len < 1024*1024` 为硬边界"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 2c：2MB 文件 Write → BestEffort（略超阈值）→ Replace 后保留。
// 与 2b 同族但放大规模（真实 2MB IO）；记录偏差同 pdr.md §11.2。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn undo_besteffort_2mb_write_persists_after_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r4c-besteffort-2mb.bin");
    let orig = pattern(2 * 1024 * 1024); // 2097152B（≫ 阈值）
    std::fs::write(&path, &orig).unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: path.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(path.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"NEW!".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(rt.undo_stack().len(), 0, "2MB 文件 Write 应降级 BestEffort");

    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        &after[0..4],
        b"NEW!",
        "BestEffort 不恢复：2MB 文件 Write 效果在 Replace 后保留"
    );
    assert_eq!(after.len(), orig.len());
    eprintln!(
        "R4C 记录偏差（pdr §11.2 BestEffort）：2MB 文件 Write 后 Replace 不恢复，\
         违反 A6；仅全量写前读（Full）可恢复，大文件撤销为补偿挂钩职责"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3a：Fork 16 路并行（平衡树 15 个 Fork 节点 → 15 个右分支区间序号）。
// 16 叶资源两两不相交（不同文件路径）→ 静态无冲突 → 全并行（每层真并行
// spawn_blocking）。叶内 Open→Seek→Read 经共享执行器锁并发执行（48 个
// syscall），值经 15 个 combine 逐层 List 合并回根：16 个 fd 两两不相交 +
// 分支内读回值按 DFS 序与文件内容一一对应（值流保真）+ 事后读回一致。
// ══════════════════════════════════════════════════════════════════════

/// 叶：Open(rd) → Seek(0) → Read(len) → List([Fd, Bytes])。
fn leaf_open_read(path: PathBuf, len: usize) -> Action {
    syscall(
        DataOp::Open {
            path: path.clone(),
            flags: read_only_flags(),
        },
        vec![rd_path(path)],
        move |v| {
            let fd = fd_of(&v);
            syscall(
                DataOp::Seek {
                    fd,
                    offset: 0,
                    whence: std::io::SeekFrom::Start(0),
                },
                vec![rd(fd)],
                move |_| {
                    syscall(DataOp::Read { fd, len }, vec![rd(fd)], move |v| {
                        Action::Pure(Value::List(vec![Value::Fd(fd), v]))
                    })
                },
            )
        },
    )
}

/// 平衡二叉树（叶子区间 [lo,hi)），combine 逐层 List 拼接。
fn build_wide(lo: usize, hi: usize, files: &[PathBuf], contents: &[Vec<u8>]) -> Action {
    if hi - lo == 1 {
        return leaf_open_read(files[lo].clone(), contents[lo].len());
    }
    let mid = (lo + hi) / 2;
    Action::Fork {
        left: Box::new(build_wide(lo, mid, files, contents)),
        right: Box::new(build_wide(mid, hi, files, contents)),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    }
}

/// DFS 展平 combine 树：叶为 List([Fd, Bytes])，内点为 List([l, r])。
fn flatten_pairs(v: &Value, out: &mut Vec<(u64, Vec<u8>)>) {
    match v {
        Value::List(l) if l.len() == 2 => match (&l[0], &l[1]) {
            (Value::Fd(fd), Value::Bytes(b)) => out.push((*fd, b.clone())),
            _ => {
                for x in l {
                    flatten_pairs(x, out);
                }
            }
        },
        Value::List(l) => {
            for x in l {
                flatten_pairs(x, out);
            }
        }
        other => panic!("期望 List，得到 {other:?}"),
    }
}

#[test]
fn fork_16_way_parallel_all_leaves_disjoint_merge_value_fidelity() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    let mut contents = Vec::new();
    for i in 0..16u8 {
        let p = dir.path().join(format!("r4c-f16-{i:02}.txt"));
        let c = format!("fork16-leaf-{i:02}").into_bytes();
        std::fs::write(&p, &c).unwrap();
        files.push(p);
        contents.push(c);
    }
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let action = build_wide(0, 16, &files, &contents);
    let v = rt.run_blocking(action).unwrap();

    let mut pairs = Vec::new();
    flatten_pairs(&v, &mut pairs);
    assert_eq!(
        pairs.len(),
        16,
        "16 叶 (fd, bytes) 全部经 15 个 combine 到达根"
    );

    let fds: Vec<u64> = pairs.iter().map(|(fd, _)| *fd).collect();
    assert_disjoint_lookupable(&fds, rt.registry());

    // 分支内读回值（DFS 序）与文件内容一一对应（combine 值流保真）。
    for (i, ((fd, bytes), content)) in pairs.iter().zip(contents.iter()).enumerate() {
        assert_eq!(bytes, content, "第 {i} 叶分支内 Read 值经 combine 保真");
        let again = read_back(&mut rt, *fd, bytes.len());
        assert_eq!(
            &again, bytes,
            "第 {i} 叶 fd {fd} 事后读回一致（映射未被并发覆盖）"
        );
    }
    assert!(rt.undo_stack().is_empty(), "只读路径不产生 undo");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 3b：连续 100 个顺序并行 Fork（每轮左右分支各开 1 文件，资源不相交
// → 并行路径 → 每轮右分支消耗 1 个全局区间序号）。断言：200 个 fd 全不碰撞
// （区间互斥）、右分支区间序号（rfd>>48）严格单调、fd 严格递增、末轮读回
// 正确。RFC-06（已修复）：右分支分配型连续 Fork 经 merge 锚点吸收后父 next_fd
// 线性增长（每轮 +2^48，100 轮 ≈ 2^56 ≪ 2^64）；修复前 Σk·2^48 二次增长，
// ~362 轮 u64 溢出（R2 记录，已修；确定性回归门见 resource.rs 单测）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn fork_100_sequential_rounds_region_monotonic_no_collision() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let mut seen = std::collections::HashSet::new();
    let mut prev_k: Option<u64> = None;
    let mut prev_max: Option<u64> = None;
    let mut last_round = (0u64, 0u64);

    for i in 0..100u64 {
        let pl = dir.path().join(format!("r4c-seq-l-{i:03}.txt"));
        let pr = dir.path().join(format!("r4c-seq-r-{i:03}.txt"));
        let cl = format!("L{i:03}");
        let cr = format!("R{i:03}");
        std::fs::write(&pl, &cl).unwrap();
        std::fs::write(&pr, &cr).unwrap();

        // 左右分支开不同文件 → 资源不相交 → 静态无冲突 → 并行路径。
        let v = rt
            .run_blocking(Action::Fork {
                left: Box::new(open_pure(pl)),
                right: Box::new(open_pure(pr)),
                combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
            })
            .unwrap();
        let (lfd, rfd) = pair_of(&v);

        assert!(seen.insert(lfd), "第 {i} 轮左 fd 碰撞: {lfd}");
        assert!(seen.insert(rfd), "第 {i} 轮右 fd 碰撞: {rfd}");
        // 右分支 fd 落在锚定根基线的全局唯一区间起点（k<<48 对齐，RFC-06
        // 修复后右分支偏移锚定根基线，而非被抬高的父 next_fd）。
        assert_eq!(
            rfd & ((1u64 << 48) - 1),
            0,
            "第 {i} 轮右分支 fd 应区间对齐（RFC-06 锚定根基线）"
        );
        let k = rfd >> 48;
        if let Some(pk) = prev_k {
            assert!(
                k > pk,
                "区间序号必须严格单调（第 {i} 轮 k={k} ≤ 上轮 {pk}）"
            );
        }
        prev_k = Some(k);
        if let Some(pm) = prev_max {
            assert!(
                lfd > pm,
                "fd 必须严格递增（第 {i} 轮 lfd={lfd} ≤ 上轮 {pm}）"
            );
        }
        prev_max = Some(rfd);
        last_round = (lfd, rfd);
    }

    assert_eq!(seen.len(), 200, "100 轮 × 2 fd 全部唯一（区间互斥无碰撞）");

    // 末轮 fd 仍可 lookup + 内容读回正确（100 轮 merge 后映射未被污染）。
    let (lfd, rfd) = last_round;
    assert!(rt.registry().lookup(lfd).is_some() && rt.registry().lookup(rfd).is_some());
    assert_eq!(read_back(&mut rt, lfd, 4), b"L099", "末轮左 fd 读回");
    assert_eq!(read_back(&mut rt, rfd, 4), b"R099", "末轮右 fd 读回");

    // RFC-06（已修复）量级验证：右分支分配型连续 Fork 经 merge 锚点吸收后，
    // 父 next_fd 线性增长（每轮 +2^48 区间一跳，100 轮 ≈ 2^56），远低于 u64
    // 溢出阈值；修复前 Σk·2^48 二次增长，~362 轮即溢出（R2 记录，已修）。
    let n = rt
        .registry()
        .allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
    assert!(
        n < (1u64 << 62),
        "100 轮后 next_fd 应保持线性量级（< 2^62），实测 {n}"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4：值类型矩阵 —— Value 全部变体经 Sequential next → 并行 Fork
// combine（值跨 spawn_blocking 线程边界往返）→ 最终保真。R1 值流只覆盖
// fd/bytes 链；本测试枚举全部 10 个变体（含大 Fd、负 I64、Unicode Str、
// 嵌套 List、SocketAddr），证明任意变体在 CPS 传递与线程间合并中无丢失/
// 截断/类型翻转。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn value_matrix_all_variants_through_pure_combine_next() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let variants = vec![
        Value::Unit,
        Value::Bool(true),
        Value::U64(0xDEAD_BEEF_CAFE_F00D),
        Value::I64(-9_007_199_254_740_993),
        Value::Bytes(vec![0x00, 0x01, 0xFE, 0xFF, 0x7F]),
        Value::Str("值类型矩阵 𝄞 中文字符串 😀".to_string()),
        Value::Fd(0x0FF0_0000_0000_0001),
        Value::Pid(65_535),
        Value::Addr("127.0.0.1:65535".parse().unwrap()),
        Value::List(vec![
            Value::U64(1),
            Value::Str("nested".to_string()),
            Value::Bool(false),
            Value::Bytes(vec![9, 8, 7]),
        ]),
    ];

    for v in variants {
        let expect = v.clone();
        let bp = Action::Sequential {
            current: Box::new(Action::Pure(v.clone())),
            next: Box::new(move |got| {
                assert_eq!(got, expect, "Sequential next 值保真（收到 {got:?}）");
                Action::Fork {
                    // 纯值分支：无 syscall 声明 → 无冲突 → 并行路径 → 值经
                    // spawn_blocking 线程边界往返（Value: Send 保真验证）。
                    left: Box::new(Action::Pure(got.clone())),
                    right: Box::new(Action::Pure(got.clone())),
                    combine: Box::new(move |a, b| {
                        assert_eq!(a, expect, "combine 左分支值保真");
                        assert_eq!(b, expect, "combine 右分支值保真");
                        Action::Pure(a)
                    }),
                }
            }),
        };
        let out = rt.run_blocking(bp).unwrap();
        assert_eq!(out, v, "值经 next+Fork combine 往返后最终保真");
        assert!(rt.undo_stack().is_empty(), "纯值路径不产生 undo");
    }
}
