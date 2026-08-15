//! Algeff 对比列：并行读取 10 个不同文件（criterion，harness = false，A7 批 3）。
//!
//! 对照 pdr.md §16「并行读取 10 个不同文件」：原生 tokio = 100%，
//! Algeff 静态路径预期 ~100%（零锁）。
//!
//! ## 实施说明（D17 Fork 并行 + 预开文件）
//! Algeff 臂用 `Action::Fork` 10 路（平衡二叉 Fork 树，combine 汇总字节数）。
//! 契约 D17（A2 批 4）：Fork 无冲突（`fork_conflict`，此处 10 文件互不冲突 →
//! can_parallel=true）且为 `Runtime` 的 Shared 执行器通道时**真并行**
//! （`run_fork_parallel`：spawn_blocking × 2 + current-thread runtime，逐层递归）。
//!
//! **D17 并行分支 fd 分配限制（A7 批 4 实测发现）**：并行分支各自从父 registry
//! 的 `next_fd` 克隆分配 fd 号（D13 隔离 registry），而共享执行器
//! （`TokioExecutor`）的 `files` 等句柄映射以 fd 为键 —— 两分支同时 Open 会
//! 分配**相同 fd 号并互相覆盖映射**（读错文件/EOF），功能断言必败。故本臂
//! 把 Open 移出 Fork：先在父 registry 顺序预开 10 个文件（fd 单调无冲突），
//! Fork 内每叶仅 `Read(fd_i, 1MB) → Close(fd_i)`（不同 fd，互不覆盖）。
//! 被测负载不变（10 个不同文件的并发读，零锁零共享），与原生臂 `tokio::join!`
//! 同构；预开的 Open 成本仍在 iter 计时内（链的一部分）。
//!
//! 批 3（D14 顺序 Fork）实测 340% 的历史对照见 perf/baseline-2026-08-15.txt；
//! 本批（D17 并行）应回归 pdr §16 的 ~100%。

use std::path::{Path, PathBuf};

use algeff_core::action::OpenFlags;
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;
use criterion::{criterion_group, criterion_main, Criterion};

const FILE_COUNT: usize = 10;
const FILE_SIZE: usize = 1024 * 1024; // 每个文件 1MB

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

// ── setup：临时目录生成 10 × 1MB 文件 ─────────────────────────────────

fn setup_ten_files(dir: &Path) -> Vec<PathBuf> {
    let data = vec![0x5au8; FILE_SIZE];
    let mut paths = Vec::with_capacity(FILE_COUNT);
    for i in 0..FILE_COUNT {
        let p = dir.join(format!("file_{i}.bin"));
        std::fs::write(&p, &data).expect("write setup file");
        paths.push(p);
    }
    paths
}

// ── 原生 tokio 参照臂（同参数：10 × 1MB，tokio::join!）────────────────

async fn native_join(paths: &[PathBuf]) -> usize {
    let (a, b, c, d, e, f, g, h, i, j) = tokio::join!(
        tokio::fs::read(&paths[0]),
        tokio::fs::read(&paths[1]),
        tokio::fs::read(&paths[2]),
        tokio::fs::read(&paths[3]),
        tokio::fs::read(&paths[4]),
        tokio::fs::read(&paths[5]),
        tokio::fs::read(&paths[6]),
        tokio::fs::read(&paths[7]),
        tokio::fs::read(&paths[8]),
        tokio::fs::read(&paths[9]),
    );
    a.expect("read0").len()
        + b.expect("read1").len()
        + c.expect("read2").len()
        + d.expect("read3").len()
        + e.expect("read4").len()
        + f.expect("read5").len()
        + g.expect("read6").len()
        + h.expect("read7").len()
        + i.expect("read8").len()
        + j.expect("read9").len()
}

// ── Algeff 臂：预开 10 文件 → Fork 10 路读（Read→Close）────────────────

/// 单文件读叶子：Read(fd, 1MB) → Close → Pure(U64(字节数))。
/// 叶子不 Open（D17 并行分支内分配 fd 会与另一分支碰撞，见头注释）。
fn read_close_leaf(fd: Fd) -> Action {
    syscall(
        DataOp::Read { fd, len: FILE_SIZE },
        vec![use_read_fd(fd)],
        Box::new(move |v| {
            let n = bytes_len(v) as u64;
            syscall(
                DataOp::Close { fd },
                vec![use_own_fd(fd)],
                Box::new(move |_| Action::Pure(Value::U64(n))),
            )
        }),
    )
}

fn fork_sum(left: Action, right: Action) -> Action {
    Action::Fork {
        left: Box::new(left),
        right: Box::new(right),
        combine: Box::new(|lv, rv| Action::Pure(Value::U64(u64_of(lv) + u64_of(rv)))),
    }
}

/// 平衡二叉 Fork 读树（10 叶；can_parallel=true → D17 真并行）。
fn fork_read_tree(fds: &[Fd], lo: usize, hi: usize) -> Action {
    if hi - lo == 1 {
        read_close_leaf(fds[lo])
    } else {
        let mid = (lo + hi) / 2;
        fork_sum(fork_read_tree(fds, lo, mid), fork_read_tree(fds, mid, hi))
    }
}

/// 预开链：顺序 Open 全部文件（父 registry 单调分配 fd，无 D17 并行冲突）
/// → 全部就绪后进入 Fork 读树。
fn open_all(paths: Vec<PathBuf>, idx: usize, fds: Vec<Fd>) -> Action {
    if idx == paths.len() {
        return fork_read_tree(&fds, 0, fds.len());
    }
    let path = paths[idx].clone();
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
            let mut fds = fds;
            fds.push(fd_of(v));
            open_all(paths, idx + 1, fds)
        }),
    )
}

fn parallel_reads_chain(paths: Vec<PathBuf>) -> Action {
    open_all(paths, 0, Vec::new())
}

fn bench_parallel_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("algeff_parallel_reads");
    // IO 型基准：降低样本数以控制 setup（10MB/样本）总开销（同批 2）。
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(3));

    group.bench_function("tokio_native_10x1MB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = setup_ten_files(dir.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(native_join(&paths))));
    });

    group.bench_function("algeff_fork10_x1MB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = setup_ten_files(dir.path());
        // Runtime 自持 tokio reactor（D9）：criterion setup 无 tokio 上下文。
        let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));
        // 功能验证（setup 内、不计时）：10 路读应汇总 10MB。
        {
            let chain = parallel_reads_chain(paths.clone());
            let v = runtime.run_blocking(chain).expect("fork 读链执行失败");
            assert_eq!(
                v,
                Value::U64((FILE_COUNT * FILE_SIZE) as u64),
                "10 路读应汇总全部字节数"
            );
        }
        b.iter(|| {
            let chain = parallel_reads_chain(paths.clone());
            let v = runtime
                .run_blocking(chain)
                .expect("fork 读链执行失败（测量中）");
            assert_eq!(v, Value::U64((FILE_COUNT * FILE_SIZE) as u64));
            criterion::black_box(v)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_parallel_reads);
criterion_main!(benches);
