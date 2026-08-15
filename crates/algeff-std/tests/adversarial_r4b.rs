//! R4b 对抗审计（第 4 轮 B 块：隔离与边界）—— algeff-std 部分：多 Runtime
//! 隔离 + Open 语义边界（真实 TokioExecutor 全链路）。
//!
//! 攻击方法论：与 R1-R3 相同——不 mock、全部经真实 `Runtime` +
//! `TokioExecutor` 全链路（`run_blocking` → `interpret` → 共享执行器通道）。
//! R1-R3 已覆盖的面（可逆深链/游标、线性、并发 Fork、错误 put_back、值流、
//! 确定性、MutexLock、Timeout、Mmap、网络深度、SendFile flush、Alloc）**不
//! 重复**，本块只攻击：
//!
//! ## 攻击面 1：多 Runtime 隔离
//! 1a. **同文件**：两个 Runtime 各执行同一「Open→Seek→Write→Read」蓝图
//!     写同一文件的非重叠区间——fd 编号独立（各自首个分配均为 0）、undo
//!     栈独立（A 的 Replace 不触碰 B 的 undo、只撤销 A 的效果）；
//! 1b. **同端口**：Runtime A 绑定端口 P；Runtime B 绑定同一端口必须失败
//!     （无跨 Runtime 共享/劫持）；A 的 listener 不受 B 失败影响；A 关闭后
//!     B 可绑定同一端口（无跨执行器句柄泄漏占用端口）；
//! 1c. **并行线程 20 轮**：两个线程各持一个 Runtime，同文件同蓝图并发跑
//!     20 轮（非重叠区间），各自 fd/undo 状态独立、结果确定。
//!
//! ## 攻击面 4：Open 语义边界（OpenFlags 组合矩阵，真实文件行为）
//! 8 种关键组合：只读/只写+create/rw/rw+create/append/创建+exclusive/
//! exclusive 撞已存在/truncate；外加 exclusive 失败无状态毒化与
//! truncate 后长度 0 + 写入增长 + undo 复原的边界断言。
//!
//! Windows 端口预算：本文件合计 2 个 TCP 监听（串行复用同一端口），远低于
//! 500 上限。平台差异（RFC-10 已修复）：修复前 Windows 上 create_new 撞已
//! 存在文件返回 ERROR_FILE_EXISTS(80)（未映射进 14 种 → `SysError::Other(80)`）
//! 、Unix 返回 EEXIST(17) → `SysError::AlreadyExists`，断言按平台分支；
//! 修复后 A5 执行器层将 Windows 码归一化为 POSIX 语义，两平台均
//! `AlreadyExists`（断言不再按平台分支）。

use std::net::SocketAddr;
use std::path::PathBuf;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceHandle, ResourceInner, ResourceUsage,
    Runtime, SysError, TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（src/ 冻结不可改，测试内复制；与 R1/R2 相同约定）──────────────

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

/// List([Fd, Bytes]) → (fd, bytes)。
fn pair_fd_bytes(v: &Value) -> (u64, Vec<u8>) {
    match v {
        Value::List(l) if l.len() == 2 => (
            fd_of(&l[0]),
            match &l[1] {
                Value::Bytes(b) => b.clone(),
                other => panic!("期望 Bytes，得到 {other:?}"),
            },
        ),
        other => panic!("期望 List([Fd, Bytes])，得到 {other:?}"),
    }
}

/// 同文件蓝图：Open → Seek(offset) → Write(data) → Seek(offset) →
/// Read(data.len()) → List([Fd, Bytes])（读回自身写入区间）。
fn open_seek_write_read(path: PathBuf, offset: u64, data: &'static [u8]) -> Action {
    syscall(
        DataOp::Open {
            path: path.clone(),
            flags: rw_flags(),
        },
        vec![wr_path(path)],
        move |v| {
            let fd = fd_of(&v);
            syscall(
                DataOp::Seek {
                    fd,
                    offset: offset as i64,
                    whence: std::io::SeekFrom::Start(0),
                },
                vec![rd(fd)],
                move |_| {
                    syscall(
                        DataOp::Write {
                            fd,
                            data: data.to_vec(),
                        },
                        vec![wr(fd)],
                        move |_| {
                            syscall(
                                DataOp::Seek {
                                    fd,
                                    offset: offset as i64,
                                    whence: std::io::SeekFrom::Start(0),
                                },
                                vec![rd(fd)],
                                move |_| {
                                    syscall(
                                        DataOp::Read {
                                            fd,
                                            len: data.len(),
                                        },
                                        vec![rd(fd)],
                                        move |v| Action::Pure(Value::List(vec![Value::Fd(fd), v])),
                                    )
                                },
                            )
                        },
                    )
                },
            )
        },
    )
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1a：两个 Runtime 同蓝图同文件 —— fd 独立、undo 独立、执行器状态
// 独立；A 的 recover 只撤销 A 的效果。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn multi_runtime_same_file_blueprint_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("shared.txt");
    std::fs::write(&p, b"0123456789").unwrap();

    let mut rt_a = Runtime::new(Box::new(TokioExecutor::new()));
    let mut rt_b = Runtime::new(Box::new(TokioExecutor::new()));

    // 同一蓝图结构：A 写 [0,4)、B 写 [4,8)。
    let v = rt_a
        .run_blocking(open_seek_write_read(p.clone(), 0, b"AAAA"))
        .unwrap();
    let (fda, got_a) = pair_fd_bytes(&v);
    assert_eq!(got_a, b"AAAA", "A 读回自身区间");

    let v = rt_b
        .run_blocking(open_seek_write_read(p.clone(), 4, b"BBBB"))
        .unwrap();
    let (fdb, got_b) = pair_fd_bytes(&v);
    assert_eq!(got_b, b"BBBB", "B 读回自身区间");

    // fd 编号空间独立：两 Runtime 各自首个分配均为 0（可相同——互不可见）。
    assert_eq!(fda, 0, "A 的 fd 编号独立（首个分配 = 0）");
    assert_eq!(fdb, 0, "B 的 fd 编号独立（首个分配 = 0）");

    // undo 独立：各栈一个 Write undo。
    assert_eq!(rt_a.undo_stack().len(), 1, "A 栈 1 个 undo");
    assert_eq!(rt_b.undo_stack().len(), 1, "B 栈 1 个 undo");

    // 物理文件：两运行时效果叠加（非重叠区间）。
    assert_eq!(std::fs::read(&p).unwrap(), b"AAAABBBB89");

    // A 单独 recover（Replace）→ A 的写被撤销，B 的效果与 undo 不受影响。
    rt_a.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"0123BBBB89",
        "A 撤销只回滚 A 的效果，B 的写仍在"
    );
    assert_eq!(rt_a.undo_stack().len(), 0, "A 的栈已清空");
    assert_eq!(
        rt_b.undo_stack().len(),
        1,
        "B 的 undo 栈不受 A 的 recover 影响"
    );

    // B 再 recover → 全部恢复原状。
    rt_b.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        std::fs::read(&p).unwrap(),
        b"0123456789",
        "B 撤销后完全复原"
    );
    assert!(rt_b.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1b：两个 Runtime 绑定同一端口 —— A 持有期间 B 必须失败（无跨
// Runtime 共享）；A 的 listener 不受 B 失败影响；A 关闭后 B 可绑定同端口
// （无跨执行器句柄泄漏占用端口）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn multi_runtime_same_port_bind_isolated() {
    let mut rt_a = Runtime::new(Box::new(TokioExecutor::new()));
    let mut rt_b = Runtime::new(Box::new(TokioExecutor::new()));

    // A 绑定 127.0.0.1:0 → 取内核分配的端口 P。
    let v = rt_a
        .run_blocking(syscall(
            DataOp::TcpBind {
                addr: "127.0.0.1:0".parse().unwrap(),
            },
            vec![],
            Action::Pure,
        ))
        .unwrap();
    let lfd_a = fd_of(&v);
    let p: SocketAddr = match rt_a.registry().lookup(lfd_a).unwrap() {
        ResourceHandle::TcpListener(l) => l.local_addr().unwrap(),
        other => panic!("期望 TcpListener，得到 {other:?}"),
    };

    // B 绑定同一端口 → 必须失败（地址占用；错误码平台相关，仅断言失败语义）。
    let e = rt_b
        .run_blocking(syscall(DataOp::TcpBind { addr: p }, vec![], Action::Pure))
        .unwrap_err();
    eprintln!("R4B 同端口：B 在 A 持有期间绑定 {p} 失败：{e:?}");

    // A 的 listener 不受 B 失败影响：仍可寻址、端口不变。
    match rt_a.registry().lookup(lfd_a).unwrap() {
        ResourceHandle::TcpListener(l) => {
            assert_eq!(l.local_addr().unwrap(), p, "A 的 listener 不受 B 失败影响")
        }
        other => panic!("期望 TcpListener，得到 {other:?}"),
    }

    // A 关闭 listener → 端口释放（op_close 经 reg.take 丢弃唯一 Arc）。
    rt_a.run_blocking(syscall(
        DataOp::Close { fd: lfd_a },
        vec![ow(lfd_a)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt_a.undo_stack().is_empty(), "网络 ops 不产生 undo");

    // B 现可绑定同一端口：无跨执行器句柄泄漏占用端口。
    let v = rt_b
        .run_blocking(syscall(DataOp::TcpBind { addr: p }, vec![], Action::Pure))
        .unwrap();
    let lfd_b = fd_of(&v);
    assert_eq!(lfd_b, 0, "B 的 fd 编号独立");
    rt_b.run_blocking(syscall(
        DataOp::Close { fd: lfd_b },
        vec![ow(lfd_b)],
        Action::Pure,
    ))
    .unwrap();
    assert!(rt_b.undo_stack().is_empty());
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 1c：并行线程各持一个 Runtime，同文件同蓝图并发跑 20 轮
// （非重叠区间）—— 各自 fd/undo 状态独立、结果确定。
// ══════════════════════════════════════════════════════════════════════

/// 单线程 20 轮：每轮 Open → Seek(offset) → Write(payload) → 读回自身区间
/// → Close；完成后断言 undo 栈长度 = 轮数（每轮恰一个 Write undo）。
fn run_rounds(path: PathBuf, offset: u64, payload: &'static [u8], rounds: usize) {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    for round in 0..rounds {
        // Open：空资源声明（pdr §18 用户责任边界；20 轮避免 A4 线性累积）。
        let v = rt
            .run_blocking(syscall(
                DataOp::Open {
                    path: path.clone(),
                    flags: rw_flags(),
                },
                vec![],
                Action::Pure,
            ))
            .unwrap();
        let fd = fd_of(&v);
        // Seek(offset) → Write(payload)
        rt.run_blocking(syscall(
            DataOp::Seek {
                fd,
                offset: offset as i64,
                whence: std::io::SeekFrom::Start(0),
            },
            vec![],
            move |_| {
                syscall(
                    DataOp::Write {
                        fd,
                        data: payload.to_vec(),
                    },
                    vec![],
                    Action::Pure,
                )
            },
        ))
        .unwrap();
        // 读回自身区间（另一线程区域不重叠 → 断言确定性）。
        let got = read_back(&mut rt, fd, 8);
        assert_eq!(
            &got[offset as usize..offset as usize + payload.len()],
            payload,
            "第 {round} 轮自身区间回读一致（跨线程互不干扰）"
        );
        // Close（Own 终结；Write→Close 合法序列，pdr §14）。
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
            .unwrap();
    }
    assert_eq!(
        rt.undo_stack().len(),
        rounds,
        "每轮一个 Write undo，栈独立累积"
    );
}

#[test]
fn multi_runtime_two_threads_20_rounds_same_blueprint_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("threads-shared.txt");
    std::fs::write(&p, b"01234567").unwrap();

    let pa = p.clone();
    let t_a = std::thread::spawn(move || run_rounds(pa, 0, b"AAAA", 20));
    let pb = p.clone();
    let t_b = std::thread::spawn(move || run_rounds(pb, 4, b"BBBB", 20));
    t_a.join().expect("线程 A 未 panic");
    t_b.join().expect("线程 B 未 panic");

    // 终态：两线程各 20 轮写后，非重叠区间各自内容完整。
    assert_eq!(std::fs::read(&p).unwrap(), b"AAAABBBB");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4a：OpenFlags 组合矩阵（8 种关键组合，真实文件行为）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn open_flags_8_combination_matrix_real_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // (1) read-only on existing
    let p1 = dir.path().join("c1.txt");
    std::fs::write(&p1, b"existing").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p1.clone(),
                flags: read_only_flags(),
            },
            vec![rd_path(p1.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd1 = fd_of(&v);
    assert_eq!(
        read_back(&mut rt, fd1, 8),
        b"existing",
        "(1) 只读打开可读原内容"
    );

    // (2) write-only + create on new
    let p2 = dir.path().join("c2.txt");
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p2.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p2.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd2 = fd_of(&v);
    assert!(p2.exists(), "(2) create 创建了新文件");
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd2,
            data: b"hi".to_vec(),
        },
        vec![wr(fd2)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&p2).unwrap(), b"hi", "(2) 只写句柄写入生效");

    // (3) read+write on existing
    let p3 = dir.path().join("c3.txt");
    std::fs::write(&p3, b"rw").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p3.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p3.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd3 = fd_of(&v);
    assert_eq!(read_back(&mut rt, fd3, 2), b"rw", "(3) rw 打开可读");

    // (4) read+write+create on new
    let p4 = dir.path().join("c4.txt");
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p4.clone(),
                flags: rw_flags(),
            },
            vec![wr_path(p4.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd4 = fd_of(&v);
    assert_eq!(
        std::fs::metadata(&p4).unwrap().len(),
        0,
        "(4) create 新文件初始长度 0"
    );

    // (5) append on existing: 写入落在文件尾（O_APPEND 语义）
    let p5 = dir.path().join("c5.txt");
    std::fs::write(&p5, b"HELLO").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p5.clone(),
                flags: OpenFlags {
                    write: true,
                    append: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p5.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd5 = fd_of(&v);
    rt.run_blocking(syscall(
        DataOp::Write {
            fd: fd5,
            data: b"XX".to_vec(),
        },
        vec![wr(fd5)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(
        std::fs::read(&p5).unwrap(),
        b"HELLOXX",
        "(5) append 写入落在文件尾"
    );

    // (6) create+exclusive on new → 成功
    let p6 = dir.path().join("c6.txt");
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p6.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p6.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd6 = fd_of(&v);
    assert!(p6.exists(), "(6) exclusive 创建新文件成功");

    // (7) create+exclusive on existing → 必须失败（RFC-10 修复后两平台均 EEXIST）
    let p7 = dir.path().join("c7.txt");
    std::fs::write(&p7, b"keep").unwrap();
    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p7.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p7.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "(7) exclusive 撞已存在（EEXIST，RFC-10 归一化后跨平台一致）"
    );
    assert_eq!(std::fs::read(&p7).unwrap(), b"keep", "(7) 失败不改动原文件");

    // (8) write+truncate on existing → 打开后长度 0
    let p8 = dir.path().join("c8.txt");
    std::fs::write(&p8, b"HELLOWORLD").unwrap();
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p8.clone(),
                flags: OpenFlags {
                    write: true,
                    truncate: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p8.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd8 = fd_of(&v);
    assert_eq!(
        std::fs::metadata(&p8).unwrap().len(),
        0,
        "(8) truncate 打开后长度 0"
    );

    // 全部成功打开的 fd 正常 Close（(7) 失败未分配 fd）。
    for fd in [fd1, fd2, fd3, fd4, fd5, fd6, fd8] {
        rt.run_blocking(syscall(DataOp::Close { fd }, vec![ow(fd)], Action::Pure))
            .unwrap();
    }
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4b：exclusive 撞已存在失败 —— 不分配 fd、不产生 undo、不改文件、
// 不毒化运行时（同 Runtime 随后可正常 exclusive 创建新文件）。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn open_exclusive_existing_fails_no_state_poison() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("exists.txt");
    std::fs::write(&p, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "exclusive 撞已存在必须失败（RFC-10 归一化后跨平台一致）"
    );
    assert_eq!(std::fs::read(&p).unwrap(), b"original", "失败不改动文件");

    // 失败不分配 fd、不产生 undo。
    assert!(rt.registry().lookup(0).is_none(), "失败不分配句柄");
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");

    // 同一 Runtime 随后可正常 exclusive 创建新文件（无状态毒化）。
    let p2 = dir.path().join("fresh.txt");
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p2.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p2.clone())],
            Action::Pure,
        ))
        .unwrap();
    assert!(p2.exists(), "失败后同 Runtime 创建新文件仍成功");
    let fd = fd_of(&v);
    assert_eq!(fd, 0, "失败未消耗 fd 编号（首次成功分配仍为 0）");
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4d：同路径 Write 模式重开（R6-F2 / RFC-12，audit/r6 §3 锁定的同
// 路径盲区）——exclusive 撞已存在失败后，同一 Runtime **同路径**以 Write 模式
// 重开必须成功。修复前 InvalidInput：`check_linear` 在 syscall 执行前预插入的
// 路径 Write 标记在物理失败后残留毒化；上方 r4b 原测试只覆盖了异路径重开。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn failed_exclusive_open_same_path_write_reopen_ok() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("exists.txt");
    std::fs::write(&p, b"original").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let e = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    write: true,
                    create: true,
                    exclusive: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(
        e,
        SysError::AlreadyExists,
        "exclusive 撞已存在失败（RFC-10 归一化）"
    );
    assert!(rt.undo_stack().is_empty(), "失败不产生 undo");
    assert!(rt.registry().lookup(0).is_none(), "失败不分配 fd");

    // RFC-12 修复：同路径以 Write 模式重开成功（修复前 InvalidInput ——
    // 失败 Open(w) 预插入的路径 Write 标记残留毒化）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);

    // 重开后可写（真实全链路：写生效 + A4 至多一次仍成立，标记计数不重复消费）。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"patched".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    let e2 = rt
        .run_blocking(syscall(
            DataOp::Write {
                fd,
                data: b"again".to_vec(),
            },
            vec![wr(fd)],
            Action::Pure,
        ))
        .unwrap_err();
    assert_eq!(e2, SysError::InvalidInput, "成功路径 A4 至多一次不变");
    assert_eq!(
        rt.undo_stack().len(),
        1,
        "被 A4 拦截的二写不产生 undo（栈中仅剩首次成功写的 undo）"
    );
    assert!(
        std::fs::read(&p).unwrap().starts_with(b"patched"),
        "重开后的写真实生效（物理文件）"
    );
}

// ══════════════════════════════════════════════════════════════════════
// 攻击面 4c：truncate 打开后长度 0；写入从 0 增长；recover（Write undo）
// 复原回长度 0。
// ══════════════════════════════════════════════════════════════════════

#[test]
fn open_truncate_zeroes_len_then_write_grows_and_undo_restores() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("trunc.txt");
    std::fs::write(&p, b"HELLOWORLD").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // read+write：保证 Write 的写前读成功（Full 撤销策略需可读句柄；只写
    // 句柄会降级 BestEffort 无 undo，见 executor.rs op_write 注释）。
    let v = rt
        .run_blocking(syscall(
            DataOp::Open {
                path: p.clone(),
                flags: OpenFlags {
                    read: true,
                    write: true,
                    truncate: true,
                    create: true,
                    ..Default::default()
                },
            },
            vec![wr_path(p.clone())],
            Action::Pure,
        ))
        .unwrap();
    let fd = fd_of(&v);
    assert_eq!(
        std::fs::metadata(&p).unwrap().len(),
        0,
        "truncate 打开后长度 0"
    );
    assert!(rt.undo_stack().is_empty(), "Open 无 undo");

    // 写入 3 字节 → 文件从 0 增长到 3。
    rt.run_blocking(syscall(
        DataOp::Write {
            fd,
            data: b"abc".to_vec(),
        },
        vec![wr(fd)],
        Action::Pure,
    ))
    .unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"abc", "truncate 后写入生效");

    // Write 对 0 长文件也有 undo（原内容为空，Full 策略）→ recover 复原回
    // 长度 0。
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());
    assert_eq!(
        std::fs::metadata(&p).unwrap().len(),
        0,
        "recover 撤销写后回到 truncate 后的长度 0"
    );
}
