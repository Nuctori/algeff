//! 共享读：Arc<File> 共享并发读同一文件（Read-Read 并行场景）。
//!
//! 对应 pdr.md §16「并行读取同一文件（只读共享）」的 100% 原生 tokio 参照列。
//!
//! 实现说明：`tokio::fs::File` 无位置读 `read_at`（其 `read` 从共享的
//! OS 文件偏移处读取，多克隆句柄并发 seek+read 会竞争偏移），因此本基准
//! 采用跨平台正确基元：`Arc<std::fs::File>` 共享同一句柄 +
//! 按偏移的原子位置读（unix `read_at` / windows `seek_read`），
//! 在 `tokio::task::spawn_blocking` 中执行 —— 零锁、无偏移竞争，
//! 8 个任务各自读互不重叠的 1MB 区间。

use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};

const FILE_SIZE: u64 = 8 * 1024 * 1024; // 共享文件 8MB
const TASKS: usize = 8; // 8 个并发读任务
const CHUNK: usize = 64 * 1024; // 每任务分块大小

#[cfg(unix)]
use std::os::unix::fs::FileExt as _;
#[cfg(windows)]
use std::os::windows::fs::FileExt as _;

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

async fn shared_read_parallel(file: &Arc<std::fs::File>, tasks: usize) -> u64 {
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

fn bench_shared_read(c: &mut Criterion) {
    let mut group = c.benchmark_group("shared_read");
    // IO 型基准：降低样本数以控制 setup（8MB/样本）总开销。
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(3));
    group.bench_function("arc_file_8tasks_8MB", |b| {
        // setup：生成 8MB 共享文件 + 自建 tokio runtime。
        // 每个测量样本执行一次，criterion 只对 iter 闭包计时。
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("shared.bin");
        std::fs::write(&file_path, vec![0u8; FILE_SIZE as usize]).expect("setup file");
        let file = Arc::new(std::fs::File::open(&file_path).expect("open shared file"));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(shared_read_parallel(&file, TASKS))));
    });
    group.finish();
}

criterion_group!(benches, bench_shared_read);
criterion_main!(benches);
