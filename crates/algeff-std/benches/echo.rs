//! 原生 tokio TCP echo 基线基准（criterion，harness = false）。
//!
//! 场景：本地 bind（127.0.0.1:0），N=1000 次连接，每次发送 1KB 并读回 1KB
//! （pdr.md §16「网络 Echo（无共享资源）」的 100% 原生 tokio 参照列）。
//!
//! A7 阶段说明：A5 的 TokioExecutor 尚在另一分支（本 worktree 中为
//! todo!()），本阶段只实现原生 tokio 基线；Algeff 对比项
//! （`Runtime::new(TokioExecutor)` + `interpret` 执行同一 echo Action 链）
//! 在阶段 3 A5 合并后补充，接入点设计见 A7 交付报告。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const N_CONNECTIONS: usize = 1000;
const PAYLOAD_LEN: usize = 1024;

/// 单次 echo 往返：连接 → 发送 1KB → 读回 1KB。
async fn echo_roundtrip(addr: SocketAddr, payload: &[u8]) -> usize {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    stream.write_all(payload).await.expect("write");
    let mut buf = [0u8; PAYLOAD_LEN];
    stream.read_exact(&mut buf).await.expect("read");
    buf.len()
}

/// N 次串行 echo 往返。服务端：accept 循环 + 每连接一个 echo 任务，
/// 收到第 N 个连接后停止接受（本轮测量结束）。
async fn run_echo(n: usize) -> usize {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");

    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_srv = Arc::clone(&accepted);

    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let done = accepted_srv.fetch_add(1, Ordering::SeqCst) + 1 >= n;
            tokio::spawn(async move {
                let mut buf = [0u8; PAYLOAD_LEN];
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(m) => {
                            if sock.write_all(&buf[..m]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
            if done {
                break;
            }
        }
    });

    let payload = vec![0xabu8; PAYLOAD_LEN];
    let mut total = 0usize;
    for _ in 0..n {
        total += echo_roundtrip(addr, &payload).await;
    }
    let _ = accepted;
    // 等 accept 循环结束（最后一个连接的处理任务随客户端关闭而自行退出）。
    let _ = server.await;
    total
}

fn bench_echo(c: &mut Criterion) {
    let mut group = c.benchmark_group("echo");
    group.sample_size(30);
    group.measurement_time(std::time::Duration::from_secs(3));
    group.bench_function("tokio_native_1000x1KB", |b| {
        // 自建 tokio runtime：位于 bench 闭包内、b.iter 之外 ——
        // 每个测量样本重建一次，不计入计时（criterion 只对 iter 闭包计时）。
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(run_echo(N_CONNECTIONS))));
    });
    group.finish();
}

criterion_group!(benches, bench_echo);
criterion_main!(benches);
