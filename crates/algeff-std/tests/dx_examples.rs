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

use algeff_core::prelude::*;
use algeff_core::{AccessMode, MmapProt, OpenFlags, PipeFlags, Resource, ResourceUsage, SysError};
use algeff_macro::{choose, do_, plan};
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

    // 同步（对齐 adversarial_r2 安全声明模式：unlock 降为 Read）
    assert_eq!(
        dx::infer_usage(&DataOp::MutexLock { id: 7 }),
        vec![usage(f(7), Write)]
    );
    assert_eq!(
        dx::infer_usage(&DataOp::MutexUnlock { id: 7 }),
        vec![usage(f(7), Read)]
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
