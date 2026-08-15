//! 并行读 10 个不同文件（Read 不同资源，零冲突）—— 原生 tokio 基线。
//!
//! 对应 pdr.md §16「并行读取 10 个不同文件」的 100% 原生 tokio 参照列。
//! setup（tempfile 生成 10 × 1MB）放在 bench 闭包内、b.iter 之外：
//! criterion 每个测量样本调用一次该闭包，仅对 iter 闭包计时，
//! 因此 setup 不计入测量。

use std::path::{Path, PathBuf};

use criterion::{criterion_group, criterion_main, Criterion};

const FILE_COUNT: usize = 10;
const FILE_SIZE: usize = 1024 * 1024; // 每个文件 1MB

/// setup：在临时目录生成 10 个 1MB 文件。
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

/// tokio::join! 并行读 10 个文件，返回总字节数（防死代码消除）。
async fn parallel_read_ten(paths: &[PathBuf]) -> usize {
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

fn bench_parallel_reads(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel_reads");
    // IO 型基准：降低样本数以控制 setup（10MB/样本）总开销。
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(3));
    group.bench_function("tokio_join_10x1MB", |b| {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = setup_ten_files(dir.path());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(parallel_read_ten(&paths))));
    });
    group.finish();
}

criterion_group!(benches, bench_parallel_reads);
criterion_main!(benches);
