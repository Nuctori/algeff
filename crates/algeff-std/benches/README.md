# algeff-std 基准（benches）说明

A7 Integration & Perf 交付物（contracts.md §5 任务 A7 / pdr.md §19.4 工具链：criterion）。

本目录 4 个 criterion 基准（`harness = false`，自带 `criterion_main`）当前实现的是
**原生 tokio 参照列**（pdr.md §16 性能预期表中的「原生 tokio = 100%」列），是后续
Algeff 对比测量的基线。Algeff 对比列（`Runtime::new(TokioExecutor)` + `interpret`
执行 Action 链）待 A5（TokioExecutor）与 A2（interpret）合并后接入，接入计划见下文
§4。

正式基线数据：`perf/baseline-2026-08-15.txt`（由 `scripts/perf.sh` 生成）。

---

## 1. 基准清单与场景说明

| bench | 场景 | 输入规模 | 被测路径 | 中位时间（2026-08-15 基线） |
|---|---|---|---|---|
| `echo` | 本地 TCP echo（127.0.0.1:0） | 100 连接 × 每连接 1000 次 1KB 往返 | `TcpListener` accept + `TcpStream` write_all/read_exact（tokio 网络栈 + loopback） | 6.3986 s / iter |
| `parallel_reads` | 并行读 10 个不同文件（零冲突） | 10 × 1MB 文件，`tokio::join!` 10 路并发 | `tokio::fs::read` × 10（文件系统读路径，无锁） | 4.5676 ms |
| `shared_read` | 8 任务并发读同一 8MB 文件（只读共享） | 8 任务 × 各 1MB 区间，`spawn_blocking` + 位置读 | `Arc<std::fs::File>` + `read_at`/`seek_read`（零锁、无偏移竞争） | 2.2445 ms |
| `append` | 10 任务并行追加同一文件（顺序无关） | 10 任务 × 32 × 1KB = 320KB/样本 | `tokio::fs::OpenOptions::append` + write_all（O_APPEND 内核原子追加） | 7.1652 ms |

### 各 bench 要点

- **echo**：自建 multi-thread tokio runtime（每测量样本重建，不计时）。服务端 accept
  循环为每个连接派生一个 echo 任务，客户端在同一连接内循环 1000 次「写 1KB → 读回
  1KB」。连接预算设计见 §5（Windows 端口池约束，CTO 裁决）。
- **parallel_reads**：10 个不同文件 → `tokio::join!` 并发读，读取路径无共享状态
  （对应 pdr.md §16「Algeff 静态路径 ~100%（零锁）」的对照）。
- **shared_read**：`tokio::fs::File` 无位置读原语，故用跨平台基元 `Arc<std::fs::File>`
  + 按偏移位置读（unix `read_at` / windows `seek_read`），在 `spawn_blocking` 中执行，
  零锁、零偏移竞争（对应「读-读可并行」场景）。
- **append**：10 个任务各自以 append 模式打开同一文件并追加，顺序无关，单次写由内核
  保证原子（对应「并行追加同一文件（顺序无关）」场景）。

测量参数：IO 型基准 `sample_size=10`、`measurement_time=3s`（echo 为 30 / 3s，样本内
单次 iter 约 6.4s，criterion 提示「可增加目标时间」属良性，每样本恰 1 iter）。
所有 setup（tempfile 生成、runtime 构建）位于 `b.iter` 之外，不计入计时。

## 2. 与 pdr.md §16 性能预期表的对应关系

| pdr.md §16 场景 | 原生 tokio（100%） | 本目录基准 | Algeff 静态路径预期 |
|---|---|---|---|
| 网络 Echo（无共享资源） | 100% | `echo` | ~103% |
| 并行读取 10 个不同文件 | 100% | `parallel_reads` | ~100%（零锁） |
| 并行追加同一文件（顺序无关） | 100% | `append` | ~100% |
| 并行读取同一文件（只读共享） | 100% | `shared_read` | ~100%（读-读可并行） |

当前基线即为上表第二列（原生 tokio = 100%）的实测数值；第三列待 §4 接入后测得，
以 `(Algeff 中位 / 原生 tokio 中位) - 1` 计算百分比对照 pdr 预期。

## 3. 运行方式

```bash
scripts/perf.sh          # 冒烟（echo --test）→ 4 个 bench 逐个完整运行 → perf/baseline-<date>.txt
```

逐 bench 独立运行可单独执行：`cargo bench --bench echo|parallel_reads|shared_read|append`。
`--test` 后缀为 criterion 单次冒烟（不产生统计数据）。

## 4. Algeff 对比列接入计划（A5 + A2 合并后）

前置条件：A5 的 `TokioExecutor`（`crates/algeff-std/src/executor.rs`，实现
`SyscallExecutor`，Full 撤销策略）与 A2 的 `interpret`（`crates/algeff-core`，
`Runtime::run` 已存在）合并入 `main`；`adapters.rs` 预包装函数
（`open_tcp`/`read`/`write`/`close` 等）就绪。

接入骨架（每个 bench 一个 `tokio_native_*` 对照 + 一个 `algeff_*` 项）：

```rust,ignore
// 伪代码：对比项通用形态
let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
let mut runtime = algeff_core::Runtime::new(Box::new(algeff_std::TokioExecutor::new()));
b.iter(|| rt.block_on(runtime.run(algeff_action_chain())));
```

### 4.1 echo —— Action 链草图

每连接一次往返对应一个 `Syscall` 节点链（`next` 闭包把上一步结果传入下一步），
100 次连接 + 1000 次往返在同一 Action 链上展开（或外层用 `Sequential` 循环）：

```rust,ignore
// 单连接单次往返（1KB）：
let chain = open_tcp(addr)                                    // TcpConnect{addr}
    .then(|fd| write(fd, bytes!(1KB)))                        // TcpWrite{fd, data}
    .then(|_| read(fd, 1024))                                 // TcpRead{fd, len}
    .then(|_| close(fd));                                     // TcpClose{fd}

// 1000 次往返：Sequential 链重复上述片段（next 闭包拼接）
// 100 连接：外层再包一层 Sequential / 或 Fork 并行（见 §4.2）
let echo_chain = repeat(1000, |_| roundtrip_chain)
    .then(|_| connect_again());   // 示意：连接数为 100
```

被测路径：`TcpConnect/TcpWrite/TcpRead/TcpClose` DataOp → `TokioExecutor::execute`
（预期额外开销 = 解释器 trampoline + 资源检查 + Undo 记录，对应 pdr ~103%）。

### 4.2 parallel_reads —— Action 链草图

10 个不同文件互不冲突，可用 `Fork` 并行（combine 汇总字节数）：

```rust,ignore
let fork10 = fork!(
    read_file("file_0.bin"), read_file("file_1.bin"), ..., read_file("file_9.bin"),
    combine = |vals: Vec<Value>| sum_bytes(vals),
);
// 每路 read_file = Open{path, read} → Read{fd, 1MB} → Close{fd}
```

被测路径：`Open/Read/Close`（不同资源）→ 资源检查应零锁（pdr「~100%（零锁）」）；
`Fork` 调度 + combine。

### 4.3 shared_read —— Action 链草图

只读共享同一文件：所有分支声明 `Read` 同一资源（读-读可并行）：

```rust,ignore
let fork8 = fork!(
    read_region(shared_fd, 0..1MB), read_region(shared_fd, 1MB..2MB), ..., // 8 路
    combine = sum_bytes,
);
// 每路 read_region = Seek{fd} + Read{fd, 1MB}（或多次 Read）
```

被测路径：同一资源多路 `Read` 的并行调度 + 冲突矩阵判定（读-读无冲突）。

### 4.4 append —— Action 链草图

顺序无关的并行追加：A3 冲突矩阵中 `Append∥Append` 默认串行（契约 D6），
需要调用方显式声明顺序无关（或 Algeff 侧仍走串行路径测量——预期 ~100%/~105%）：

```rust,ignore
// 方案 A（顺序无关 opt-in，D6）：
let fork10 = fork!(
    append_chunk(shared_path, 32KB), ... ×10,
    combine = count_ok, // 结果顺序无关
);
// 每路 append_chunk = Open{path, append} → Write{fd, 32KB} → Close{fd}
```

被测路径：`Open/Write/Close` + 追加模式；对比原生 tokio 的 O_APPEND 直写，
Algeff 需在 `TokioExecutor::execute(Open{append})` 映射到同一 O_APPEND。

### 4.5 接入注意事项

- 每对比项与对应原生项使用**同一 runtime 线程池配置**与同一 setup（tempfile），
  仅替换被测执行路径（tokio 直接调用 vs `Runtime::run(Action)`）。
- 计算方式：`对比百分比 = algeff_median / tokio_native_median × 100%`，
  对照 pdr.md §16 预期列；超出预期则报回归（A7 介入优化）。
- echo 对比项同样受 §5 连接预算约束（总连接数 ≤ 数千级）。

## 5. Windows 端口池约束（echo 连接预算，CTO 裁决记录）

Windows 动态端口范围默认约 1024–15000（本机 13977 个），TIME_WAIT 保留约 120s，
可持续新连接速率 ≈ 116/s。echo 首版参数（30 样本 × 1000 连接）约需 3 万+ 连接，
必然触发 `WSAEADDRINUSE(10048)`。经 CTO 裁决改为：

- 每测量样本 100 个连接，每连接内循环 1000 次 1KB 往返（负载性质不变）；
- 单次 bench 总连接数 ≈ 样本数 × 100 ≈ 3.1k，实测峰值 TIME_WAIT 2447，安全。

后续新增任何高频建连基准，须先核对连接预算与 `netsh int ipv4 show dynamicport tcp`。

## 6. G3 门禁对照（contracts.md §4）

- bench 可运行：`cargo bench --bench echo|parallel_reads|shared_read|append` 全绿
  （本批实测 exit 0）。
- 文档齐备：本文档 + `perf/baseline-2026-08-15.txt`。
- CI yaml 校验：A8 交付（`.github/workflows/ci.yml`），不在本批范围。
