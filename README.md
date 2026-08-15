# Algeff

**把系统调用变成数据**——一个可组合、可重放、可撤销的确定性副作用框架（Rust，仅依赖 tokio）。

```text
传统写法：      代码直接调系统 → 副作用发生在哪一步？出错了怎么回滚？没法重放。
Algeff 写法：   先把"要做的事"写成一份数据蓝图（Action）→ 交给运行时执行 → 可撤销、可重放、可组合。
```

> ⚠️ **实验性项目（Research-grade）**
>
> 语义正确性已经过 5 轮「对抗测试 × 形式逻辑审计」验证（见[设计推导](#设计推导速览)），但**并行性能与部分边界语义仍有已知缺口**。版本 `0.1.0` 尚未发布，不提供稳定性承诺，API 可能变化。生产采用前请阅读[已知限制](#已知限制与边界)。

---

## 目录

1. [这是什么？30 秒理解](#这是什么30-秒理解)
2. [快速上手（5 分钟）](#快速上手5-分钟)
3. [核心概念：蓝图、执行、资源](#核心概念蓝图执行资源)
4. [常用模式速查](#常用模式速查)
5. [深入：确定性、重放与撤销机制](#深入确定性重放与撤销机制)
6. [设计推导速览](#设计推导速览)（工程师/研究者向）
7. [性能速览](#性能速览)
8. [已知限制与边界](#已知限制与边界)
9. [文档入口](#文档入口)

---

## 这是什么？30 秒理解

### 问题

系统交互（读文件、写网络、发信号……）是**指令**：执行了就回不去，出错了难收拾，想重跑一遍得重写逻辑。传统代码把副作用散落在函数调用里，测试要 mock、回滚要手写、重放要重演。

### 答案

Algeff 把所有系统交互编码成**不可变的数据结构（`Action`）**：

- **可组合**：小操作拼成大蓝图，像搭积木；
- **可缓存/可重放**：同一份蓝图跑 100 次，结果完全一致；
- **可撤销**：运行时自动记录"做过的操作"，出错可整体回滚；
- **确定性**：蓝图 + 输入 ⇒ 唯一结果，天然可测试、可验证。

### 心智模型：食谱 vs 做菜

| 传统编程 | Algeff |
| --- | --- |
| 边读食谱边做菜（指令即执行） | 先把食谱抄成一张卡片（蓝图） |
| 做坏了只能重新开始 | 卡片在手，随时"回到上一步"重来 |
| 换一个厨房就得重写食谱 | 同一张卡片换执行器就能跑（tokio / mock / 未来后端） |

---

## 快速上手（5 分钟）

### 1. 添加依赖

```toml
[dependencies]
algeff-core  = { path = "crates/algeff-core" }
algeff-std   = { path = "crates/algeff-std" }
algeff-macro = { path = "crates/algeff-macro" }   # 可选语法糖
tokio        = { version = "1", features = ["rt", "macros"] }
```

### 2. 第一个程序：纯蓝图

```rust
use algeff_core::prelude::*;
use algeff_macro::plan;

fn main() {
    // Runtime 在 tokio 上下文之外创建（D9 契约）
    let mut rt = Runtime::new(Box::new(algeff_std::TokioExecutor::new()));

    // 蓝图 = 数据：一段"先算 1+1，再算 2×3"的序列
    let blueprint = plan! {
        Action::Pure(Value::U64(1 + 1));
        Action::Pure(Value::U64(2 * 3));
    };

    let result = rt.run_blocking(blueprint);
    println!("{result:?}"); // Ok(Unit)：plan! 链收敛于 Unit
}
```

### 3. 真实文件 IO：写一个文件并读回来

```rust
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;

fn main() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let path = std::path::PathBuf::from("hello.txt");

    // Open（声明"要写这个路径"）→ 拿到 fd → Write → Read → Close
    let blueprint = Action::Syscall {
        op: DataOp::Open { path: path.clone(), flags: OpenFlags { read: true, write: true, ..Default::default() } },
        resources: vec![write_path(&path)],   // 类型安全资源声明
        next: Box::new(|v| {
            let fd = match v { Value::Fd(fd) => fd, other => panic!("期望 Fd，得到 {other:?}") };
            Action::Sequential {
                current: Box::new(Action::Syscall {
                    op: DataOp::Write { fd, data: b"hello algeff".to_vec() },
                    resources: vec![write_fd(fd)],
                    next: Box::new(Action::Pure),
                }),
                next: Box::new(|_| Action::Sequential {
                    current: Box::new(Action::Syscall {
                        op: DataOp::Seek { fd, offset: 0, whence: std::io::SeekFrom::Start(0) },
                        resources: vec![read_fd(fd)],
                        next: Box::new(Action::Pure),
                    }),
                    next: Box::new(|_| Action::Syscall {
                        op: DataOp::Read { fd, len: 64 },
                        resources: vec![read_fd(fd)],
                        next: Box::new(Action::Pure),
                    }),
                }),
            }
        }),
    };

    let v = rt.run_blocking(blueprint).unwrap();
    println!("读回: {:?}", v); // Ok(Bytes(b"hello algeff"))
}

// 资源声明辅助：告诉运行时"这个操作要怎么写/读/独占哪些资源"
fn write_path(p: &std::path::PathBuf) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Path(p.clone())).into_usage()
}
fn write_fd(fd: u64) -> ResourceUsage {
    TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(fd)).into_usage()
}
fn read_fd(fd: u64) -> ResourceUsage {
    TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd)).into_usage()
}
```

> 💡 每个操作都要声明它**怎么使用**资源（读/写/独占）——这是 Algeff 保证撤销安全和冲突检测的基础，见[核心概念](#核心概念蓝图执行资源)。

### 4. 用适配器少写样板

`algeff_std::adapters` 提供预包装的常用操作（返回可直接组合的 `Action`）：

```rust
use algeff_core::prelude::*;
use algeff_std::{TokioExecutor, adapters::{open_file, write, read, close}};

let blueprint = Action::Sequential {
    current: Box::new(open_file(path.clone(), OpenFlags { read: true, write: true, ..Default::default() })),
    next: Box::new(|v| {
        let fd = match v { Value::Fd(fd) => fd, other => panic!("{other:?}") };
        Action::Sequential {
            current: Box::new(write(fd, b"hello".to_vec())),
            next: Box::new(|_| Action::Sequential {
                current: Box::new(read(fd, 64)),
                next: Box::new(|_| close(fd)),
            }),
        }
    }),
};
```

适配器清单：`open_file` / `create_dir` / `read_dir` / `stat` / `unlink` / `open_tcp` / `accept` / `connect` / `read` / `write` / `close`（源码：`crates/algeff-std/src/adapters.rs`）。

---

## 核心概念：蓝图、执行、资源

### Action = 数据化的系统操作

所有操作都是 `Action` 枚举的一个节点，不可变、可自由嵌套。核心节点：

| 节点 | 作用 | 直觉 |
| --- | --- | --- |
| `Action::Pure(v)` | 返回一个值 | 终点/常量 |
| `Action::Syscall { op, resources, next }` | 执行一个系统调用，结果交给 `next` | 一步操作 |
| `Action::Sequential { current, next }` | 先做 `current`，把结果喂给 `next`（CPS 链） | 顺序组合 |
| `Action::Fork { left, right, combine }` | 两分支并行执行，结果合并 | 并行 |
| `Action::Catch { action, handler }` | 出错时交给 `handler` 处理 | try/catch |
| `Action::Timeout { action, duration, on_timeout }` | 超时走 `on_timeout` | 限时 |
| `Action::Scope { path, inner }` | 在临时作用域内执行，退出自动撤销 | 沙箱 |
| `Action::Replace { target }` | 先回滚全部已做操作，再执行 `target` | 一键撤销 |
| `Action::Choose { options }` | 运行时选择 | 分支 |

> 链式写法是 CPS（延续传递风格）：每一步的 `next` 是"接下来做什么"的闭包。可以理解为**把程序的控制流显式写成数据**。

### 蓝图与执行分离

```text
          构建蓝图（纯数据，无副作用）              执行（副作用发生在这里）
用户代码 ──────────────────────────→ Action ──→ Runtime::run_blocking(blueprint)
             可以缓存 / 复制 / 重放                 可撤销 / 可恢复
```

- 蓝图**不触发任何副作用**——测试时可以构造任意蓝图，不碰真实系统；
- 执行器可替换（当前是 tokio 后端）——同一蓝图未来可跑在 mock 或别的后端上；
- 每次执行都有独立的撤销栈：`rt.undo_stack()` 查看，`Replace` 一键回滚。

### 资源声明：类型安全的使用契约

每个 `Syscall` 必须声明它如何使用资源（`Vec<ResourceUsage>`）：

- `TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(fd))` — 读
- `TypedResource::<WriteOnly>::new_write(...)` — 写
- `TypedResource::<Owned>::new_owned(...)` — 独占（如 Close、Unlink）

运行时据此做两件事：**冲突检测**（两个并行分支同时写同一资源 → 按契约处理）与**撤销跟踪**（写过的文件才能恢复）。

---

## 常用模式速查

### 顺序链（宏版）

```rust
plan! { Action::Pure(Value::U64(1)); Action::Pure(Value::U64(2)); }
```

### 并行 Fork

```rust
let blueprint = fork! {
    Action::Pure(Value::U64(10)),   // 左分支
    Action::Pure(Value::U64(20)),   // 右分支
    // combine 默认 Unit；想合并结果就自定义：
    // |l, r| Action::Pure(Value::List(vec![l, r]))
};
```

两个分支若**静态冲突**（同一资源被两侧写/独占），运行时自动退化为顺序执行（确定性保证，D14/D17 契约）。

### 错误捕获

```rust
let blueprint = Action::Catch {
    action: Box::new(/* 可能失败的操作，如打开不存在的文件 */),
    handler: Box::new(|err| match err {
        SysError::NotFound => Action::Pure(Value::U64(0)),   // 文件不存在 → 返回 0
        other => Action::Pure(Value::U64(1)),                // 其他错误 → 1
    }),
};
```

### 超时

```rust
let blueprint = Action::Timeout {
    action: Box::new(/* 长操作 */),
    duration: std::time::Duration::from_millis(100),
    on_timeout: Box::new(Action::Pure(Value::U64(0))),
};
```

### 一键撤销（Replace）

```rust
// 前面做了一堆写操作 → 全部回滚到执行前状态，再执行 target
let blueprint = Action::Replace { target: Box::new(Action::Pure(Value::Unit)) };
```

### 完整 TCP echo 服务器骨架

```rust
use algeff_core::prelude::*;
use algeff_std::TokioExecutor;

fn main() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    let blueprint = Action::Syscall {
        op: DataOp::TcpBind { addr },
        resources: vec![],
        next: Box::new(|v| match v {
            Value::Fd(listener) => {
                // accept → 循环 TcpRead/TcpWrite（分片到达需循环读，见 tests/e2e.rs）
                Action::Syscall {
                    op: DataOp::TcpAccept { listener },
                    resources: vec![],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }
            }
            other => panic!("期望 Fd，得到 {other:?}"),
        }),
    };

    rt.run_blocking(blueprint).unwrap();
}
```

> 完整可运行示例与分片处理：`crates/algeff-std/tests/e2e.rs`（真实端到端，含 TCP 原生客户端对测）。

---

## 深入：确定性、重放与撤销机制

### 为什么确定性是免费的

副作用被集中到 `Runtime` 一处执行，因此：

- **执行轨迹只由蓝图决定**（`fork!` 冲突时自动顺序化 → 无调度器非确定性）；
- **同一蓝图 + 同一输入 ⇒ 同一结果**（100 轮重复执行逐字节一致，测试锁定）；
- 蓝图本身是数据 → 可以 `serde` 序列化、缓存、离线重放（pdr §1.1）。

### trackΓ / recoverΓ：撤销的记账本

运行时对每个已执行操作维护一张**跟踪表 Γ**：

- 写操作执行前记录"写前状态"（小文件 = 完整内容快照；大文件 = 长度/元数据策略）；
- `Replace`/`Scope` 退出触发 **recoverΓ**：按 LIFO 顺序把资源恢复到执行前状态；
- 恢复是**幂等且可嵌套**的——作用域嵌套时内层先恢复，外层接着恢复；
- 资源线性：`Write`/`Own` 恰好消费一次，双写冲突在运行时被拦截（A4 公理）。

### 句柄与 fd

- fd 是 `u64`，**单调递增、不复用**（D1 契约）——撤销栈里的旧 fd 永远不会被新资源顶替；
- `Write` 返回即意味着已 flush 到 OS（D-039 契约）——蓝图的"写成功"语义可观察、可断言。

### 深度守卫（重要！）

解释器递归处理嵌套蓝图，为防栈溢出设置了**深度上限 96**（2MB 线程栈下的安全阈值）：

- **左结合链**（如 `adapters::seq` 逐层嵌套）**≥ 97 步**返回 `Err(SysError::Other(105))`；
- **右结合 CPS**（`and_then` 风格）恒为深度 1，**无限制**；
- 错误可被 `Catch` 捕获；
- 1MB 主线程栈用户需自行抬栈（`/STACK` 链接参数或放到 spawn 线程），见 [RFC-11](spec/resource-notes.md)。

---

## 设计推导速览

（面向工程师/研究者。完整推导过程——动机、公理化、契约设计、审计证据——见 [`docs/src/derivation.md`](docs/src/derivation.md)（工程论文）与 `spec/` 形式化文档。）

| # | 推导步骤 | 一句话 | 证据 |
| --- | --- | --- | --- |
| 1 | 动机 | Unix 效应是"指令"而非"数据"→ 无法组合/缓存/重放 → 代数化 | `pdr.md` §0–§1 |
| 2 | 公理化 | A1–A7 七条公理（结合律/单位元/交换律/资源线性/分支隔离/撤销双态/无死锁） | `spec/axioms.md` |
| 3 | 命题 | P1–P5 五条可证明性质（幺半群/并行交换律/写隔离/撤销双态/无死锁） | `spec/proofs.md` |
| 4 | 契约冻结 | D1–D19 决策表 = 正确性承诺边界 | `contracts.md` |
| 5 | 关键决策 | Fd=u64 单调（D1）；Fork=静态冲突判定（D14/D17）；Replace=recover+clear（D10）；深度阈值 96 = 实测崩溃边界 104–108 留 8% 余量（D-052） | 决策链 + `spec/resource-notes.md` |
| 6 | 实现 | 三层 crate：core 解释器（13 节点）/ std tokio 执行器 / macro 语法糖 | `pdr.md` §15 |
| 7 | 验证分层 | 305 个测试函数（约 297 二进制 + 8 doc-test），44 个测试二进制 | `spec/verification-plan.md` |
| 8 | 对抗审计 ×5 | 120 个 E2E 测试，每轮独立发现（句柄活性/fd 区间/盲区/栈溢出…） | `spec/proof-obligations.md` |
| 9 | 数学审计 ×5 | P1/P2/P3/P5 收敛为「有效（附声明前提）」，P4 部分（RFC-05，阶段 3+ 已裁决） | `spec/proof-obligations.md` |
| 10 | 缺陷库 | RFC-05~11 全部登记；RFC-11（栈溢出）与 RFC-10（Windows 错误码）已修复 | `spec/resource-notes.md` §10 |
| 11 | 性能推导 | echo 103.1%（顺序≈原生）；并行读受 executor 锁串行化限制 | `perf/baseline-2026-08-15.txt` |
| 12 | 结论 | 语义正确性定案；并行性能/跨平台为已知开放面 | 本文档下方 |

---

## 性能速览

相对原生 tokio 的基准（`perf/baseline-2026-08-15.txt`，A7 批 2-4，>100% = 更慢）：

| 基准 | 对比 | 含义 |
| --- | --- | --- |
| echo（顺序 TCP 回显） | **103.1%** | 顺序路径 ≈ 原生 tokio，可放心用 |
| append（顺序追加） | 24.3% | 每步全量撤销记账 + flush 的开销，顺序小操作注意 |
| parallel_reads（并行读） | 366.2% | 并行受 executor 共享锁串行化（阶段 3+ R-6 重构目标） |
| shared_read（共享读） | 570.9% | 同上 |

结论：**顺序业务逻辑开销可忽略；并行 IO 密集负载请等 R-6 锁重构**。

---

## 已知限制与边界

| 限制 | 说明 | 状态 |
| --- | --- | --- |
| 并行吞吐 | executor 共享锁串行化所有物理 IO | 阶段 3+ R-6 重构 |
| 深度上限 | 左结合链 ≥97 步报 `Other(105)`（右结合无限制） | 已修复（RFC-11），见上 |
| Windows 错误码 | 已归一化到 POSIX 语义（EEXIST/EADDRINUSE/…） | 已修复（RFC-10） |
| Replace 句柄活性 | Replace 后旧 fd 的残留句柄仍可写（边界反例） | RFC-05，阶段 3+ 已裁决 |
| fd 区间溢出 | Fork 右分支极端分配 ~360 轮后可能溢出 u64 | RFC-06，阶段 3+ |
| Timeout 孤儿副作用 | 超时取消的并行分支副作用不可撤销 / 锁饥饿 | RFC-08/09，阶段 3+ |
| 闭包静态盲区 | `next` 闭包内构造的 `Syscall` 不参与静态冲突检测（合并时仍并入父） | 系统性声明前提，已文档化 |
| 1MB 主线程栈 | 深嵌套蓝图需抬栈（`/STACK` 或 spawn 线程） | 用户责任，已文档化 |

完整清单：`spec/resource-notes.md` §10。

---

## 文档入口

| 入口 | 面向 | 内容 |
| --- | --- | --- |
| [`docs/src/derivation.md`](docs/src/derivation.md) | 工程师/研究者 | 设计推导与验证过程（工程论文：动机→公理→契约→审计→收敛） |
| [`docs/`](docs/)（mdBook） | 所有人 | 概述/架构/示例/路线图：`mdbook build docs` 后打开 `docs/book/index.html` |
| `spec/proof-obligations.md` | 审计 | 证明义务登记表（A1–A7 × P1–P5 证据链闭环） |
| `pdr.md` | 规范 | 完整设计规范 v3.2（权威源） |
| `contracts.md` | 规范 | 冻结契约与决策 D1–D19 |
| `crates/algeff-std/tests/e2e.rs` | 开发者 | 真实端到端示例（文件/TCP/管道/撤销） |
| `scripts/release.sh` | 发布 | 发布预览（tag → dry-run → 顺序提示） |

开发：`cargo test --workspace`（全量 300+ 测试）、`cargo fmt --check`、`cargo clippy --workspace -- -D warnings`。CI 三平台（ubuntu/windows/macos）自动执行全部检查。
