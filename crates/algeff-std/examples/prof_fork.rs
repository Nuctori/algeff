//! 性能剖析：分解 Algeff Fork 链成本（迭代 3-A1 产物，配合 perf/ 基线使用）。
//! 用法：cargo run --release -p algeff-std --example prof_fork
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Instant;

use algeff_core::action::OpenFlags;
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;

const FILE_COUNT: usize = 10;
const FILE_SIZE: usize = 1024 * 1024;

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

fn fork_read_tree(fds: &[Fd], lo: usize, hi: usize) -> Action {
    if hi - lo == 1 {
        read_close_leaf(fds[lo])
    } else {
        let mid = (lo + hi) / 2;
        fork_sum(fork_read_tree(fds, lo, mid), fork_read_tree(fds, mid, hi))
    }
}

fn fork_pure_tree(lo: usize, hi: usize) -> Action {
    if hi - lo == 1 {
        Action::Pure(Value::U64(1))
    } else {
        let mid = (lo + hi) / 2;
        fork_sum(fork_pure_tree(lo, mid), fork_pure_tree(mid, hi))
    }
}

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

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = setup_ten_files(dir.path());
    let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));

    // 1) 纯 Fork 树（无 IO）：隔离 spawn/驱动/克隆/合并开销
    let mut pure_times = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let v = runtime
            .run_blocking(fork_pure_tree(0, FILE_COUNT))
            .expect("pure fork tree");
        assert_eq!(v, Value::U64(FILE_COUNT as u64));
        pure_times.push(t.elapsed());
    }
    pure_times.sort();
    let n = pure_times.len();
    println!(
        "pure fork tree (10 leaves, 9 nodes): med={:.3}ms min={:.3}ms max={:.3}ms",
        pure_times[n / 2].as_secs_f64() * 1e3,
        pure_times[0].as_secs_f64() * 1e3,
        pure_times[n - 1].as_secs_f64() * 1e3
    );

    // 2) 顺序路径读（无 Fork）：10 次顺序 read+close
    //    用 Sequential 串接 read_close_leaf —— 但叶子需要先 Open……
    //    改为：预开 + 顺序 Sequential 读链
    let mut seq_times = Vec::new();
    for _ in 0..20 {
        // 先预开（不计时部分由 run_blocking 内部完成）——无法拆分，改为分开测：
        // open 链计时
        let t = Instant::now();
        let v = runtime
            .run_blocking(open_all(paths.clone(), 0, Vec::new()))
            .expect("fork 读链");
        assert_eq!(v, Value::U64((FILE_COUNT * FILE_SIZE) as u64));
        seq_times.push(t.elapsed());
    }
    seq_times.sort();
    let n = seq_times.len();
    println!(
        "full fork read chain: med={:.3}ms min={:.3}ms max={:.3}ms",
        seq_times[n / 2].as_secs_f64() * 1e3,
        seq_times[0].as_secs_f64() * 1e3,
        seq_times[n - 1].as_secs_f64() * 1e3
    );

    // 3) 单次 1MB 读（Open+Read+Close 顺序）
    let mut single_times = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let v = runtime
            .run_blocking(single_read_chain(paths[0].clone()))
            .expect("单读链");
        assert_eq!(v, Value::U64(FILE_SIZE as u64));
        single_times.push(t.elapsed());
    }
    single_times.sort();
    let n = single_times.len();
    println!(
        "single 1MB read (Open+Read+Close): med={:.3}ms min={:.3}ms max={:.3}ms",
        single_times[n / 2].as_secs_f64() * 1e3,
        single_times[0].as_secs_f64() * 1e3,
        single_times[n - 1].as_secs_f64() * 1e3
    );

    // 3b) 只 open 链（不读）：10 次顺序 Open + Close 立即
    let mut open_times = Vec::new();
    for _ in 0..20 {
        let chain = open_only_chain(paths.clone());
        let t = Instant::now();
        let v = runtime.run_blocking(chain).expect("open 链");
        assert_eq!(v, Value::U64(FILE_COUNT as u64));
        open_times.push(t.elapsed());
    }
    open_times.sort();
    let n = open_times.len();
    println!(
        "10 sequential Open+Close: med={:.3}ms min={:.3}ms max={:.3}ms",
        open_times[n / 2].as_secs_f64() * 1e3,
        open_times[0].as_secs_f64() * 1e3,
        open_times[n - 1].as_secs_f64() * 1e3
    );

    // 4) 顺序读 10 文件（预开后 Sequential 串行读）
    let mut seq_read_times = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let v = runtime
            .run_blocking(open_then_seq_reads(paths.clone()))
            .expect("顺序读链");
        assert_eq!(v, Value::U64((FILE_COUNT * FILE_SIZE) as u64));
        seq_read_times.push(t.elapsed());
    }
    seq_read_times.sort();
    let n = seq_read_times.len();
    println!(
        "seq read 10 files (pre-open, read+close each): med={:.3}ms min={:.3}ms max={:.3}ms",
        seq_read_times[n / 2].as_secs_f64() * 1e3,
        seq_read_times[0].as_secs_f64() * 1e3,
        seq_read_times[n - 1].as_secs_f64() * 1e3
    );

    // 5) 微基准：drive（current-thread runtime 构建 + block_on 空 future）
    let mut drive_times = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        for _ in 0..10 {
            let v: u64 = drive_probe(async { 42u64 });
            criterion_black_box(v);
        }
        drive_times.push(t.elapsed());
    }
    drive_times.sort();
    let n = drive_times.len();
    println!(
        "drive (runtime build + block_on empty): med={:.3}us/call",
        drive_times[n / 2].as_secs_f64() * 1e6 / 10.0
    );

    // 6) 微基准：spawn_blocking 空任务（经 Runtime::run_blocking 所在 reactor）
    let mut spawn_times = Vec::new();
    for _ in 0..50 {
        let t = Instant::now();
        for _ in 0..10 {
            let rt = runtime_ref();
            let h = rt.spawn_blocking(|| 42u64);
            criterion_black_box(rt.block_on(h).expect("spawn_blocking"));
        }
        spawn_times.push(t.elapsed());
    }
    spawn_times.sort();
    let n = spawn_times.len();
    println!(
        "spawn_blocking empty (multi-thread rt, warm pool): med={:.3}us/call",
        spawn_times[n / 2].as_secs_f64() * 1e6 / 10.0
    );
}

fn criterion_black_box<T>(d: T) -> T {
    std::hint::black_box(d)
}

fn drive_probe<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(f)
}

fn runtime_ref() -> &'static tokio::runtime::Runtime {
    // 复用 Runtime::new 内部 reactor：模拟 run_blocking 的 spawn_blocking 调用点。
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| tokio::runtime::Runtime::new().expect("multi-thread runtime"))
}

/// 顺序读 10 文件链：预开全部 → 顺序 Read(1MB)+Close → Pure(U64)。
fn open_then_seq_reads(paths: Vec<PathBuf>) -> Action {
    open_then_seq(paths, 0, Vec::new())
}

fn open_then_seq(paths: Vec<PathBuf>, idx: usize, fds: Vec<Fd>) -> Action {
    if idx == paths.len() {
        return seq_reads(fds, 0, 0);
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
            open_then_seq(paths, idx + 1, fds)
        }),
    )
}

fn seq_reads(fds: Vec<Fd>, idx: usize, acc: u64) -> Action {
    if idx == fds.len() {
        return Action::Pure(Value::U64(acc));
    }
    let fd = fds[idx];
    syscall(
        DataOp::Read { fd, len: FILE_SIZE },
        vec![use_read_fd(fd)],
        Box::new(move |v| {
            let n = bytes_len(v) as u64;
            syscall(
                DataOp::Close { fd },
                vec![use_own_fd(fd)],
                Box::new(move |_| seq_reads(fds, idx + 1, acc + n)),
            )
        }),
    )
}

/// 单次 1MB 读链：Open → Read(1MB) → Close → Pure(U64)。
fn single_read_chain(path: PathBuf) -> Action {
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
        }),
    )
}

/// 顺序 Open → Close 链（每文件 Open 后立即 Close，返回 U64(计数)）。
fn open_only_chain(paths: Vec<PathBuf>) -> Action {
    open_seq(paths, 0, 0)
}

fn open_seq(paths: Vec<PathBuf>, idx: usize, count: u64) -> Action {
    if idx == paths.len() {
        return Action::Pure(Value::U64(count));
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
            let fd = fd_of(v);
            syscall(
                DataOp::Close { fd },
                vec![use_own_fd(fd)],
                Box::new(move |_| open_seq(paths, idx + 1, count + 1)),
            )
        }),
    )
}
