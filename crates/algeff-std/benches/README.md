# algeff-std 基准（benches）说明

A7 Integration & Perf 交付物（contracts.md §5 任务 A7 / pdr.md §19.4 工具链：criterion）。

本目录 8 个 criterion 基准（`harness = false`，自带 `criterion_main`）：4 个**原生 tokio
参照列**（pdr.md §16 性能预期表的「原生 tokio = 100%」列，批 2）+ 4 个 **Algeff 对比列**
（`Runtime::new(TokioExecutor)` + `interpret` 执行 Action 链，批 3 交付；批 4 在 D17
Fork 并行化后完成复测）。对比列的实现与现状见 §4。

正式基线数据：`perf/baseline-2026-08-15.txt`（由 `scripts/perf.sh` 生成，批 4 已刷新）。

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

## 4. Algeff 对比列（批 3 交付，批 4 D17 复测现状）

前置条件（已满足）：A5 的 `TokioExecutor`（`crates/algeff-std/src/executor.rs`）与
A2 的 `interpret`/`Runtime`（`crates/algeff-core`）已合并入 main；每个对比 bench 为
一个 `tokio_native_*` 参照臂 + 一个 `algeff_*` 臂（同 setup、同参数、同 runtime 线程池
配置）。通用形态：

```rust,ignore
// 对比项通用形态（伪代码）
let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
let mut runtime = algeff_core::Runtime::new(Box::new(algeff_std::TokioExecutor::new()));
b.iter(|| rt.block_on(runtime.run(algeff_action_chain())));
```

### 4.1 echo（无 Fork，与并行化无关）

每连接恰 1 次 1KB 往返（A4 资源线性使多次往返不可表达，CTO 裁决等负载双对比项）：
`TcpConnect → TcpWrite → TcpRead(读满 1KB) → TcpClose` 链，N 连接经 `Sequential` 串接。
被测路径：`TcpConnect/TcpWrite/TcpRead/TcpClose` → `TokioExecutor::execute`
（解释器 trampoline + 资源检查 + Undo 记录）。批 4 复测 103.1%（pdr 预期 ~103%，批 3
为 100.0%，每样本 1 iter 的 ±10% 噪声内无回归）。

### 4.2 parallel_reads（D17 并行路径 + 预开模式）

10 个不同文件零冲突 → `Action::Fork`（平衡二叉 Fork 树，combine 汇总字节数），
`fork_conflict` 判定 can_parallel=true → D17 `run_fork_parallel` 真并行（线程级）。

**批 4 蓝图调整（fd 分配碰撞）**：D17 并行分支各自从父 registry 的 `next_fd` 克隆
分配 fd 号，而共享执行器（`TokioExecutor`）的 `files` 等句柄映射以 fd 为键——分支内
各自 Open 会分配相同 fd 并互相覆盖（读错文件/EOF，功能断言失败）。故 Algeff 臂改为：
父 registry **顺序预开** 10 文件 → Fork 内每叶 `Read(fd_i, 1MB) → Close(fd_i)`
（不同 fd，互不覆盖）；被测负载不变（10 个不同文件的并发读，零锁零共享），预开的
Open 成本仍在 iter 计时内。

**批 4 实测**：366%（vs 批 3 D14 顺序 340%，无实质改善）。D17 并行路径已触发但读仍
串行化——执行器互斥锁（`Arc<Mutex<Box<dyn SyscallExecutor + Send>>>`）在 `exec_via` 中对整个
`execute`（含物理 IO await）持锁，跨分支 Syscall 全部串行。锁边界收窄属 A2 域
（runtime.rs），A7 不改；复测目标回归 pdr §16 ~100%。

### 4.3 shared_read（同 fd 游标读，D17 并行无收益）

只读共享同一文件：`Open{read}` 后 Fork 8 路 `Read(fd, 1MB)`（读-读冲突矩阵兼容 →
can_parallel=true）。批 4 实测 571%（vs 批 3 D14 顺序 308%，**回归**）：并行路径触发
但两层串行化——执行器互斥锁（同 4.2）+ 同 fd 游标读共用 `files[fd]` 文件互斥锁与
游标（`op_read` 按序推进；即使锁边界收窄也不并行，需位置读原语——执行器层/A5 域
待办）。spawn + current-thread runtime 创建开销叠加，实测反超 D14 顺序基线。

### 4.4 append（D6 串行路径）

顺序无关的并行追加：A3 冲突矩阵 `Append∥Append` 默认串行（契约 D6），Algeff 臂走
D6 默认串行路径（`Open{append} → Write → Close` × 10 任务顺序展开，CTO 批准）。
批 4 复测 24.3%（批 3 为 29.4%，无回归）——小负载下串行追加显著快于原生 10 路
并行追加（tokio::spawn + 同步开销主导）。opt-in 并行
（`can_parallel_with append_order_insensitive`）留待后续基准驱动。

### 4.5 接入注意事项

- 每对比项与对应原生项使用**同一 runtime 线程池配置**与同一 setup（tempfile），
  仅替换被测执行路径（tokio 直接调用 vs `Runtime::run(Action)`）。
- 计算方式：`对比百分比 = algeff_median / tokio_native_median × 100%`，
  对照 pdr.md §16 预期列；超出预期则报回归（A7 介入优化）。
- echo 对比项同样受 §5 连接预算约束（总连接数 ≤ 数千级）。
- **D17 并行蓝图注意**：并行分支内不要 Open（fd 分配碰撞，见 4.2）；需要分支内
  独立打开资源的场景待 A2/A5 解决（fd 分配去冲突 / 位置读）。

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
