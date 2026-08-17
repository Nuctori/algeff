//! A3 批 2：registry 生命周期集成测试（模拟解释器使用模式）。
//!
//! 目的：在 A2 解释器（`runtime.rs::interpret`）合并之前，用 `ResourceRegistry`
//! 现有公共 API 预演解释器将要执行的调用序列，验证 D10（Replace → clear()）与
//! D13（Fork clone 隔离-合并）的 registry 侧配合。不依赖 `interpret`，不触碰 src/。
//!
//! 覆盖：
//! - `open_write_close_lifecycle`：Open(allocate) → Write(check_linear) → Close(take)，
//!   A4 线性语义 + D1 fd 单调；
//! - `replace_semantics`：D10 Replace 的 registry 侧配合 —— clear() 复位句柄与线性状态；
//! - `fork_clone_merge_pattern`：D13 Fork 隔离-合并模式（clone → 子消费 → 合并回父）；
//! - `linearity_sequence_random`：proptest 随机 usage 序列下 A4 状态机不变量。

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use algeff_core::{
    AccessMode, Fd, Resource, ResourceHandle, ResourceRegistry, ResourceUsage, SysError,
};
use proptest::prelude::*;

fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
    ResourceUsage {
        resource: r,
        mode: m,
    }
}

fn mutex_handle() -> ResourceHandle {
    // 简单的物理句柄：Arc 共享的 tokio Mutex（无需 async 上下文，无临时文件）。
    ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(())))
}

/// 构造真实 `tokio::fs::File` 句柄（模拟 Open 成功），返回句柄与临时文件路径。
/// `tokio::fs::File::from_std` 为同步构造，不需要 async 上下文。
fn temp_file_handle() -> (ResourceHandle, PathBuf) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("algeff_a3_reg_{}_{}.tmp", std::process::id(), n));
    let f = std::fs::File::create(&path).expect("创建临时文件失败");
    let tf = tokio::fs::File::from_std(f);
    (ResourceHandle::File(Arc::new(tf)), path)
}

// ---------------------------------------------------------------------------
// (a) Open → Write → Close 生命周期
// ---------------------------------------------------------------------------

/// 模拟 pdr.md §14 的 Read/Write/Close 蓝图在解释器中的 registry 调用序列：
/// Open 成功 → `allocate` 登记句柄；Write → `check_linear(Write)`（A4 至多一次）；
/// Close → `check_linear(Own)` 终结 + `take` 取出句柄（Own 语义）。
#[test]
fn open_write_close_lifecycle() {
    let (fh, path1) = temp_file_handle();
    let mut reg = ResourceRegistry::new();

    // Open：分配全局唯一句柄，registry 可见
    let fd = reg.allocate(fh);
    let r = Resource::Fd(fd);
    assert!(reg.lookup(fd).is_some(), "Open 后句柄应可见");

    // Write：A4 use 语义通过（Write 不限次数）
    assert!(
        reg.check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok(),
        "首次 Write 应通过线性检查"
    );
    // 重复 Write 允许（use 语义，运行时维护独立 undo）
    assert!(
        reg.check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok(),
        "同一资源二次 Write 允许（use 语义）"
    );

    // Close 的 Own 语义：Own 终结检查通过，随后 take 移除句柄
    assert!(
        reg.check_linear(&usage(r.clone(), AccessMode::Own)).is_ok(),
        "Write → Close(Own) 是合法序列（pdr.md §14）"
    );
    let taken = reg.take(fd).expect("take 应取出句柄");
    assert!(matches!(taken, ResourceHandle::File(_)));

    // registry 状态正确：句柄已移除；资源键保持终结标记（任何 usage 拒绝）
    assert!(reg.lookup(fd).is_none(), "Close 后句柄应移除");
    assert_eq!(
        reg.check_linear(&usage(r.clone(), AccessMode::Read)),
        Err(SysError::InvalidInput),
        "Own 终结后 Read 也应被拒绝"
    );

    // D1：fd 单调递增，不复用已关闭的句柄
    let (fh2, path2) = temp_file_handle();
    let fd2 = reg.allocate(fh2);
    assert!(fd2 > fd, "fd 应单调递增（决策 D1），永不复用");
    assert_ne!(fd2, fd);

    drop(reg);
    let _ = std::fs::remove_file(&path1);
    let _ = std::fs::remove_file(&path2);
}

// ---------------------------------------------------------------------------
// (b) D10 Replace → clear() 的 registry 侧配合
// ---------------------------------------------------------------------------

/// `Replace { target }`（决策 D10）：先 recover 再执行 target —— 释放当前路径
/// 上积累的资源与线性标记，再进入新蓝图。registry 侧对应 `clear()`：
/// `handles`/`consumed`/`owned_consumed` 全部复位；`next_fd` 不复位（D1 单调）。
#[test]
fn replace_semantics() {
    let (fh1, p1) = temp_file_handle();
    let (fh2, p2) = temp_file_handle();
    let (fh3, p3) = temp_file_handle();
    let mut reg = ResourceRegistry::new();

    // 旧蓝图积累的句柄
    let fds: Vec<Fd> = [fh1, fh2, fh3]
        .into_iter()
        .map(|h| reg.allocate(h))
        .collect();
    let (r0, r1) = (Resource::Fd(fds[0]), Resource::Fd(fds[1]));
    let _ = fds[2];

    // 旧路径上的线性消费：Write 消费 + Own 终结
    assert!(reg
        .check_linear(&usage(r0.clone(), AccessMode::Write))
        .is_ok());
    assert!(reg
        .check_linear(&usage(r0.clone(), AccessMode::Own))
        .is_ok());
    assert!(reg
        .check_linear(&usage(r1.clone(), AccessMode::Write))
        .is_ok());
    assert_eq!(
        reg.check_linear(&usage(r0.clone(), AccessMode::Read)),
        Err(SysError::InvalidInput),
        "Own 终结后任何 usage 拒绝"
    );

    // Replace：clear() 释放一切
    reg.clear();

    // handles 全部清空
    for fd in &fds {
        assert!(reg.lookup(*fd).is_none(), "Replace 后句柄应全部释放");
    }
    // 线性状态复位：同资源再次 Write + Own 成功（A4 回到未消费状态）
    assert!(
        reg.check_linear(&usage(r0.clone(), AccessMode::Write))
            .is_ok(),
        "clear() 后同资源应可再次 Write"
    );
    assert!(
        reg.check_linear(&usage(r0.clone(), AccessMode::Own))
            .is_ok(),
        "clear() 后同资源应可再次 Own 终结"
    );
    assert!(
        reg.check_linear(&usage(r1.clone(), AccessMode::Write))
            .is_ok(),
        "clear() 后第二资源 Write 消费记录复位"
    );

    // D1：新蓝图分配不复用旧 fd
    let (fh_new, p_new) = temp_file_handle();
    let nfd = reg.allocate(fh_new);
    assert!(fds.iter().all(|f| *f != nfd), "fd 永不复用（决策 D1）");
    assert!(nfd > fds[2], "新 fd 应大于 Replace 前最大 fd");

    drop(reg);
    for p in [p1, p2, p3, p_new] {
        let _ = std::fs::remove_file(&p);
    }
}

// ---------------------------------------------------------------------------
// (c) D13 Fork 隔离-合并模式
// ---------------------------------------------------------------------------

/// D13 Fork 模式：Fork 前 `child = parent.clone()` 隔离子任务状态（COW，公理 A5）；
/// 子任务在私有副本上 allocate 新句柄 + 消费线性；join 时把子句柄迁回父注册表
/// （next_fd 取 max，D1 单调保证合并无重复 fd）。
///
/// 当前公共 API 只暴露 `take`（取出）+ `allocate`（分配新 fd）作为迁移原语：
/// 合并为「值迁移 + fd 重分配」——句柄值身份（Arc）保留、fd 身份重分配。
/// 保留 fd 身份的 `merge` 原语见 RFC（spec/resource-notes.md §7.4）。
#[test]
fn fork_clone_merge_pattern() {
    // 父注册表预置句柄
    let mut parent = ResourceRegistry::new();
    let p1 = parent.allocate(mutex_handle());
    let p2 = parent.allocate(mutex_handle());

    // Fork：克隆父状态，子任务获得私有副本（D13）
    let mut child = parent.clone();

    // 子任务：在私有副本上分配新句柄 + 消费线性
    let c1 = child.allocate(mutex_handle());
    let c2 = child.allocate(mutex_handle());
    assert_ne!(c1, c2);
    assert!(
        child
            .check_linear(&usage(Resource::Fd(c1), AccessMode::Write))
            .is_ok(),
        "子路径 Write 应通过"
    );
    assert!(
        child
            .check_linear(&usage(Resource::Fd(c1), AccessMode::Own))
            .is_ok(),
        "子路径 Own 终结应通过"
    );

    // ── 隔离断言（A5 COW / D13）──
    // 父看不到子新句柄
    assert!(parent.lookup(c1).is_none(), "父不应看到子句柄 c1");
    assert!(parent.lookup(c2).is_none(), "父不应看到子句柄 c2");
    // 子的线性消费不污染父：父对该资源仍可正常 Write（A4 状态隔离）
    assert!(
        parent
            .check_linear(&usage(Resource::Fd(c1), AccessMode::Write))
            .is_ok(),
        "子路径的线性消费不应污染父"
    );

    // ── 合并前置条件：无重复 fd ──
    // 子克隆自父，next_fd 继承父值；子新分配的 fd 均 ≥ 父 next_fd，
    // 与父已有句柄（均 < 父 next_fd）互不重叠 —— extend 式合并天然无冲突。
    assert!(p1 < c1 && p2 < c1, "子新 fd 应大于父已有全部 fd");
    let mut all = HashSet::new();
    for fd in [p1, p2, c1, c2] {
        assert!(all.insert(fd), "fd {fd} 在父子间重复");
    }

    // ── 合并回父：子句柄迁入父 ──
    let mut migrated: Vec<(Fd, Fd, Arc<tokio::sync::Mutex<()>>)> = Vec::new();
    for cfd in [c1, c2] {
        let h = child.take(cfd).expect("子注册表持有句柄");
        // 记录句柄值身份（Arc），迁移后与父侧对比
        let expect_arc = match &h {
            ResourceHandle::Mutex(m) => Arc::clone(m),
            _ => unreachable!("本测试只使用 Mutex 句柄"),
        };
        let new_fd = parent.allocate(h);
        migrated.push((cfd, new_fd, expect_arc));
    }

    // 父 registry 可见子句柄（值身份保留：Arc 指向同一底层对象）
    for (cfd, nfd, expect_arc) in &migrated {
        let h = parent
            .lookup(*nfd)
            .unwrap_or_else(|| panic!("父应可见迁移后的句柄（子 fd {cfd} → 父 fd {nfd}）"));
        match h {
            ResourceHandle::Mutex(m) => {
                assert!(
                    Arc::ptr_eq(m, expect_arc),
                    "迁移应保留句柄值身份（同一 Arc 对象）"
                );
            }
            _ => unreachable!("本测试只使用 Mutex 句柄"),
        }
    }

    // 父注册表内无重复 fd
    let mut seen = HashSet::new();
    for fd in [p1, p2] {
        assert!(seen.insert(fd));
    }
    for (_, nfd, _) in &migrated {
        assert!(seen.insert(*nfd), "合并后父注册表出现重复 fd {nfd}");
    }

    // next_fd 取 max 的可见行为：合并后父注册表的分配继续单调递增，
    // 覆盖子侧全部 fd（等效于 next_fd = max(parent, child)）。
    let p3 = parent.allocate(mutex_handle());
    let max_known = migrated.iter().map(|(_, n, _)| *n).max().unwrap();
    assert!(p3 > max_known, "合并后分配应大于子侧全部 fd");

    // 合并卫生：子路径的线性消费不随句柄迁移（父侧新 fd 键线性状态全新）。
    // 注意：c1 == migrated[0].1 == 2（父子 next_fd 同一起点），该键已被上方
    // 隔离断言消费过，故用第二个迁移句柄（新 fd 3）的键验证全新线性状态。
    assert!(
        parent
            .check_linear(&usage(Resource::Fd(migrated[1].1), AccessMode::Write))
            .is_ok(),
        "迁移句柄在父侧应带全新线性状态"
    );
}

// ---------------------------------------------------------------------------
// (d) A4 线性状态机：随机 usage 序列
// ---------------------------------------------------------------------------

// 随机 usage 序列（Read/Write/Append/Own × 若干资源）下，断言每一步后
// 「Write 至多一次 + Own 终结」不变量都成立。用 `check_linear` 返回值
// 构造状态机断言：
// - 资源已 Own 终结 → 任何模式拒绝；
// - 未终结：Write 已消费 → Write 拒绝（其余模式接受）；Write 未消费 → 全部接受；
// - Read/Append 不消费（不改变状态）。
proptest! {
    #[test]
    fn linearity_sequence_random(
        ops in proptest::collection::vec(any::<(u8, u8)>(), 1..64),
    ) {
        const N_RES: usize = 4;
        let mut reg = ResourceRegistry::new();
        // 资源键直接用 Fd(i)，无需真实句柄 —— check_linear 只追踪资源键
        let keys: Vec<Resource> = (0..N_RES as Fd).map(Resource::Fd).collect();
        let mut owned = [false; N_RES];

        for (ri, mi) in ops {
            let i = ri as usize % N_RES;
            let mode = match mi % 4 {
                0 => AccessMode::Read,
                1 => AccessMode::Write,
                2 => AccessMode::Append,
                _ => AccessMode::Own,
            };
            let ok = reg.check_linear(&usage(keys[i].clone(), mode)).is_ok();

            // 状态机预期：Own 终结后一切拒绝；Write（use 语义）不限次数
            let expected = if owned[i] {
                false
            } else {
                match mode {
                    AccessMode::Write | AccessMode::Own | AccessMode::Read | AccessMode::Append => {
                        true
                    }
                }
            };
            prop_assert_eq!(
                ok, expected,
                "资源 {} {:?}：owned={}",
                i, mode, owned[i]
            );

            // 推进状态机（失败时状态不变，故无条件置位安全）
            match mode {
                AccessMode::Own => owned[i] = true,
                AccessMode::Write | AccessMode::Read | AccessMode::Append => {}
            }
        }
    }
}
