//! 并行追加同一文件（顺序无关场景）—— 原生 tokio 基线。
//!
//! 对应 pdr.md §16「并行追加同一文件（顺序无关）」的 100% 原生 tokio 参照列。
//! 10 个任务各自以 append 模式（O_APPEND）打开同一文件并写入 32 × 1KB，
//! 写入顺序无关，由内核保证单次写原子追加。

use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::io::AsyncWriteExt;

const TASKS: usize = 10;
const CHUNKS_PER_TASK: usize = 32;
const CHUNK_LEN: usize = 1024;

/// 单任务：append 打开并追加 CHUNKS_PER_TASK × 1KB。
async fn append_task(path: PathBuf) -> std::io::Result<()> {
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

/// 10 个任务并行追加同一文件，返回成功任务数。
async fn parallel_append(path: &Path, tasks: usize) -> u64 {
    let mut handles = Vec::with_capacity(tasks);
    for _ in 0..tasks {
        handles.push(tokio::spawn(append_task(path.to_path_buf())));
    }
    let mut ok = 0u64;
    for h in handles {
        h.await.expect("task").expect("append");
        ok += 1;
    }
    ok
}

fn bench_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("append");
    // IO 型基准：降低样本数（每样本新 tempdir + 新文件）。
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(3));
    group.bench_function("tokio_10tasks_x32KB", |b| {
        // setup：临时目录 + 自建 tokio runtime（每个测量样本一次，不计入计时）。
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("append.log");
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(parallel_append(&path, TASKS))));
    });
    group.finish();
}

criterion_group!(benches, bench_append);
criterion_main!(benches);
