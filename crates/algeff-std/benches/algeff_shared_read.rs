//! Algeff 对比列：并行读取同一文件（只读共享）（criterion，harness = false，A7 批 3）。
//!
//! 对照 pdr.md §16「并行读取同一文件（只读共享）」：原生 tokio = 100%，
//! Algeff 静态路径预期 ~100%（读-读可并行）。
//!
//! ## 实施说明（D14 顺序 Fork + 游标读）
//! Algeff 臂：Open{read} 共享文件 → `Action::Fork` 8 路（平衡二叉 Fork 树，combine
//! 汇总字节数）→ Close。每路 = Read(fd, 1MB)，8 路声明同一 fd 的 Read 模式
//! （A4：Read 不消费，可重复）。契约决策 D14：Fork 阶段 1 = 静态冲突检测
//! （读-读无冲突，`can_parallel` 通过）+ **顺序执行**——因此 8 路实际串行；
//! 又因顺序执行，游标语义良定义：第 k 路自然读到 [k×1MB, (k+1)×1MB)，等价于
//! 原生臂的位置读区间，无需显式 Seek。实测耗时为串行 8 × 1MB 读——**预期差距
//! 归因 D14 顺序 Fork**，并行化属阶段 3 待办（pdr §16 的 ~100% 是并行化后目标值）。
//!
//! 对比基准：本文件内建 `tokio_native_8tasks_8MB` 同参数参照臂（与批 2 相同的
//! Arc<File> + 位置读 + spawn_blocking，零锁零偏移竞争）。

use std::path::PathBuf;
use std::sync::Arc;

use algeff_core::action::OpenFlags;
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;
use criterion::{criterion_group, criterion_main, Criterion};

const FILE_SIZE: u64 = 8 * 1024 * 1024; // 共享文件 8MB
const TASKS: usize = 8; // 8 个并发读任务
const CHUNK: usize = 64 * 1024; // 原生臂每任务分块大小

#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(windows)]
use std::os::windows::fs::FileExt as _;

// ── 公共小工具（bench 内部构造，禁止改 src/，故本地复制）────────────

fn syscall(op: DataOp, resources: Vec<ResourceUsage>, next: NextFn) -> Action {
    Action::Syscall {
        op,
        resources,
        next,
    }
}

fn use_read_path(path: PathBuf) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Path(path)).into_usage()
}
fn use_read_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
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

fn bytes_len(v: Value) -> usize {
    match v {
        Value::Bytes(b) => b.len(),
        other => panic!("期望 Bytes，得到 {other:?}"),
    }
}

fn u64_of(v: Value) -> u64 {
    match v {
        Value::U64(u) => u,
        other => panic!("期望 U64，得到 {other:?}"),
    }
}

// ── 原生 tokio 参照臂（同参数：Arc<File> + 8 位置读任务）──────────────

/// 按偏移原子位置读（不移动共享的 OS 文件偏移）。
fn positional_read(file: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    #[cfg(unix)]
    {
        file.read_at(buf, offset)
    }
    #[cfg(windows)]
    {
        file.seek_read(buf, offset)
    }
}

/// 一个任务：位置读 [start, end) 区间，返回读到字节数。
fn read_region(file: Arc<std::fs::File>, start: u64, end: u64) -> u64 {
    let mut buf = vec![0u8; CHUNK];
    let mut offset = start;
    let mut total = 0u64;
    while offset < end {
        let want = std::cmp::min(CHUNK as u64, end - offset) as usize;
        let n = positional_read(&file, &mut buf[..want], offset).expect("read_at");
        if n == 0 {
            break;
        }
        total += n as u64;
        offset += n as u64;
    }
    total
}

async fn native_shared_read(file: &Arc<std::fs::File>, tasks: usize) -> u64 {
    let per_task = FILE_SIZE / tasks as u64;
    let mut handles = Vec::with_capacity(tasks);
    for t in 0..tasks {
        let f = Arc::clone(file);
        let start = t as u64 * per_task;
        let end = start + per_task;
        handles.push(tokio::spawn(async move {
            tokio::task::spawn_blocking(move || read_region(f, start, end))
                .await
                .expect("blocking read")
        }));
    }
    let mut sum = 0u64;
    for h in handles {
        sum += h.await.expect("task");
    }
    sum
}

// ── Algeff 臂：Fork 8 路共享读同一 fd ─────────────────────────────────

/// 单路读：Read(fd, 1MB) → Pure(U64(字节数))（顺序 Fork 下游标自然前进）。
fn read_one(fd: Fd) -> Action {
    syscall(
        DataOp::Read {
            fd,
            len: (FILE_SIZE / TASKS as u64) as usize,
        },
        vec![use_read_fd(fd)],
        Box::new(move |v| Action::Pure(Value::U64(bytes_len(v) as u64))),
    )
}

fn fork_sum(left: Action, right: Action) -> Action {
    Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|lv, rv| Action::Pure(Value::U64(u64_of(lv) + u64_of(rv)))),
    }
}

/// 平衡二叉 Fork 树（8 叶；D14 阶段 1 = 静态检测 + 顺序执行）。
fn fork_tree(fd: Fd, lo: usize, hi: usize) -> Action {
    if hi - lo == 1 {
        read_one(fd)
    } else {
        let mid = (lo + hi) / 2;
        fork_sum(fork_tree(fd, lo, mid), fork_tree(fd, mid, hi))
    }
}

/// Open{read} → Fork 8 路读 → Close → Pure(总字节数)。
fn shared_read_chain(path: PathBuf) -> Action {
    let usage = use_read_path(path.clone());
    syscall(
        DataOp::Open {
            path,
            flags: OpenFlags {
                read: true,
                ..Default::default()
            },
        },
        vec![usage],
        Box::new(move |v| {
            let fd = fd_of(v);
            // Fork 树结果 → Close → 原样传递结果。
            let tree = fork_tree(fd, 0, TASKS);
            Action::Sequential {
                current: Box::new(tree),
                next: Box::new(move |v| {
                    let n = u64_of(v);
                    syscall(
                        DataOp::Close { fd },
                        vec![use_own_fd(fd)],
                        Box::new(move |_| Action::Pure(Value::U64(n))),
                    )
                }),
            }
        }),
    )
}

fn bench_shared_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("algeff_shared_read");
    // IO 型基准：降低样本数以控制 setup（8MB/样本）总开销（同批 2）。
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(3));

    group.bench_function("tokio_native_8tasks_8MB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("shared.bin");
        std::fs::write(&file_path, vec![0u8; FILE_SIZE as usize]).expect("setup file");
        let file = Arc::new(std::fs::File::open(&file_path).expect("open shared file"));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(native_shared_read(&file, TASKS))));
    });

    group.bench_function("algeff_fork8_shared_8MB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("shared.bin");
        std::fs::write(&file_path, vec![0u8; FILE_SIZE as usize]).expect("setup file");
        // Runtime 自持 tokio reactor（D9）：criterion setup 无 tokio 上下文。
        let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));
        // 功能验证（setup 内、不计时）：8 路读应汇总 8MB。
        {
            let chain = shared_read_chain(file_path.clone());
            let v = runtime.run_blocking(chain).expect("共享读链执行失败");
            assert_eq!(v, Value::U64(FILE_SIZE), "8 路共享读应汇总全部字节数");
        }
        b.iter(|| {
            let chain = shared_read_chain(file_path.clone());
            let v = runtime.run_blocking(chain).expect("共享读链执行失败（测量中）");
            assert_eq!(v, Value::U64(FILE_SIZE));
            criterion::black_box(v)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_shared_read);
criterion_main!(benches);
