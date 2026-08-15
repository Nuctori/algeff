//! A5 批 2 端到端示例（pdr.md §14「完整实现」的落地证明）：
//!
//! `Runtime::new(Box::new(TokioExecutor::new()))` → 手写 CPS Action 链
//! Open(temp 文件, 读写) → Write → Seek(0) → Read → Close，
//! 断言数据往返一致并打印结果。
//!
//! 契约注意（D9）：Runtime 自持 tokio reactor，`Runtime::new` 与
//! `run_blocking` 都必须在 tokio 上下文之外调用——本例位于进程主线程，
//! 直接使用 core 提供的 `run_blocking`（无需 std::thread + block_on 包装）。
//!
//! 运行：`cargo run -p algeff-std --example file_roundtrip`

use std::path::PathBuf;

use algeff_core::{
    Action, DataOp, OpenFlags, Owned, ReadOnly, ResourceInner, ResourceUsage, Runtime,
    TypedResource, Value, WriteOnly,
};
use algeff_std::TokioExecutor;

// ── 本地辅助（adapters.rs 内部同名私有辅助的公开复制；src/ 冻结不可改）──

fn rd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
fn wr(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn ow(fd: u64) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}
fn wr_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(path)).into_usage()
}

/// 构造单个 Syscall 节点（next 为 CPS 延续）。
fn syscall(
    op: DataOp,
    resources: Vec<ResourceUsage>,
    next: impl FnOnce(Value) -> Action + 'static,
) -> Action {
    Action::Syscall {
        op,
        resources,
        next: Box::new(next),
    }
}

fn fd_of(v: &Value) -> u64 {
    match v {
        Value::Fd(fd) => *fd,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

fn main() {
    // temp 文件：pid 唯一避免并行运行冲突；truncate 保证干净初始状态。
    let path = std::env::temp_dir().join(format!("algeff_roundtrip_{}.txt", std::process::id()));
    let payload: Vec<u8> = b"Algeff E2E: Open->Write->Seek->Read->Close roundtrip payload".to_vec();
    let payload_len = payload.len();
    let expect = payload.clone();
    let flags = OpenFlags {
        read: true,
        write: true,
        create: true,
        truncate: true,
        ..Default::default()
    };

    // D9：Runtime::new 在 tokio 上下文之外（此处为进程主线程）。
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));

    // Action 链（手写 CPS，fd 由 Open 的返回值贯穿各节点）：
    // Open → Write → Seek(0) → Read → Close → Pure(Bytes)
    let chain = syscall(
        DataOp::Open {
            path: path.clone(),
            flags,
        },
        vec![wr_path(path.clone())],
        move |v| {
            let fd = fd_of(&v);
            syscall(
                DataOp::Write {
                    fd,
                    data: payload.clone(),
                },
                vec![wr(fd)],
                move |_| {
                    syscall(
                        DataOp::Seek {
                            fd,
                            offset: 0,
                            whence: std::io::SeekFrom::Start(0),
                        },
                        vec![rd(fd)],
                        move |_| {
                            syscall(
                                DataOp::Read {
                                    fd,
                                    len: payload.len(),
                                },
                                vec![rd(fd)],
                                move |v| {
                                    let got = match v {
                                        Value::Bytes(b) => b,
                                        other => panic!("期望 Bytes，得到 {other:?}"),
                                    };
                                    syscall(DataOp::Close { fd }, vec![ow(fd)], move |_| {
                                        Action::Pure(Value::Bytes(got))
                                    })
                                },
                            )
                        },
                    )
                },
            )
        },
    );

    let result = rt.run_blocking(chain).expect("run_blocking 执行失败");
    // Write 的 undo 闭包持有文件 Arc（Full 撤销，本例不 recover），
    // drop(rt) 释放撤销栈/注册表，Windows 上才能删除打开句柄的文件。
    drop(rt);
    let _ = std::fs::remove_file(&path);

    match result {
        Value::Bytes(got) if got == expect => {
            println!(
                "✓ 文件往返一致：写入 {} 字节 → 读回 {} 字节（{}）",
                payload_len,
                got.len(),
                path.display()
            );
        }
        Value::Bytes(got) => {
            println!(
                "✗ 文件往返不一致：写入 {} 字节 → 读回 {} 字节",
                payload_len,
                got.len()
            );
            std::process::exit(1);
        }
        other => panic!("意外结果 {other:?}"),
    }
}
