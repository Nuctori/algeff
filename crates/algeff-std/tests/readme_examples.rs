//! README 示例编译验证（审查 HIGH-1/HIGH-2 修复的永久护栏）：
//! 本文件是 README 示例的手工镜像（非自动抽取）：README 示例改动后
//! 若不同步本文件，CI 编译/运行即失败——防止 API 漂移的护栏。
//! 注意：README §4 片段无 fn main 包裹（本文件为可运行测试）；
//! 镜像覆盖 = 示例体逐字，测试包裹（临时目录/防挂死 Timeout）为测试侧变体。
//!
//! 与 README 的对应关系：
//! - `readme_file_io_roundtrip` ⇔ README §3（手写 Syscall 链）
//! - `readme_adapters_chain`   ⇔ README §4（adapters 版）
//! - `readme_patterns`         ⇔ README 常用模式速查（fork!/Catch/Timeout/Replace）
//! - `readme_tcp_bind`         ⇔ README TCP echo 骨架（仅 Bind+Accept 一次）

use std::path::PathBuf;
use std::time::Duration;

use algeff_core::{
    Action, DataOp, OpenFlags, ReadOnly, ResourceInner, ResourceUsage, Runtime, SysError,
    TypedResource, Value, WriteOnly,
};
use algeff_macro::{fork, plan};
use algeff_std::TokioExecutor;

// ── 资源声明辅助（与 README §3 相同）────────────────────────────

fn write_path(p: &PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(p.clone())).into_usage()
}
fn write_fd(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn read_fd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}

// ── §3：手写 Syscall 链（与 README 代码逐字一致）────────────────

#[test]
fn readme_file_io_roundtrip() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let dir = std::env::temp_dir().join(format!("algeff-readme-{}", std::process::id()));
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
                        next: Box::new(|v| Action::Pure(v)),
                    }),
                }),
            }
        }),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"hello algeff".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
}

// ── §4：adapters 版链 ─────────────────────────────────────────

#[test]
fn readme_adapters_chain() {
    use algeff_std::adapters::{close, open_file, read, write};

    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let dir = std::env::temp_dir().join(format!("algeff-readme-adapters-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hello.txt");

    let blueprint = Action::Sequential {
        current: Box::new(open_file(
            path.clone(),
            OpenFlags {
                read: true,
                write: true,
                create: true,
                ..Default::default()
            },
        )),
        next: Box::new(move |v| {
            let fd = match v {
                Value::Fd(fd) => fd,
                other => panic!("{other:?}"),
            };
            Action::Sequential {
                current: Box::new(write(fd, b"hello".to_vec())),
                next: Box::new(move |_| Action::Sequential {
                    // 关闭后重开：新句柄从位置 0 读（适配器层无 Seek，见 §3 手写版）
                    current: Box::new(close(fd)),
                    next: Box::new(move |_| Action::Sequential {
                        current: Box::new(open_file(
                            path.clone(),
                            OpenFlags {
                                read: true,
                                write: false,
                                ..Default::default()
                            },
                        )),
                        next: Box::new(move |v| {
                            let fd2 = match v {
                                Value::Fd(fd) => fd,
                                other => panic!("{other:?}"),
                            };
                            Action::Sequential {
                                current: Box::new(read(fd2, 64)),
                                next: Box::new(move |v| Action::Sequential {
                                    current: Box::new(close(fd2)),
                                    next: Box::new(move |_| Action::Pure(v)),
                                }),
                            }
                        }),
                    }),
                }),
            }
        }),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    assert_eq!(v, Value::Bytes(b"hello".to_vec()));
    let _ = std::fs::remove_dir_all(&dir);
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

// ── TCP 骨架：Bind → Accept 一次 ─────────────────────────────

#[test]
fn readme_tcp_bind() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    let blueprint = Action::Syscall {
        op: DataOp::TcpBind { addr },
        resources: vec![],
        next: Box::new(move |v| match v {
            Value::Fd(listener) => {
                // accept → 循环 TcpRead/TcpWrite（分片到达需循环读，见 tests/e2e.rs）
                Action::Syscall {
                    op: DataOp::TcpAccept { listener },
                    resources: vec![],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }
            }
            other => panic!("期望 Fd，得到 {other:?}"),
        }),
    };

    // 仅验证构建可执行（无客户端连接时 Accept 会阻塞——用 Timeout 包裹避免挂死）
    let bounded = Action::Timeout {
        action: Box::new(blueprint),
        duration: Duration::from_millis(50),
        on_timeout: Box::new(Action::Pure(Value::Unit)),
    };
    assert_eq!(rt.run_blocking(bounded).unwrap(), Value::Unit);
}
