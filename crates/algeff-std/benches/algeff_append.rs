//! Algeff 对比列：并行追加同一文件（criterion，harness = false，A7 批 3）。
//!
//! 对照 pdr.md §16「并行追加同一文件（顺序无关）」：原生 tokio = 100%，
//! Algeff 静态路径预期 ~100%。
//!
//! ## 实施说明（D6：Append∥Append 默认串行）
//! 契约决策 D6：Append∥Append 默认**串行**（仅当结果顺序无关时并行，且需调用方
//! 显式 opt-in）。本基准直接测量 Algeff 串行路径：10 个追加任务经 interpret
//! Sequential 链顺序执行（每个任务 Open{append} → Write(32KB) → Close）。
//! opt-in 并行（`can_parallel_with(..., append_order_insensitive=true)` + 阶段 3
//! Fork 并行调度）留给后续批，见 rfc。
//!
//! 资源声明：Open 的路径资源用 `TypedResource::<AppendOnly>::new_append` 声明
//! Append 模式（A4 不消费，10 次 Open 同一路径均通过；adapters::open_file 只有
//! read/write 分支，故 bench 内部手写声明——CTO 批准）。每次 Write 声明新 fd 的
//! Write 模式（各 fd 恰好消费一次）；Close 声明 Own。
//!
//! 对比基准：本文件内建 `tokio_native_10tasks_x32KB` 同参数参照臂（与批 2 相同的
//! 10 任务 × 32 × 1KB O_APPEND 并行追加），百分比 = algeff/原生 × 100%。
//! 批 2 的 7.1652 ms 保留在基线文件作为历史参照。

use std::path::PathBuf;

use algeff_core::action::{Bytes, OpenFlags};
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;
use criterion::{criterion_group, criterion_main, Criterion};
use tokio::io::AsyncWriteExt;

const TASKS: usize = 10;
const CHUNKS_PER_TASK: usize = 32;
const CHUNK_LEN: usize = 1024;

// ── 公共小工具（bench 内部构造，禁止改 src/，故本地复制）────────────

fn syscall(op: DataOp, resources: Vec<ResourceUsage>, next: NextFn) -> Action {
    Action::Syscall {
        op,
        resources,
        next,
    }
}

fn use_append_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<AppendOnly>::new_append(ResourceInner::Path(path)).into_usage()
}
fn use_write_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn use_own_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<Owned>::new_owned(ResourceInner::Fd(fd)).into_usage()
}

fn fd_of(v: Value) -> Fd {
    match v {
        Value::Fd(f) => f,
        other => panic!("期望 Fd，得到 {other:?}"),
    }
}

// ── 原生 tokio 参照臂（同参数：10 任务 × 32KB 并行追加）───────────────

async fn native_append_task(path: PathBuf) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await?;
    let chunk = vec![b'z'; CHUNK_LEN];
    for _ in 0..CHUNKS_PER_TASK {
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

async fn native_parallel_append(path: &std::path::Path, tasks: usize) -> u64 {
    let mut handles = Vec::with_capacity(tasks);
    for _ in 0..tasks {
        handles.push(tokio::spawn(native_append_task(path.to_path_buf())));
    }
    let mut ok = 0u64;
    for h in handles {
        h.await.expect("task").expect("append");
        ok += 1;
    }
    ok
}

// ── Algeff 臂：10 任务顺序追加（Sequential 链，D6 默认串行）───────────

const APPEND_FLAGS: OpenFlags = OpenFlags {
    read: false,
    write: true,
    append: true,
    create: true,
    truncate: false,
    exclusive: false,
};

/// 单任务：Open{append} → Write(fd, 32KB) → Close → `tail`。
fn append_task(path: PathBuf, chunk: Bytes, tail: Action) -> Action {
    let usage = use_append_path(path.clone());
    syscall(
        DataOp::Open {
            path,
            flags: APPEND_FLAGS,
        },
        vec![usage],
        Box::new(move |v| {
            let fd = fd_of(v);
            syscall(
                DataOp::Write { fd, data: chunk },
                vec![use_write_fd(fd)],
                Box::new(move |_| {
                    syscall(
                        DataOp::Close { fd },
                        vec![use_own_fd(fd)],
                        Box::new(move |_| tail),
                    )
                }),
            )
        }),
    )
}

/// N 个追加任务串行拼接（task k 的 Close 接 task k+1 的 Open，惰性展开）。
fn append_chain(path: PathBuf, tasks: usize, chunk: Bytes) -> Action {
    if tasks == 0 {
        return Action::Pure(Value::U64(0));
    }
    let tail = append_chain(path.clone(), tasks - 1, chunk.clone());
    append_task(path, chunk, tail)
}

fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("algeff_append");
    // IO 型基准：降低样本数（每样本新 tempdir + 新文件）。
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(3));

    group.bench_function("tokio_native_10tasks_x32KB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("append.log");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(native_parallel_append(&path, TASKS))));
    });

    group.bench_function("algeff_serial_10tasks_x32KB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("append.log");
        // Runtime 自持 tokio reactor（D9）：criterion setup 无 tokio 上下文。
        let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));
        let chunk = vec![b'z'; CHUNK_LEN];
        // 功能验证（setup 内、不计时）：串行追加链应成功。
        {
            let chain = append_chain(path.clone(), TASKS, chunk.clone());
            runtime.run_blocking(chain).expect("追加链执行失败");
        }
        b.iter(|| {
            let chain = append_chain(path.clone(), TASKS, chunk.clone());
            let v = runtime
                .run_blocking(chain)
                .expect("追加链执行失败（测量中）");
            criterion::black_box(v)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_append);
criterion_main!(benches);
