//! Algeff 对比列：网络 Echo（criterion，harness = false，A7 批 3）。
//!
//! 对照 pdr.md §16「网络 Echo（无共享资源）」：原生 tokio = 100%，
//! Algeff 静态路径预期 ~103%。
//!
//! ## 负载设计（CTO 裁决 2026-08-15，A4 线性语义阻塞）
//! 批 2 README §4.1 原计划「100 连接 × 每连接 1000 往返」无法在冻结契约下
//! 表达：A4 资源线性（resource.rs `check_linear`）规定同一资源 Write 恰好消费
//! 一次，而每连接 1000 次往返 = 同一连接 fd 上 1000 次 Write —— 第 2 个往返即被
//! 拒绝（`Err(InvalidInput)`，见 execution_axioms.rs::exec_A4_linearity_runtime）。
//! pdr.md §14 `handle_client` 的规范形态恰是「每连接 1 往返」（read→write→close）。
//!
//! CTO 裁决（选项 A）：本文件内建**等负载双对比项**——
//! `tokio_native_100conns_x1rtt_1KB`（同参数原生参照）+ `algeff_100conns_x1rtt_1KB`
//! （`Runtime::new(TokioExecutor)` + `run_blocking` 执行 TcpConnect→TcpWrite→
//! TcpRead→TcpClose 链）。百分比 = algeff/原生 × 100% = 纯解释器+线性检查开销。
//! 批 2 的 100×1000 原生基线保留为历史参照列（A4 下不可等比，见基线文件备注）。
//!
//! ## Windows 端口预算（README §5 约束，沿用批 2 裁决）
//! 每 iter = 100 连接 × 1 往返。每样本 iter 数由 warm_up/measurement 时间限制在
//! 个位数（per-iter ≈ 20-40ms，measurement_time=100ms → ~2-4 iter/样本），
//! 单 bench 总连接数 ≈ 样本数 × 每样本 iter 数 × 100 ≈ 2-4k，与批 2（3.1k 连接、
//! 实测峰值 TIME_WAIT 2447）同一量级，安全。连接全部顺序执行（先 connect 再 close，
//! 不并发），进一步压低同刻 TIME_WAIT。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use algeff_core::action::Bytes;
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;
use criterion::{criterion_group, criterion_main, Criterion};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const N_CONNECTIONS: usize = 100; // 每测量样本的连接数（CTO 裁决，端口预算见头注释）
const ROUNDTRIPS: usize = 1; // A4：每连接恰 1 往返（CTO 裁决选项 A）
const PAYLOAD_LEN: usize = 1024;

// ── 公共小工具（bench 内部构造，禁止改 src/，故本地复制）────────────

fn syscall(op: DataOp, resources: Vec<ResourceUsage>, next: NextFn) -> Action {
    Action::Syscall {
        op,
        resources,
        next,
    }
}

fn use_read_fd(fd: Fd) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
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

fn bytes_len(v: Value) -> usize {
    match v {
        Value::Bytes(b) => b.len(),
        other => panic!("期望 Bytes，得到 {other:?}"),
    }
}

/// 服务端：独立线程 + 自有 tokio runtime 的常驻 accept 循环。
/// 每连接派生一个 echo 任务（读到 EOF 或错误即退出）。Algeff 侧 `Runtime::new`
/// 自持 reactor（D9）且 `run_blocking` 不能在 tokio 上下文中嵌套调用，故服务端
/// 必须与测量线程分离（双方对比项共用同一服务端实现，保证对称）。
struct EchoServer {
    addr: SocketAddr,
    /// 已 accept 的连接计数（功能验证用：确认 Algeff 链确实完成了全部连接）。
    accepted: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl EchoServer {
    fn start() -> Self {
        let (tx_addr, rx_addr) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_srv = Arc::clone(&stop);
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_srv = Arc::clone(&accepted);
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            rt.block_on(async move {
                let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
                let addr = listener.local_addr().expect("local_addr");
                let _ = tx_addr.send(addr);
                while !stop_srv.load(Ordering::Relaxed) {
                    match listener.accept().await {
                        Ok((mut sock, _)) => {
                            accepted_srv.fetch_add(1, Ordering::SeqCst);
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
                        }
                        Err(_) => break,
                    }
                }
            });
        });
        let addr = rx_addr.recv().expect("server addr");
        EchoServer {
            addr,
            accepted,
            stop,
            handle: Some(handle),
        }
    }

    /// 本轮已完成的连接数（链执行完毕后读取）。
    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // 解除可能阻塞的 accept：发起一次哑连接（本地、不计时）。
        let _ = std::net::TcpStream::connect_timeout(&self.addr, Duration::from_millis(100));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ── 原生 tokio 参照臂（同参数：100 连接 × 1 往返）────────────────────

async fn native_session(addr: SocketAddr, payload: &[u8]) {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let mut buf = [0u8; PAYLOAD_LEN];
    for _ in 0..ROUNDTRIPS {
        stream.write_all(payload).await.expect("write");
        stream.read_exact(&mut buf).await.expect("read");
    }
}

async fn native_run(addr: SocketAddr, n: usize) -> usize {
    let payload = vec![0xabu8; PAYLOAD_LEN];
    let mut total = 0usize;
    for _ in 0..n {
        native_session(addr, &payload).await;
        total += PAYLOAD_LEN;
    }
    total
}

// ── Algeff 臂：TcpConnect→TcpWrite→TcpRead→TcpClose 链 ────────────────

/// 读满 `remaining` 字节后执行 `done`（TCP 分片安全：循环经 next 闭包展开）。
fn read_until(fd: Fd, remaining: usize, done: Action) -> Action {
    if remaining == 0 {
        return done;
    }
    syscall(
        DataOp::TcpRead { fd, len: remaining },
        vec![use_read_fd(fd)],
        Box::new(move |v| {
            let n = bytes_len(v); // n ≤ remaining（TcpRead 至多返回剩余长度）
            read_until(fd, remaining - n, done)
        }),
    )
}

/// 单连接会话：TcpConnect → TcpWrite(1KB) → 读满 1KB → TcpClose → `tail`。
fn conn_session(addr: SocketAddr, payload: Bytes, tail: Action) -> Action {
    syscall(
        DataOp::TcpConnect { addr },
        vec![],
        Box::new(move |v| {
            let fd = fd_of(v);
            let after_read = syscall(
                DataOp::Close { fd },
                vec![use_own_fd(fd)],
                Box::new(move |_| tail),
            );
            let read_loop = read_until(fd, PAYLOAD_LEN, after_read);
            syscall(
                DataOp::TcpWrite { fd, data: payload },
                vec![use_write_fd(fd)],
                Box::new(move |_| read_loop),
            )
        }),
    )
}

/// N 连接串行 echo 会话链（iter 开始处一次预建：递归深度 = N，链长 ≈ 4N 节点；
/// 构造开销计入 Algeff 臂测量——CPS 蓝图构建本就是 Algeff 使用成本的一部分）。
fn echo_client(addr: SocketAddr, payload: Bytes, connections: usize) -> Action {
    if connections == 0 {
        return Action::Pure(Value::U64(0));
    }
    let tail = echo_client(addr, payload.clone(), connections - 1);
    conn_session(addr, payload, tail)
}

fn bench_echo(c: &mut Criterion) {
    let mut group = c.benchmark_group("algeff_echo");
    // IO 型基准 + Windows 端口预算：样本少、每样本 iter 数受控（见头注释）。
    group.sample_size(10);
    group.warm_up_time(Duration::from_millis(50));
    group.measurement_time(Duration::from_millis(100));

    group.bench_function("tokio_native_100conns_x1rtt_1KB", |b| {
        let server = EchoServer::start();
        let addr = server.addr;
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        b.iter(|| criterion::black_box(rt.block_on(native_run(addr, N_CONNECTIONS))));
    });

    group.bench_function("algeff_100conns_x1rtt_1KB", |b| {
        let server = EchoServer::start();
        let addr = server.addr;
        let payload = vec![0xabu8; PAYLOAD_LEN];
        // Runtime 自持 tokio reactor（D9）：criterion setup 无 tokio 上下文，可安全 new。
        let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));
        // 端到端功能验证（setup 内、不计时）：链必须完成全部连接且服务端全部 accept。
        {
            let chain = echo_client(addr, payload.clone(), N_CONNECTIONS);
            let v = runtime.run_blocking(chain).expect("echo 链执行失败");
            assert_eq!(v, Value::U64(0), "echo 链结果异常");
            assert_eq!(
                server.accepted(),
                N_CONNECTIONS,
                "服务端应 accept 恰好 N_CONNECTIONS 个连接"
            );
        }
        b.iter(|| {
            let chain = echo_client(addr, payload.clone(), N_CONNECTIONS);
            let v = runtime
                .run_blocking(chain)
                .expect("echo 链执行失败（测量中）");
            assert_eq!(v, Value::U64(0), "echo 链结果异常（测量中）");
            criterion::black_box(v)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_echo);
criterion_main!(benches);
