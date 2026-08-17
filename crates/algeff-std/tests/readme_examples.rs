//! README 示例编译验证（审查 HIGH-1/HIGH-2 修复的永久护栏）：
//! 本文件是 README 示例的手工镜像（非自动抽取）：README 示例改动后
//! 若不同步本文件，CI 编译/运行即失败——防止 API 漂移的护栏。
//! 注意：README 片段无 fn main 包裹（本文件为可运行测试）；
//! 镜像覆盖 = 示例体逐字，测试包裹（临时目录/防挂死 Timeout）为测试侧变体。
//!
//! 与 README 的对应关系：
//! - `readme_file_io_roundtrip_do` ⇔ README §3（do_! 版，推荐写法）
//! - `readme_file_io_cps_legacy`   ⇔ README §3 折叠对比块（手写 CPS 链旧写法）
//! - `readme_explicit_resources`   ⇔ README §4（资源声明：自动推导与显式覆盖）
//! - `readme_patterns`             ⇔ README 常用模式速查（plan!/fork!/Catch/Timeout/Replace）
//! - `readme_do_error_handling`    ⇔ README 常用模式速查（do_! 错误短路上抛 + Catch 恢复）
//! - `readme_tcp_bind`             ⇔ README TCP echo 骨架（do_! 版 Bind+Accept 一次）

use std::path::PathBuf;
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_macro::{do_, fork, plan};
use algeff_std::{dx, TokioExecutor};

// ── 资源声明辅助（README §3 折叠对比块：旧写法）────────────────────

fn write_path(p: &PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(p.clone())).into_usage()
}
fn write_fd(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn read_fd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}

// ── §3：do_! 版（与 README 代码逐字一致）────────────────────────

#[test]
fn readme_file_io_roundtrip_do() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let dir = std::env::temp_dir().join(format!("algeff-readme-do-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hello.txt");
    let flags = OpenFlags {
        read: true,
        write: true,
        create: true,
        ..Default::default()
    };

    // 语法就是普通 Rust：open/write/seek/read/close 直书，
    // fd 经 let 绑定贯穿，资源声明由 dx 按操作自动推导
    let blueprint = do_! {
        let fd = dx::open(&path, flags);
        dx::write(&fd, b"hello algeff".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data // 尾表达式 = 链的最终值
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"hello algeff".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── §3 折叠对比块：手写 CPS 链旧写法（与 README 代码逐字一致）────

#[test]
fn readme_file_io_cps_legacy() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let dir = std::env::temp_dir().join(format!("algeff-readme-cps-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hello.txt");

    // Open（声明"要写这个路径"）→ 拿到 fd → Write → Seek → Read
    // 注意：每个 next 闭包都要加 move（NextFn 是 'static 的）
    let blueprint = Action::Syscall {
        op: DataOp::Open {
            path: path.clone(),
            flags: OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            },
        },
        resources: vec![write_path(&path)], // 类型安全资源声明
        next: Box::new(move |v| {
            let fd = match v {
                Value::Fd(fd) => fd,
                other => panic!("期望 Fd，得到 {other:?}"),
            };
            Action::Sequential {
                current: Box::new(Action::Syscall {
                    op: DataOp::Write {
                        fd,
                        data: b"hello algeff".to_vec(),
                    },
                    resources: vec![write_fd(fd)],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                next: Box::new(move |_| Action::Sequential {
                    current: Box::new(Action::Syscall {
                        op: DataOp::Seek {
                            fd,
                            offset: 0,
                            whence: std::io::SeekFrom::Start(0),
                        },
                        resources: vec![read_fd(fd)],
                        next: Box::new(|_| Action::Pure(Value::Unit)),
                    }),
                    next: Box::new(move |_| Action::Syscall {
                        op: DataOp::Read { fd, len: 64 },
                        resources: vec![read_fd(fd)],
                        // 最后一个操作把结果透传出去（next 收到 Read 的 Bytes）
                        next: Box::new(Action::Pure),
                    }),
                }),
            }
        }),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"hello algeff".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── §4：资源声明自动推导与显式覆盖（与 README 代码逐字一致）──────

#[test]
fn readme_explicit_resources() {
    use algeff_core::prelude::*;
    use algeff_core::OpenFlags;
    use algeff_std::dx;

    // 自动推导：write 模式 → Write(path)
    let auto = dx::open(
        "hello.txt",
        OpenFlags {
            write: true,
            ..Default::default()
        },
    );

    // 显式覆盖：自定义资源声明完全替换默认推导（syscall_with 优先于 infer_usage）
    let custom = dx::syscall_with(
        DataOp::Open {
            path: "hello.txt".into(),
            flags: OpenFlags {
                write: true,
                ..Default::default()
            },
        },
        vec![ResourceUsage {
            resource: Resource::Path("/custom".into()),
            mode: AccessMode::Read,
        }],
    );

    assert!(matches!(auto, Action::Syscall { .. }));
    assert!(matches!(custom, Action::Syscall { .. }));
}

// ── 常用模式速查：plan! / fork! / Catch / Timeout / Replace ──

#[test]
fn readme_patterns() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // plan!（README §2 与速查）
    let p: Action = plan! {
        Action::Pure(Value::U64(1));
        Action::Pure(Value::U64(2));
    };
    assert!(matches!(rt.run_blocking(p).unwrap(), Value::Unit));

    // fork!（README 速查：left:/right: 标签）
    let f: Action = fork! {
        left: Action::Pure(Value::U64(10)),
        right: Action::Pure(Value::U64(20)),
    };
    assert!(matches!(rt.run_blocking(f).unwrap(), Value::Unit));

    // 自定义合并 → 手写 Action::Fork（README 注记）
    let f2 = Action::Fork {
        left: Box::new(Action::Pure(Value::U64(10))),
        right: Box::new(Action::Pure(Value::U64(20))),
        combine: Box::new(|l, r| Action::Pure(Value::List(vec![l, r]))),
    };
    assert!(matches!(rt.run_blocking(f2).unwrap(), Value::List(_)));

    // Catch（README 速查）
    let c = Action::Catch {
        action: Box::new(Action::Pure(Value::U64(1))),
        handler: Box::new(|err| match err {
            SysError::NotFound => Action::Pure(Value::U64(0)),
            _ => Action::Pure(Value::U64(1)),
        }),
    };
    assert_eq!(rt.run_blocking(c).unwrap(), Value::U64(1));

    // Timeout（README 速查）
    let t = Action::Timeout {
        action: Box::new(Action::Sleep {
            duration: Duration::from_millis(5),
            next: Box::new(|_| Action::Pure(Value::U64(7))),
        }),
        duration: Duration::from_millis(100),
        on_timeout: Box::new(Action::Pure(Value::U64(0))),
    };
    assert_eq!(rt.run_blocking(t).unwrap(), Value::U64(7));

    // Replace（README 速查）
    let r = Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    };
    assert_eq!(rt.run_blocking(r).unwrap(), Value::Unit);
}

// ── 常用模式速查：do_! 错误短路上抛 + Catch 恢复 ────────────────

#[test]
fn readme_do_error_handling() {
    use algeff_core::prelude::*;
    use algeff_core::OpenFlags;
    use algeff_macro::do_;
    use algeff_std::dx;

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    let blueprint = do_! {
        let fd = dx::open("no_such.txt", OpenFlags { read: true, ..Default::default() });
        dx::write(&fd, b"x".to_vec());
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data // 打开失败时链首即 Err，不会执行到这里
    };

    let result = rt.run_blocking(blueprint);
    assert!(matches!(result, Err(SysError::NotFound)));

    // 需要恢复：Catch 包住 do_! 链，NotFound 走 handler 返回 0
    let guarded = Action::Catch {
        action: Box::new(do_! {
            let fd = dx::open("no_such.txt", OpenFlags { read: true, ..Default::default() });
            let data = dx::read(&fd, 64);
            data
        }),
        handler: Box::new(|err| match err {
            SysError::NotFound => Action::Pure(Value::U64(0)),
            _ => Action::Pure(Value::U64(1)),
        }),
    };
    assert_eq!(rt.run_blocking(guarded).unwrap(), Value::U64(0));
}

// ── TCP 骨架：do_! 版 Bind → Accept 一次 ───────────────────────

#[test]
fn readme_tcp_bind() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // 绑定监听 → accept 一个连接（新句柄运行时分配，资源自动推导为空集）
    let blueprint = do_! {
        let listener = dx::open_tcp(addr);
        let _conn = dx::accept(&listener);
        Value::Unit
    };

    // 仅验证构建可执行（无客户端连接时 Accept 会阻塞——用 Timeout 包裹避免挂死）
    let bounded = Action::Timeout {
        action: Box::new(blueprint),
        duration: Duration::from_millis(50),
        on_timeout: Box::new(Action::Pure(Value::Unit)),
    };
    assert_eq!(rt.run_blocking(bounded).unwrap(), Value::Unit);
}

// ── 痛点对比章节（README「它解决什么：传统 IO 的四个痛点」）──────

#[test]
fn readme_painpoints_atomic_replay_undo_compose() {
    use algeff_core::prelude::*;
    use algeff_core::OpenFlags;
    use algeff_macro::do_;
    use algeff_std::dx;

    let dir = std::env::temp_dir().join(format!("algeff-readme-pain-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("atomic.txt");

    // 痛点 1：原子写入（read: true 是撤销前提，只写句柄无法构造逆 → 报错）
    std::fs::write(&path, "before").unwrap();
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    rt.run_blocking(do_! {
        let fd = dx::open(&path, OpenFlags {
            read: true,
            write: true,
            create: true,
            ..Default::default()
        });
        dx::write(&fd, b"new content".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "new content",
        "痛点 1：写入生效（可撤销前提 read:true）"
    );

    // 痛点 2：可重放——同一蓝图结构 3 次，结果一致（每轮 truncate 打开）
    for i in 0..3u64 {
        let mut rt2 = Runtime::new(Box::new(TokioExecutor::new()));
        // 闭包内引用 path → 每轮 clone owned 值（do_! 生成 'static 闭包）
        let p2 = path.clone();
        let v = rt2
            .run_blocking(do_! {
                let fd = dx::open(&p2, OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    truncate: true,
                    ..Default::default()
                });
                dx::write(&fd, format!("round {i}").into_bytes());
                dx::close(&fd);
                let fd2 = dx::open(&p2, OpenFlags { read: true, ..Default::default() });
                let data = dx::read(&fd2, 64);
                dx::close(&fd2);
                data
            })
            .unwrap();
        assert_eq!(
            v,
            Value::Bytes(format!("round {i}").into_bytes()),
            "痛点 2 第 {i} 次重放"
        );
    }

    // 痛点 3：Replace 一键撤销（独立 Runtime → 撤销栈只含本段副作用）
    std::fs::write(&path, "original").unwrap();
    let mut rt3 = Runtime::new(Box::new(TokioExecutor::new()));
    rt3.run_blocking(do_! {
        let fd = dx::open(&path, OpenFlags { read: true, write: true, ..Default::default() });
        dx::write(&fd, b"temporary".to_vec());
        dx::close(&fd);
        Value::Unit
    })
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "temporary");
    rt3.run_blocking(Action::Replace {
        target: Box::new(Action::Pure(Value::Unit)),
    })
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "original",
        "痛点 3：Replace 一键回滚到 open 前状态"
    );

    // 痛点 4：组合性——小蓝图拼大蓝图一次执行
    let mut rt4 = Runtime::new(Box::new(TokioExecutor::new()));
    let mut steps: Vec<Action> = Vec::new();
    for i in 0..3 {
        let p = dir.join(format!("file_{i}.txt"));
        steps.push(do_! {
            let fd = dx::open(&p, OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            });
            dx::write(&fd, format!("content {i}").into_bytes());
            dx::close(&fd);
            Value::Unit
        });
    }
    let combined = steps
        .into_iter()
        .reduce(|acc, s| Action::Sequential {
            current: Box::new(acc),
            next: Box::new(move |_| s),
        })
        .unwrap();
    rt4.run_blocking(combined).unwrap();
    for i in 0..3 {
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("file_{i}.txt"))).unwrap(),
            format!("content {i}"),
            "痛点 4：file_{i}.txt 写入生效"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 痛点 5：重试安全——幂等键去重（README「痛点 5」镜像）。
#[test]
fn readme_painpoints_idempotency_retry() {
    use algeff_core::prelude::*;
    use algeff_core::OpenFlags;
    use algeff_macro::do_;
    use algeff_std::dx;

    let dir = std::env::temp_dir().join(format!("algeff-readme-idem-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("charge.txt");
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // 带幂等键的副作用段（如扣款/发邮件/建表）：同 key 只真正执行一次。
    // do_! 生成 'static 闭包：path clone 进闭包；Action 不可 Clone，重试重新构造。
    let make_charge = || {
        let p = path.clone();
        dx::idempotent(
            "charge:order-42",
            do_! {
                let fd = dx::open(&p, OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                });
                dx::write(&fd, b"charged".to_vec());
                dx::close(&fd);
                Value::U64(42)
            },
        )
    };

    // 重试 3 次：只有第一次真正执行（键 COMMITTED → 后续返回缓存）。
    for _ in 0..3 {
        let v = rt.run_blocking(make_charge()).unwrap();
        assert_eq!(v, Value::U64(42), "重试返回缓存结果（副作用未重复）");
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "charged",
        "文件只写了一次（非幂等效应未重复执行）"
    );
    // undo 栈只含第一次执行的副作用记录：open(create 新建→unlink 逆) + write 逆
    // + REVERT 标记 = 3（后两次缓存命中不压 undo）。
    assert_eq!(
        rt.undo_stack().len(),
        3,
        "重试不产生新 undo（从未真正'新执行'）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
