//! DX 迭代 1 示例测试：`do_!` 宏 + `algeff_std::dx` 哲学承诺的可执行验证
//! （镜像 README 未来 §3 示例，设计论证见 docs/src/dx-design.md）。
//!
//! 覆盖四条承诺：
//! 1. **最小文件 IO ≤15 行、接近普通 Rust**——do_! 块内 open/write/seek/read/
//!    close 直书，fd 经 let 绑定贯穿，尾表达式即链最终值；真实执行写读回；
//! 2. **错误路径**——链中任一步失败沿 CPS 链上抛（后续语句不执行），
//!    返回真实 `SysError` 而非 panic；
//! 3. **与 plan! 共存**——plan! 包裹 do_!（命令式阶段内嵌声明式骨架）、
//!    do_! 内嵌 plan!/choose!（骨架内声明式子步骤）；
//! 4. **资源自动推导 + 显式覆盖**——infer_usage 模式表全量断言；
//!    `syscall_with` 覆盖默认推导。
//!
//! 迭代 2 新增（§6）：文件面全操作一个 do_! 覆盖、TCP bind/accept 骨架、
//! 错误处理 `dx::catch`（do_! 内组合）、do_! 与 plan!/fork! 混合、
//! munmap/send_file/dup2 包装补齐。

//!    `syscall_with` 覆盖默认推导；
//! 5. **锁重入 + 信号可重复（R7 DX 修复回归）**——mutex lock→unlock→再 lock
//!    成功；SendSignal 二次发送不被 A4 线性域拒绝。

use algeff_core::prelude::*;
use algeff_core::{AccessMode, MmapProt, OpenFlags, PipeFlags, Resource, ResourceUsage, SysError};
use algeff_macro::{choose, do_, fork, plan};
use algeff_std::dx;
use algeff_std::TokioExecutor;

fn rt() -> Runtime {
    Runtime::new(Box::new(TokioExecutor::new()))
}

fn usage(resource: Resource, mode: AccessMode) -> ResourceUsage {
    ResourceUsage { resource, mode }
}

fn p(s: &str) -> Resource {
    Resource::Path(s.to_string())
}

fn f(fd: u64) -> Resource {
    Resource::Fd(fd)
}

/// 打开文件（写 + 建）的通用 flags。
fn open_rw_create() -> OpenFlags {
    OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    }
}

// ── 1. 最小文件 IO：写 → 回读（真实执行）──────────────────────────────

#[test]
fn minimal_file_io_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hello.txt");
    let flags = open_rw_create();

    // 示例体（不含测试断言）共 7 行：open/write/seek/read/close + 尾表达式
    let blueprint = do_! {
        let fd = dx::open(&path, flags);
        dx::write(&fd, b"hello dx".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    // 构造即纯数据：外层为 CPS Sequential 链（与手写 and_then 等价）。
    assert!(matches!(blueprint, Action::Sequential { .. }));

    // 真实执行：值流贯穿（fd 绑定 → Bytes 尾值），物理文件落盘。
    let v = rt().run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"hello dx".to_vec()));
    assert_eq!(std::fs::read(&path).unwrap(), b"hello dx");
}

// ── 2. 错误路径：错误沿 do_! 链上抛，后续语句不执行 ───────────────────

#[test]
fn error_path_propagates_without_panic() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no_such.txt");

    // 不带 create 打开不存在的文件：Open 在链首失败 → 整链 Err，后续语句短路。
    let blueprint = do_! {
        let fd = dx::open(&missing, OpenFlags {
            read: true,
            ..Default::default()
        });
        dx::write(&fd, b"x".to_vec());
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    let err = rt().run_blocking(blueprint).unwrap_err();
    assert!(matches!(err, SysError::NotFound));
    // 失败语句之后的操作（Write/Read/Close）未执行：无残留副作用可断言——
    // 链在 Open 处即返回，不会进入后续节点。
}

#[test]
fn error_mid_chain_skips_tail() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.txt");
    let missing = dir.path().join("no_such.txt");

    // 链中部失败（Stat 不存在路径）：尾表达式不会被求值（链整体 Err）。
    let blueprint = do_! {
        let f1 = dx::open(&good, open_rw_create());
        dx::write(&f1, b"data".to_vec());
        let _meta = dx::stat(&missing);
        dx::close(&f1);
        Value::U64(1)
    };
    let err = rt().run_blocking(blueprint).unwrap_err();
    assert!(matches!(err, SysError::NotFound));
}

// ── 3. 组合共存：plan! ⇄ do! ─────────────────────────────────────────

#[test]
fn plan_wraps_do_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    let file = sub.join("a.txt");

    // 两个命令式 do_! 阶段先构造成 Action 值，再作为元素交给 plan! 骨架——
    // 注意：plan! 的 continuation 闭包非 move，直接内嵌会借用外部路径；
    // 预构建后 plan! 只做值组合，无借用问题（蓝图即值、再组合）。
    let mkdir_act = do_! {
        dx::mkdir(&sub, 0o755);
        Value::Unit
    };
    let write_act = do_! {
        let f = dx::open(file.clone(), open_rw_create());
        dx::write(&f, b"hi".to_vec());
        dx::close(&f);
        Value::Unit
    };
    let blueprint: Action = plan! {
        mkdir_act;
        write_act;
    };

    rt().run_blocking(blueprint).unwrap();
    assert!(file.exists(), "do_! 阶段应真实落盘");
    assert_eq!(std::fs::read(&file).unwrap(), b"hi");
}

#[test]
fn do_block_embeds_plan_and_choose() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c.txt");
    let use_hi = true;

    // do_! 骨架内嵌 plan!（声明式子步骤）与 choose!（条件分支）——
    // do_! 语句槽接受任何返回 Action 的表达式。
    let blueprint = do_! {
        let f = dx::open(&file, open_rw_create());
        choose!(
            use_hi,
            then: plan! {
                dx::write(&f, b"hi".to_vec());
            },
            else: dx::write(&f, b"bye".to_vec()),
        );
        dx::close(&f);
        Value::Unit
    };

    rt().run_blocking(blueprint).unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"hi",
        "choose! then 分支应生效"
    );
}

// ── 4. 资源自动推导（infer_usage 模式表）与显式覆盖 ───────────────────

#[test]
fn infer_usage_table() {
    use AccessMode::{Append, Own, Read, Write};

    // 文件
    let open_w = dx::infer_usage(&DataOp::Open {
        path: "/a".into(),
        flags: OpenFlags {
            write: true,
            ..Default::default()
        },
    });
    assert_eq!(open_w, vec![usage(p("/a"), Write)]);
    let open_a = dx::infer_usage(&DataOp::Open {
        path: "/a".into(),
        flags: OpenFlags {
            append: true,
            ..Default::default()
        },
    });
    assert_eq!(open_a, vec![usage(p("/a"), Append)]);
    let open_r = dx::infer_usage(&DataOp::Open {
        path: "/a".into(),
        flags: OpenFlags {
            read: true,
            ..Default::default()
        },
    });
    assert_eq!(open_r, vec![usage(p("/a"), Read)]);

    assert_eq!(
        dx::infer_usage(&DataOp::Read { fd: 3, len: 8 }),
        vec![usage(f(3), Read)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Write {
            fd: 3,
            data: vec![]
        }),
        vec![usage(f(3), Write)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Close { fd: 3 }),
        vec![usage(f(3), Own)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Seek {
            fd: 3,
            offset: 0,
            whence: std::io::SeekFrom::Start(0),
        }),
        vec![usage(f(3), Read)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Stat { path: "/a".into() }),
        vec![usage(p("/a"), Read)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Unlink { path: "/a".into() }),
        vec![usage(p("/a"), Own)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Rename {
            from: "/a".into(),
            to: "/b".into(),
        }),
        vec![usage(p("/a"), Write), usage(p("/b"), Write)]
    );

    // 目录
    assert_eq!(
        dx::infer_usage(&DataOp::Mkdir {
            path: "/d".into(),
            mode: 0o755
        }),
        vec![usage(p("/d"), Write)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::ReadDir { path: "/d".into() }),
        vec![usage(p("/d"), Read)]
    );

    // 网络 / 管道 / 时间：运行时才分配句柄 → 空集（pdr.md §18 用户责任域）
    assert_eq!(
        dx::infer_usage(&DataOp::TcpBind {
            addr: "127.0.0.1:8080".parse().unwrap()
        }),
        vec![]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::TcpConnect {
            addr: "127.0.0.1:8080".parse().unwrap()
        }),
        vec![]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::PipeOpen {
            flags: PipeFlags::default()
        }),
        vec![]
    );
    assert_eq!(dx::infer_usage(&DataOp::GetTime), vec![]);

    // 进程
    assert_eq!(
        dx::infer_usage(&DataOp::Kill { pid: 42, signal: 9 }),
        vec![usage(Resource::Pid(42), Write)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Wait { pid: 42 }),
        vec![usage(Resource::Pid(42), Own)]
    );

    // 同步（R7 修复：锁仲裁在 executor 层 arbiter（D16/R-1），与 A4 线性域
    // 正交——空声明保证 lock→unlock→再 lock 可重入，对齐 adversarial_r2 2b）
    assert_eq!(dx::infer_usage(&DataOp::MutexLock { id: 7 }), vec![]);
    assert_eq!(dx::infer_usage(&DataOp::MutexUnlock { id: 7 }), vec![]);

    // 信号（无仲裁层：二次发送允许，A4 不消费 Signal/Pid）
    assert_eq!(
        dx::infer_usage(&DataOp::SendSignal { signal: 9, pid: 42 }),
        vec![]
    );

    // 内存
    assert_eq!(
        dx::infer_usage(&DataOp::Mmap {
            path: "/a".into(),
            len: 10,
            prot: MmapProt {
                write: true,
                ..Default::default()
            },
        }),
        vec![usage(p("/a"), Write)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Munmap { addr: 0, len: 10 }),
        vec![]
    );

    // 其他
    assert_eq!(
        dx::infer_usage(&DataOp::SendFile {
            out: 1,
            input: 2,
            offset: 0,
            len: 5
        }),
        vec![usage(f(1), Write), usage(f(2), Read)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Dup { fd: 3 }),
        vec![usage(f(3), Write)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::Dup2 {
            old_fd: 3,
            new_fd: 4
        }),
        vec![usage(f(4), Write), usage(f(3), Read)]
    );
}

#[test]
fn inferred_resources_flow_through_chain() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chain.txt");

    // 链首 Open 的资源由 flags 推导（Write(path)）；
    // 其后 Write(fd)/Close(fd) 的 fd 声明随运行时绑定值进入链节点。
    let blueprint = do_! {
        let fd = dx::open(&path, open_rw_create());
        dx::write(&fd, b"x".to_vec());
        dx::close(&fd);
        Value::Unit
    };

    let Action::Sequential { current, next } = blueprint else {
        panic!("do_! 应展开为 Sequential 链");
    };
    let Action::Syscall { op, resources, .. } = *current else {
        panic!("链首应为 Syscall(Open)");
    };
    assert!(matches!(op, DataOp::Open { .. }));
    assert_eq!(
        resources,
        vec![usage(
            Resource::Path(path.to_string_lossy().into_owned()),
            AccessMode::Write
        )]
    );

    // 第 2 步：Write，fd 声明 = 运行绑定的 fd 值（CPS 值流贯穿）。
    let Action::Sequential { current, next } = next(Value::Fd(9)) else {
        panic!("第 2 步应为 Sequential");
    };
    let Action::Syscall { op, resources, .. } = *current else {
        panic!("第 2 步应为 Syscall(Write)");
    };
    assert!(matches!(op, DataOp::Write { .. }));
    assert_eq!(resources, vec![usage(f(9), AccessMode::Write)]);

    // 第 3 步：Close，Own(fd) 终结。
    let Action::Sequential { current, .. } = next(Value::Unit) else {
        panic!("第 3 步应为 Sequential");
    };
    let Action::Syscall { op, resources, .. } = *current else {
        panic!("第 3 步应为 Syscall(Close)");
    };
    assert!(matches!(op, DataOp::Close { .. }));
    assert_eq!(resources, vec![usage(f(9), AccessMode::Own)]);
}

#[test]
fn explicit_override_wins_over_inference() {
    // syscall_with：显式 ResourceSet 覆盖 infer_usage 的默认推导
    // （Open 本应推导 Write(path)，现改声明 Read(custom)）。
    let custom = vec![usage(p("/custom"), AccessMode::Read)];
    let a = dx::syscall_with(
        DataOp::Open {
            path: "/x".into(),
            flags: OpenFlags {
                write: true,
                ..Default::default()
            },
        },
        custom.clone(),
    );
    let Action::Syscall { op, resources, .. } = &a else {
        panic!("syscall_with 应构造 Syscall 节点");
    };
    assert!(matches!(op, DataOp::Open { .. }));
    assert_eq!(resources, &custom, "显式声明应完全覆盖自动推导");
}

// ── 5. 边界：空块收敛 Pure(Unit)；let _ 丢弃语句 ─────────────────────

#[test]
fn empty_block_and_discard_statement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.txt");

    // 空 do_! 块 → Pure(Unit)。
    let empty: Action = do_! {};
    assert!(matches!(empty, Action::Pure(Value::Unit)));

    // let _ / 表达式语句：执行并丢弃中间值。
    let blueprint = do_! {
        let _ = dx::open(&path, open_rw_create());
        let fd = dx::open(&path, OpenFlags {
            read: true,
            ..Default::default()
        });
        let _data = dx::read(&fd, 64);
        dx::close(&fd);
        Value::Unit
    };
    assert!(matches!(rt().run_blocking(blueprint).unwrap(), Value::Unit));
}

// ── 6. 迭代 2：文件全操作 do_! 覆盖 / TCP 骨架 / catch / plan!+fork! 混合 ──

#[test]
fn file_ops_comprehensive_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("data");
    let file = sub.join("payload.bin");

    // 一个 do_! 块覆盖文件面常用操作：Mkdir/Open/Write/Seek/Read/Stat/Close/
    // Unlink。尾表达式组合两个链内值（Value::List），全链真实执行。
    // 注：file 在链内多处使用，clone 后 move 进闭包（'static），外部保留原件断言。
    let fc = file.clone();
    let blueprint = do_! {
        dx::mkdir(&sub, 0o755);
        let fd = dx::open(fc.clone(), open_rw_create());
        dx::write(&fd, b"dx roundtrip".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        let meta = dx::stat(fc.clone());
        dx::close(&fd);
        dx::unlink(fc.clone());
        Value::List(vec![data, meta])
    };

    let v = rt().run_blocking(blueprint).unwrap();
    match v {
        Value::List(items) => {
            assert_eq!(items[0], Value::Bytes(b"dx roundtrip".to_vec()), "读回内容");
            assert_eq!(
                items[1],
                Value::List(vec![Value::U64(12), Value::Bool(false), Value::Bool(true)]),
                "stat → List([len, is_dir, is_file])"
            );
        }
        other => panic!("期望 List，得到 {other:?}"),
    }
    assert!(!file.exists(), "unlink 后文件应已删除");
    assert!(sub.exists(), "unlink 只删文件，目录仍在");
}

#[test]
fn tcp_bind_accept_skeleton() {
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // 骨架：bind → accept → close（不真实 accept——需并发客户端，见 docs）。
    let blueprint = do_! {
        let listener = dx::open_tcp(addr);
        let _conn = dx::accept(&listener);
        dx::close(&listener);
        Value::Unit
    };

    // 结构断言：链首 Syscall(TcpBind)，资源空集（运行时句柄，pdr.md §18）。
    let Action::Sequential { current, next } = blueprint else {
        panic!("do_! 应展开为 Sequential 链");
    };
    let Action::Syscall { op, resources, .. } = *current else {
        panic!("链首应为 Syscall(TcpBind)");
    };
    assert!(matches!(op, DataOp::TcpBind { .. }));
    assert_eq!(resources, vec![], "TcpBind 新句柄静态不可声明 → 空集");

    // 第 2 步：accept，listener 值（Fd）经 CPS 值流贯穿。
    let Action::Sequential { current, .. } = next(Value::Fd(0)) else {
        panic!("第 2 步应为 Sequential");
    };
    let Action::Syscall { op, .. } = *current else {
        panic!("第 2 步应为 Syscall(TcpAccept)");
    };
    assert!(matches!(op, DataOp::TcpAccept { listener: 0 }));

    // 真实执行（仅 bind → close：127.0.0.1:0 由内核分配端口，不阻塞）。
    let v = rt()
        .run_blocking(do_! {
            let listener = dx::open_tcp(addr);
            dx::close(&listener);
            Value::Unit
        })
        .unwrap();
    assert_eq!(v, Value::Unit);
}

#[test]
fn catch_handles_error_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no_such.txt");
    let fallback = dir.path().join("fallback.txt");
    let fb = fallback.clone(); // handler 需 'static：clone 后 move 进闭包

    // do_! 内 dx::catch：Open 缺失文件失败 → handler 收到真实 SysError，
    // 返回替代 Action（打开可创建文件）→ 链继续，fd 绑定 = handler 的结果值。
    let blueprint = do_! {
        let fd = dx::catch(
            dx::open(&missing, OpenFlags {
                read: true,
                ..Default::default()
            }),
            move |e| {
                assert!(matches!(e, SysError::NotFound), "handler 应收到 NotFound");
                dx::open(fb.clone(), open_rw_create())
            },
        );
        dx::write(&fd, b"recovered".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data
    };

    let v = rt().run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"recovered".to_vec()));
    assert_eq!(
        std::fs::read(&fallback).unwrap(),
        b"recovered",
        "handler 的替代路径真实落盘"
    );
}

#[test]
fn catch_success_passthrough_skips_handler() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.txt");
    let ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_c = ran.clone();

    // 成功路径：值原样贯穿、handler 不执行。若 handler 误执行会返回 Unit，
    // 后续 dx::close(&fd) 的 expect_fd 立即 panic——结构上证明未走 handler。
    let blueprint = do_! {
        let fd = dx::catch(
            dx::open(&good, open_rw_create()),
            move |_e| {
                ran_c.store(true, std::sync::atomic::Ordering::SeqCst);
                dx::unit()
            },
        );
        dx::write(&fd, b"ok".to_vec());
        dx::close(&fd);
        Value::Unit
    };

    rt().run_blocking(blueprint).unwrap();
    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "成功路径 handler 不得执行"
    );
    assert_eq!(std::fs::read(&good).unwrap(), b"ok");
}

#[test]
fn do_plan_fork_mix() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("mix");
    let fa = sub.join("a.txt");
    let fb = sub.join("b.txt");

    // plan!/fork! 内嵌 do_! 均预构建 Action 值（do_! 展开闭包要求 'static、
    // plan!/fork! 只做值组合），与迭代 1 plan_wraps_do_blocks 同模式。
    let mkdir_act = do_! { dx::mkdir(&sub, 0o755); Value::Unit };
    let left_act = do_! {
        let f = dx::open(fa.clone(), open_rw_create());
        dx::write(&f, b"A".to_vec());
        dx::close(&f);
        Value::Unit
    };
    let right_act = do_! {
        let f = dx::open(fb.clone(), open_rw_create());
        dx::write(&f, b"B".to_vec());
        dx::close(&f);
        Value::Unit
    };
    let stat_act = do_! {
        let _ = dx::stat(&sub);
        Value::Unit
    };

    // do_!（命令式骨架）内嵌 plan!（声明式子步骤）与 fork!（并发分叉，
    // 左右分支写不同文件 → 无资源冲突）——三者混合，真实执行。
    let blueprint = do_! {
        plan! { mkdir_act };
        fork! {
            left: left_act,
            right: right_act,
        };
        stat_act;
        Value::U64(42)
    };

    let v = rt().run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::U64(42));
    assert_eq!(std::fs::read(&fa).unwrap(), b"A");
    assert_eq!(std::fs::read(&fb).unwrap(), b"B");
}

#[test]
fn remaining_op_wrappers_construct_syscall_nodes() {
    // munmap / send_file / dup2：补齐 DataOp → dx 包装全量映射，
    // 资源自动推导沿用 infer_usage 表（显式断言）。
    let m = dx::munmap(0x1000, 4096);
    let Action::Syscall { op, resources, .. } = &m else {
        panic!("munmap 应构造 Syscall 节点");
    };
    assert!(matches!(
        op,
        DataOp::Munmap {
            addr: 0x1000,
            len: 4096
        }
    ));
    assert_eq!(resources, &vec![]);

    let s = dx::send_file(&Value::Fd(1), &Value::Fd(2), 0, 5);
    let Action::Syscall { op, resources, .. } = &s else {
        panic!("send_file 应构造 Syscall 节点");
    };
    assert!(matches!(
        op,
        DataOp::SendFile {
            out: 1,
            input: 2,
            offset: 0,
            len: 5
        }
    ));
    assert_eq!(
        resources,
        &vec![
            usage(f(1), AccessMode::Write),
            usage(f(2), AccessMode::Read)
        ]
    );

    let d = dx::dup2(&Value::Fd(3), 4);
    let Action::Syscall { op, resources, .. } = &d else {
        panic!("dup2 应构造 Syscall 节点");
    };
    assert!(matches!(
        op,
        DataOp::Dup2 {
            old_fd: 3,
            new_fd: 4
        }
    ));
    assert_eq!(
        resources,
        &vec![
            usage(f(4), AccessMode::Write),
            usage(f(3), AccessMode::Read)
        ]
    );
}

/// 审计 R4-F1 + R7 修复：TcpShutdown 推导为**空集**——(a) Write 声明与
/// tcp_write 消费冲突（A4 至多一次）→ 误拒 write→shutdown 合法链；(b) Own
/// 声明是终结语义 → 拒绝后续 close（shutdown→close 标准链被误拒，R7 审计）。
/// shutdown 为半关闭、不终结 fd，无 A4 消费语义——空声明，物理层执行。
#[test]
fn tcp_shutdown_declares_empty_not_own() {
    let a = dx::tcp_shutdown(&Value::Fd(9), std::net::Shutdown::Write);
    let Action::Syscall { resources, .. } = &a else {
        panic!("tcp_shutdown 应构造 Syscall 节点");
    };
    assert_eq!(
        resources,
        &vec![],
        "TcpShutdown 应声明空集（非 Own 非 Write——半关闭不终结 fd）"
    );
}

/// 审计 R4-F2 修复：锁循环 lock→unlock→lock 同 id 可重入（Write 声明下
/// 第二次 lock 被 A4 误拒；Read 声明无消费语义）。
#[test]
fn mutex_lock_cycle_reentrant_via_dx() {
    let mut r = rt();
    // lock → unlock → lock 同 id（Read 声明：无 A4 消费）
    let blueprint = Action::Sequential {
        current: Box::new(dx::mutex_lock(5)),
        next: Box::new(|_| Action::Sequential {
            current: Box::new(dx::mutex_unlock(5)),
            next: Box::new(|_| dx::mutex_lock(5)),
        }),
    };
    let v = r.run_blocking(blueprint).unwrap();
    assert_eq!(
        v,
        Value::Unit,
        "锁循环可重入（修复前第二次 lock InvalidInput）"
    );
    // 清理：解锁 + 无残留
    r.run_blocking(Action::Sequential {
        current: Box::new(dx::mutex_unlock(5)),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    })
    .unwrap();
}

// ── 分隔：以上 A3 迭代 2 测试段，以下 dxfix R7 回归段 ──

// ── 6. R7 DX 修复回归：锁可重入 + 信号可重复 ─────────────────────────

#[test]
fn mutex_lock_unlock_relock_succeeds() {
    // R7 发现：infer_usage 把 MutexLock 推导为 Write(Fd(id))，A4 每资源至多
    // 消费一次（arbiter release 只清占坑不清 A4 consumed）→ lock→unlock→再
    // lock 第二次必被 A4 拒 InvalidInput。修复后空声明：仲裁在 executor 层
    // arbiter（unlock 释放占坑）→ 顺序重入成功。
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let blueprint = do_! {
        dx::mutex_lock(7);
        dx::mutex_unlock(7);
        dx::mutex_lock(7);
        dx::mutex_unlock(7);
        Value::Unit
    };
    // 修复前：第二次 mutex_lock 在此抛 InvalidInput。
    rt.run_blocking(blueprint).unwrap();

    // 两把锁的 undo 入栈（显式 unlock 已物理释放，undo 为幂等 no-op）
    assert_eq!(rt.undo_stack().len(), 2);
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert!(rt.undo_stack().is_empty());

    // 同 id 再次重入仍然成立（undo 释放不破坏可重入性，对齐 r2 2a 断言）。
    rt.run_blocking(dx::mutex_lock(7)).unwrap();
    rt.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
}

#[test]
fn send_signal_repeatable_no_a4_rejection() {
    // R7 评估：SendSignal 原推导 Write(Signal)+Write(Pid(pid)) → Signal 全局
    // 资源至多一次 → 二次发送被 A4 拒 InvalidInput。修复后空声明：executor
    // 层对 Signal 无仲裁、二次发送语义上允许（SIGTERM→SIGKILL 优雅停机），
    // 成败由物理层决定，绝不由 A4 线性域拒绝。
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // 长存活子进程（平台差异：Unix sh -c sleep；Windows cmd timeout）。
    #[cfg(windows)]
    let cmd = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/C", "timeout", "60"]);
        c
    };
    #[cfg(not(windows))]
    let cmd = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "sleep 60"]);
        c
    };
    let pid = rt.run_blocking(dx::spawn(cmd)).unwrap();
    assert!(matches!(pid, Value::Pid(_)));

    // 第一次 SIGKILL（9）：路由 op_kill → 物理杀进程。
    rt.run_blocking(dx::send_signal(9, &pid)).unwrap();

    // 第二次 SIGKILL：修复前在 A4 层被拒（InvalidInput，Signal/Pid 已消费）；
    // 修复后进入物理层（已终止子进程 start_kill 幂等成功，或平台层错误），
    // 但绝不可能是 A4 的 InvalidInput。
    let second = rt.run_blocking(dx::send_signal(9, &pid));
    match second {
        Ok(_) => {}
        Err(e) => assert!(
            !matches!(e, SysError::InvalidInput),
            "二次 SendSignal 不应被 A4 拒绝（InvalidInput），实测 {e:?}"
        ),
    }

    // 收割子进程（退出码 1 = 信号终止，平台差异不断言具体值）。
    let v = rt.run_blocking(dx::wait(&pid)).unwrap();
    assert!(matches!(v, Value::U64(_)));
}
