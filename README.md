# Algeff

**把系统调用变成数据**——一个可组合、可重放、可撤销的确定性副作用框架（Rust，仅依赖 tokio）。

```text
传统写法：      代码直接调系统 → 副作用发生在哪一步？出错了怎么回滚？没法重放。
Algeff 写法：   先把"要做的事"写成一份数据蓝图（Action）→ 交给运行时执行 → 可撤销、可重放、可组合。
```

> ⚠️ **实验性项目（Research-grade）**
>
> 语义正确性已经过 5 轮「对抗测试 × 形式逻辑审计」验证（见[设计推导](#设计推导速览)），但**并行性能与部分边界语义仍有已知缺口**。当前版本 `0.1.0`（早期发布），不提供稳定性承诺，API 可能变化。生产采用前请阅读[已知限制](#已知限制与边界)。

---

## 目录

1. [这是什么？](#这是什么)
2. [快速上手](#快速上手)
3. [它解决什么：传统 IO 的四个痛点](#它解决什么传统-io-的四个痛点)
4. [核心概念：蓝图、执行、资源](#核心概念蓝图执行资源)
5. [常用模式速查](#常用模式速查)
6. [深入：确定性、重放与撤销机制](#深入确定性重放与撤销机制)
7. [设计推导速览](#设计推导速览)（工程师/研究者向）
8. [性能速览](#性能速览)
9. [已知限制与边界](#已知限制与边界)
10. [文档入口](#文档入口)

---

## 这是什么？

### 问题

系统交互（读文件、写网络、发信号……）是**指令**：执行了就回不去，出错了难收拾，想重跑一遍得重写逻辑。传统代码把副作用散落在函数调用里，测试要 mock、回滚要手写、重放要重演。

### 答案

Algeff 把所有系统交互编码成**不可变的数据结构（`Action`）**：

- **可组合**：小操作拼成大蓝图，像搭积木；
- **可缓存/可重放**：同一份蓝图跑 100 次，结果完全一致；
- **可撤销**：运行时自动记录"做过的操作"，出错可整体回滚；
- **确定性**：蓝图 + 输入 ⇒ 唯一结果，天然可测试、可验证。

---

## 快速上手

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
use algeff_core::OpenFlags;
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

fn main() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let path = std::path::PathBuf::from("hello.txt");
    let flags = OpenFlags { read: true, write: true, create: true, ..Default::default() };

    // 语法就是普通 Rust：open/write/seek/read/close 直书，
    // fd 经 let 绑定贯穿，资源声明由 dx 按操作自动推导
    let blueprint = do_! {
        let fd = dx::open(&path, flags);
        dx::write(&fd, b"hello algeff".to_vec());
        dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
        let data = dx::read(&fd, 64);
        dx::close(&fd);
        data // 尾表达式 = 链的最终值
    };

    let v = rt.run_blocking(blueprint).unwrap();
    println!("读回: {:?}", v); // Ok(Bytes(b"hello algeff"))
}
```

`do_!` 把这段「正常操作」折叠成一条 CPS 链（`and_then` 嵌套）：**展开后的 `Action` 与手写链逐节点同构**，仍然是纯数据——可缓存、可重放、可撤销。设计论证见 [`docs/src/dx-design.md`](docs/src/dx-design.md)。

<details>
<summary>对比：同样的事，手写 CPS 链（旧写法）长这样——do_! 展开后就是它</summary>

```rust
use algeff_core::{Action, DataOp, OpenFlags, ReadOnly, ResourceInner, ResourceUsage, Runtime, TypedResource, Value, WriteOnly};
use algeff_std::TokioExecutor;

fn main() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let path = std::path::PathBuf::from("hello.txt");

    // Open（声明"要写这个路径"）→ 拿到 fd → Write → Seek → Read
    // 注意：每个 next 闭包都要加 move（NextFn 是 'static 的）
    let blueprint = Action::Syscall {
        op: DataOp::Open {
            path: path.clone(),
            flags: OpenFlags { read: true, write: true, create: true, ..Default::default() },
        },
        resources: vec![write_path(&path)], // 类型安全资源声明
        next: Box::new(move |v| {
            let fd = match v {
                Value::Fd(fd) => fd,
                other => panic!("期望 Fd，得到 {other:?}"),
            };
            Action::Sequential {
                current: Box::new(Action::Syscall {
                    op: DataOp::Write { fd, data: b"hello algeff".to_vec() },
                    resources: vec![write_fd(fd)],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
                next: Box::new(move |_| Action::Sequential {
                    current: Box::new(Action::Syscall {
                        op: DataOp::Seek { fd, offset: 0, whence: std::io::SeekFrom::Start(0) },
                        resources: vec![read_fd(fd)],
                        next: Box::new(|_| Action::Pure(Value::Unit)),
                    }),
                    next: Box::new(move |_| Action::Syscall {
                        op: DataOp::Read { fd, len: 64 },
                        resources: vec![read_fd(fd)],
                        // 最后一个操作把结果透传出去（next 收到 Read 的 Bytes）
                        next: Box::new(|v| Action::Pure(v)),
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

</details>

> ✅ 本示例与下文示例由 `crates/algeff-std/tests/readme_examples.rs` 编译验证，保证照抄可跑。

> 💡 每个操作都自动携带**资源声明**（`dx` 按 `DataOp` 推导：写 → `Write(path)`、关闭 → `Own(fd)`……；需要精确控制时用 `dx::syscall_with` 覆盖，见 §4）——这是 Algeff 保证撤销安全和冲突检测的基础，见[核心概念](#核心概念蓝图执行资源)。

### 4. 资源声明：自动推导与显式覆盖

`dx` 的每个操作按 `DataOp` **自动推导**资源声明（`infer_usage` 模式表：写 → `Write(path)`、关闭 → `Own(fd)`……）。需要精确控制时，用 `dx::syscall_with` **显式覆盖**：

```rust
use algeff_core::prelude::*;
use algeff_core::OpenFlags;
use algeff_std::dx;

// 自动推导：write 模式 → Write(path)
let auto = dx::open("hello.txt", OpenFlags { write: true, ..Default::default() });

// 显式覆盖：自定义资源声明完全替换默认推导（syscall_with 优先于 infer_usage）
let custom = dx::syscall_with(
    DataOp::Open {
        path: "hello.txt".into(),
        flags: OpenFlags { write: true, ..Default::default() },
    },
    vec![ResourceUsage {
        resource: Resource::Path("/custom".into()),
        mode: AccessMode::Read,
    }],
);

assert!(matches!(auto, Action::Syscall { .. }));
assert!(matches!(custom, Action::Syscall { .. }));
```

> 覆盖优先级：`syscall_with` 显式声明 > `infer_usage` 自动推导 > 空集。更低层的预包装操作 `algeff_std::adapters`（`open_file` / `read` / `write` / `close` / `stat` / `unlink` / `open_tcp` / `connect` …）仍可用——它们返回 `Action`，可直接作 `do_!` 的语句（源码：`crates/algeff-std/src/adapters.rs`）。

---

## 它解决什么：传统 IO 的四个痛点

### 痛点 1：原子性——写到一半失败怎么办？

**传统**：`open → write → close`，任何一步失败（磁盘满、权限、崩溃）文件就是半写状态；要原子更新得手动临时文件 + fsync + rename + 删除，漏一步就是脏数据。

**Algeff**：蓝图是数据，运行时自动追踪每个操作的逆操作；任一步失败，**已做的副作用自动回滚**：

```rust
use algeff_core::prelude::*;
use algeff_core::OpenFlags;
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

fn main() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let path = std::path::PathBuf::from("atomic.txt");
    std::fs::write(&path, "before").unwrap();

    // Open 必须带 read: true——Write 的撤销需要写前读原内容；
    // 只写句柄无法构造撤销 → 运行时报错（语义真回归，不静默降级）。
    let blueprint = do_! {
        let fd = dx::open(&path, OpenFlags {
            read: true, write: true, create: true, ..Default::default()
        });
        dx::write(&fd, b"new content".to_vec());
        // 若这里某步失败 → 前面的 Write 自动回滚，文件仍是 "before"
        dx::close(&fd);
        Value::Unit
    };

    rt.run_blocking(blueprint).unwrap();
    println!("写入后: {:?}", std::fs::read_to_string(&path).unwrap());
}
```

### 痛点 2：可重放——想再跑一遍？

**传统**：复制粘贴整个函数；手动管理状态重置、mock 时间。

**Algeff**：蓝图是不可变数据，同一份蓝图跑多少次结果一致（每轮 `truncate` 打开 + 写入 + 读回验证）：

```rust
let path = std::path::PathBuf::from("replay.txt");
for i in 0..3 {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    // do_! 生成 'static 闭包：块内引用 path 需每轮 clone owned 值
    let p = path.clone();
    let v = rt.run_blocking(do_! {
        let fd = dx::open(&p, OpenFlags {
            read: true, write: true, create: true, truncate: true, ..Default::default()
        });
        dx::write(&fd, format!("round {i}").into_bytes());
        dx::close(&fd);
        let fd2 = dx::open(&p, OpenFlags { read: true, ..Default::default() });
        let data = dx::read(&fd2, 64);
        dx::close(&fd2);
        data
    }).unwrap();
    println!("第 {} 次: {:?}", i + 1, v); // Ok(Bytes(b"round 0")) / round 1 / round 2
}
```

### 痛点 3：一键撤销——做了 N 步想回滚？

**传统**：手动写 N 个补偿逻辑（恢复原内容、删临时文件、关句柄），还容易漏。

**Algeff**：`Replace` 节点 = 回滚执行段全部副作用 + 重新开始：

```rust
let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
std::fs::write(&path, "original").unwrap();

// 第一步：做有副作用的操作（Write 是 use 语义，可多次）
rt.run_blocking(do_! {
    let fd = dx::open(&path, OpenFlags { read: true, write: true, ..Default::default() });
    dx::write(&fd, b"temporary".to_vec());
    dx::close(&fd);
    Value::Unit
}).unwrap();
println!("副作用后: {:?}", std::fs::read_to_string(&path).unwrap()); // "temporary"

// 第二步：Replace 一键回滚 → 文件恢复 "original"
rt.run_blocking(Action::Replace {
    target: Box::new(Action::Pure(Value::Unit)),
}).unwrap();
println!("撤销后: {:?}", std::fs::read_to_string(&path).unwrap()); // "original"
```

> 撤销能力是**类型化的**：每个操作属于 Identity（无副作用）/ Invertible（可逆，逆构造失败 → 报错）/ NonInvertible（不可逆，如 unlink/udp/管道写——Replace 闸门显式拒绝，**绝不静默假回滚**）。

### 痛点 4：组合性——小蓝图拼大蓝图

**传统**：函数调用耦合在控制流里，无法拆分复用、重排、缓存。

**Algeff**：每个操作是独立的数据片段，自由组合成一条大蓝图一次执行：

```rust
let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
let dir = std::path::PathBuf::from("batch");

let mut steps: Vec<Action> = Vec::new();
for i in 0..3 {
    let p = dir.join(format!("file_{i}.txt"));
    steps.push(do_! {
        let fd = dx::open(&p, OpenFlags { read: true, write: true, create: true, ..Default::default() });
        dx::write(&fd, format!("content {i}").into_bytes());
        dx::close(&fd);
        Value::Unit
    });
}

// 顺序组合成一条大蓝图（Sequential 链），一次执行
let combined = steps.into_iter().reduce(|acc, s| {
    Action::Sequential { current: Box::new(acc), next: Box::new(move |_| s) }
}).unwrap();
rt.run_blocking(combined).unwrap();
```

### 痛点 5：重试安全——非幂等效应不重复执行

**传统**：消息队列消费/网络超时重试/异步重调，扣款、发邮件、库存扣减这类非幂等效应，失败重试极易重复伤害。要幂等得业务层手写去重表。

**Algeff**：`dx::idempotent(key, action)` 给副作用段挂**全局幂等键**——键 COMMITTED 未 REVERTED 时重试返回缓存结果，**不重新执行**（运行时状态机去重，无需业务手写）：

```rust
let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
let path = std::path::PathBuf::from("charge.txt");

// 带幂等键的副作用段（如扣款/发邮件/建表）：同 key 只真正执行一次
// （do_! 生成 'static 闭包：path 需 clone 进闭包；Action 不可 Clone，重试重新构造）
let make_charge = || {
    let p = path.clone();
    dx::idempotent("charge:order-42", do_! {
        let fd = dx::open(&p, OpenFlags { read: true, write: true, create: true, ..Default::default() });
        dx::write(&fd, b"charged".to_vec());
        dx::close(&fd);
        Value::U64(42)
    })
};

// 重试 3 次：只有第一次真正执行（键 COMMITTED → 后续返回缓存）
for _ in 0..3 {
    let v = rt.run_blocking(make_charge()).unwrap();
    println!("结果: {:?}", v); // 42 × 3（后两次来自缓存，副作用未重复）
}
```

**恰好一次**：副作用被 `Replace`/`Scope` 撤销后键置 REVERTED，允许未来重新执行——"这个副作用在整个生命周期中只发生一次"（如插件热重载不重复建表初始化）。

---

## 核心概念：蓝图、执行、资源

### Action = 数据化的系统操作

所有操作都是 `Action` 枚举的一个节点，不可变、可自由嵌套。核心节点：

| 节点 | 作用 | 直觉 |
| -------------------------------------------------- | -------------------------------- | --------- |
| `Action::Pure(v)` | 返回一个值 | 终点/常量 |
| `Action::Syscall { op, resources, next }` | 执行一个系统调用，结果交给 `next` | 一步操作 |
| `Action::Sequential { current, next }` | 先做 `current`，把结果喂给 `next`（CPS 链） | 顺序组合 |
| `Action::Fork { left, right, combine }` | 两分支并行执行，结果合并 | 并行 |
| `Action::Catch { action, handler }` | 出错时交给 `handler` 处理 | try/catch |
| `Action::Timeout { action, duration, on_timeout }` | 超时走 `on_timeout` | 限时 |
| `Action::Scope { base, inner, next }` | 在临时作用域内执行，退出自动撤销 | 沙箱 |
| `Action::Replace { target }` | 先回滚全部已做操作，再执行 `target` | 一键撤销 |
| `Action::Choose { cond, then_branch, else_branch }` | 条件分支 | if/else |

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

### do_! 命令式链：错误短路上抛

`do_!` 块内**任一步失败，错误沿链上抛**（后续语句不执行），`run*` 返回 `Err(SysError)` 而非 panic；需要恢复时用 `Action::Catch` 包住整个链：

```rust
use algeff_core::prelude::*;
use algeff_core::OpenFlags;
use algeff_macro::do_;
use algeff_std::dx;

let blueprint = do_! {
    let fd = dx::open("no_such.txt", OpenFlags { read: true, ..Default::default() });
    dx::write(&fd, b"x".to_vec());
    let data = dx::read(&fd, 64);
    dx::close(&fd);
    data // 打开失败时链首即 Err，不会执行到这里
};

let result = rt.run_blocking(blueprint); // rt 见 §2
assert!(matches!(result, Err(SysError::NotFound)));

// 需要恢复：Catch 包住 do_! 链，NotFound 走 handler 返回 0
let guarded = Action::Catch {
    action: Box::new(do_! {
        let fd = dx::open("no_such.txt", OpenFlags { read: true, ..Default::default() });
        let data = dx::read(&fd, 64);
        data
    }),
    handler: Box::new(|err| match err {
        SysError::NotFound => Action::Pure(Value::U64(0)),
        other => Action::Pure(Value::U64(1)),
    }),
};
assert_eq!(rt.run_blocking(guarded).unwrap(), Value::U64(0));
```

### 并行 Fork

```rust
let blueprint = fork! {
    left: Action::Pure(Value::U64(10)),    // 左分支
    right: Action::Pure(Value::U64(20)),   // 右分支
};
```

> 宏版 `fork!` 的合并函数固定为「忽略两侧值，收敛为 `Value::Unit`」。需要自定义合并（如取两侧结果拼成 List）时，手写 `Action::Fork { left, right, combine }` 即可。

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
use algeff_macro::do_;
use algeff_std::dx;
use algeff_std::TokioExecutor;

fn main() {
    let mut rt = Runtime::new(Box::new(TokioExecutor::new()));
    let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // 绑定监听 → accept 一个连接（新句柄运行时分配，资源自动推导为空集）
    let blueprint = do_! {
        let listener = dx::open_tcp(addr);
        let _conn = dx::accept(&listener);
        Value::Unit
    };

    // 无客户端连接时 TcpAccept 会一直阻塞——用 Timeout 包裹避免挂死
    let bounded = Action::Timeout {
        action: Box::new(blueprint),
        duration: std::time::Duration::from_millis(50),
        on_timeout: Box::new(Action::Pure(Value::Unit)),
    };
    rt.run_blocking(bounded).unwrap();
}
```

> 完整可运行示例与分片处理：`crates/algeff-std/tests/e2e.rs`（真实端到端，含 TCP 原生客户端对测）。

---

## 深入：确定性、重放与撤销机制

### 为什么确定性是免费的

副作用被集中到 `Runtime` 一处执行，因此：

- **执行轨迹只由蓝图决定**（`fork!` 冲突时自动顺序化 → 无调度器非确定性）；
- **同一蓝图 + 同一输入 ⇒ 同一结果**（100 轮重复执行逐字节一致，测试锁定）；
- 蓝图本身是数据 → 天然可缓存、可复制、可离线重放（序列化支持是阶段 3 设计目标，尚未实现——`Action` 含闭包，不可直接 serde）；

### trackΓ / recoverΓ：撤销的记账本

运行时对每个已执行操作维护一张**跟踪表 Γ**，并按**撤销能力**（代数角色）分类：

- **Identity（单位元）**：read/stat/close/dup/bind 等无外部可观察副作用——天然可回归，无需逆操作；
- **Invertible（可逆）**：写/seek/rename/open(create|truncate)/sendfile 等——执行前记录"写前状态"（写前读原内容/游标位置），`Replace`/`Scope` 退出触发 **recoverΓ**：按 LIFO 顺序恢复；
- **NonInvertible（不可逆）**：unlink/udp/管道写/kill 等投递/消费/删除语义——无逆元，Replace 闸门拒绝回滚（显式报错，不静默假回滚）；
- **无法构造逆 → Err**：如只写句柄写前读失败——运行时报错而非静默降级（语义真回归原则）。

### 句柄与 fd

- fd 是 `u64`，**单调递增、不复用**（D1 契约）——撤销栈里的旧 fd 永远不会被新资源顶替；
- `Write` 返回即意味着已 flush 到 OS（D-039 契约）——蓝图的"写成功"语义可观察、可断言；
- **A4 use/move 拆分**：`Write` 是 use 语义不限次数（每次写独立逆操作，LIFO 撤销仍正确）；`Own`（Close）是 move 语义恰好终结一次，之后任何 usage 拒绝；互斥锁防重入由 A7 仲裁器保证。

### 深度守卫（重要！）

解释器递归处理嵌套蓝图，为防栈溢出设置了**深度上限 64**（2MB 线程栈下的安全阈值，迭代 1 由 96 下调——取消传播协议帧膨胀后的复测裁决）：

- **左结合链**（如 `adapters::seq` 逐层嵌套）**≥ 65 步**返回 `Err(SysError::Other(105))`（迭代 1 阈值 96→64 后口径）；
- **右结合 CPS**（`and_then` 风格）恒为深度 1，**无限制**；
- 错误可被 `Catch` 捕获；
- 1MB 主线程栈用户需自行抬栈（`/STACK` 链接参数或放到 spawn 线程），见 [RFC-11](spec/resource-notes.md)。

---

## 设计推导速览

（面向工程师/研究者。完整推导过程——动机、公理化、契约设计、审计证据——见 [`docs/src/derivation.md`](docs/src/derivation.md)（工程论文）与 `spec/` 形式化文档。）

| #   | 推导步骤    | 一句话                                                                                                   | 证据                             |
| --- | ------- | ----------------------------------------------------------------------------------------------------- | ------------------------------ |
| 1   | 动机      | Unix 效应是"指令"而非"数据"→ 无法组合/缓存/重放 → 代数化                                                                  | `pdr.md` §0–§1                 |
| 2   | 公理化     | A1–A7 七条公理（结合律/单位元/交换律/资源线性/分支隔离/撤销双态/无死锁）                                                            | `spec/axioms.md`               |
| 3   | 命题      | P1–P5 五条可证明性质（幺半群/并行交换律/写隔离/撤销双态/无死锁）                                                                 | `spec/proofs.md`               |
| 4   | 契约冻结    | D1–D19 决策表 = 正确性承诺边界                                                                                  | `contracts.md`                 |
| 5   | 关键决策    | Fd=u64 单调（D1）；Fork=静态冲突判定（D14/D17）；Replace=recover+clear（D10）；深度阈值 64（D-052 初版 96 → 迭代 1 取消传播帧膨胀复测裁决 64） | 决策链 + `spec/resource-notes.md` |
| 6   | 实现      | 三层 crate：core 解释器（13 节点）/ std tokio 执行器 / macro 语法糖                                                   | `pdr.md` §15                   |
| 7   | 验证分层    | 435 个测试函数（约 422 二进制 + 13 doc-test），51 个测试二进制 + 3 个 doc-test 运行；数学层（cost.rs 效应开销代数：Grade 区间幺半群 / EffectCost 三元组积 / CostBudget 阈值 / for_op 度量派生）已由 `tests/cost_algebra.rs` 29 个定律测试 + `cost_audit.rs` 3 个行为测试全覆盖（D-104 落点 a 运行时记录路线） | `spec/verification-plan.md`    |
| 8   | 对抗审计 ×8 | 203 个 E2E 测试（R1-R6=171 + R7=15 + R7AB=4 + R8=13，逐二进制 `--list` 实测），每轮独立发现（句柄活性/fd 区间/盲区/栈溢出/macOS errno/线性残留…）                                                               | `spec/proof-obligations.md`    |
| 9   | 数学审计 ×8 | P1/P2/P3/P5「有效（附声明前提）」，P4「有效（附范围声明）」——R7-A 已核销（TakeHandleGuard）、R7-B 部分核销（ForkJoinMerge，耗尽路径残余登记）；P3 物理层 make_mut 阶段 3 登记                                                    | `spec/proof-obligations.md`    |
| 10  | 缺陷库     | RFC-05~11 全部登记；RFC-11（栈溢出）与 RFC-10（Windows 错误码）已修复                                                    | `spec/resource-notes.md` §10   |
| 11  | 性能推导    | echo 103.1%（顺序≈原生）；并行读受 executor 锁串行化限制                                                               | `perf/baseline-2026-08-15.txt` |
| 12  | 结论      | 语义正确性定案；并行性能/跨平台为已知开放面                                                                                | 本文档下方                          |

---

## 性能速览

相对原生 tokio 的基准（`perf/baseline-2026-08-15.txt`，A7 批 2-4，&gt;100% = 更慢）：

| 基准 | 对比 | 含义 |
| ------------------- | ---------- | ----------------------------------- |
| echo（顺序 TCP 回显） | **103.1%** | 顺序路径 ≈ 原生 tokio，可放心用 |
| append（顺序追加） | 39.1% | D-039 写后 flush 契约（Write 返回 ⇔ OS 落盘）的诚实成本；R6 复测（基线 24.3% 为修复前旧数，见 `perf/baseline-r6-2026-08-16.txt`） |
| parallel_reads（并行读） | 264.4% | R-6 快照通道复测（366.2% 为修复前旧数；分支独占执行器 + per-fd 锁，物理 IO 移出共享锁） |
| shared_read（共享读） | 380.6% | 同上（570.9% 为修复前旧数） |

结论：**顺序业务逻辑开销可忽略；并行 IO 密集负载经 R-6 快照通道已从 3.7×/5.7× 改善至 2.6×/3.8×**——§13 分解审计定位残余主因：Open/Close 执行器物理路径（~0.44ms/次，占比 ~80%）+ 共享游标语义成本（shared_read，正解=ReadAt 原语）；Fork 机制 ~0.08ms/次、解释器帧 <1% 均非主项。优化方向见 resource-notes §13。

---

## 已知限制与边界

| 限制 | 说明 | 状态 |
| ------------- | ------------------------------------------ | ---------------- |
| 并行吞吐 | executor 共享锁串行化所有物理 IO | 阶段 3+ R-6 重构 |
| 深度上限 | 左结合链 ≥65 步报 `Other(105)`（右结合无限制） | 已修复（RFC-11），见上 |
| Windows 错误码 | 已归一化到 POSIX 语义（EEXIST/EADDRINUSE/…） | 已修复（RFC-10） |
| Replace 句柄活性 | Replace 后旧 fd 的残留句柄仍可写——已闭环（R7 翻转测试断言 NotFound，义务表 A4 RFC-05 闭合） | RFC-05 已闭合 |
| fd 区间溢出 | Fork 右分支极端分配 ~360 轮后可能溢出 u64 | RFC-06，阶段 3+ |
| Timeout 孤儿副作用 | 超时取消的并行分支副作用：墙钟路径已修（取消广播+回滚，668b7ed）；残余=R7-B 耗尽路径（分支自身已持锁 + 阻塞 IO）、嵌套 Timeout 不复合取消、VC 墙钟通道无广播（R7-A 已核销） | RFC-08/09 墙钟已修；R7-A/B 迭代 3 核销 |
| 管道半端 | 未 Dup 的管道在 Fork 分支内 IO 被拒绝（Arc 共享与 `Arc::get_mut` 冲突） | RFC-07，阶段 3+ |
| 闭包静态盲区 | `next` 闭包内构造的 `Syscall` 对静态冲突检测不可见（运行时仅收集 `current` 的资源声明；执行时仍会真实执行并并入父撤销栈） | 系统性声明前提，已文档化 |
| 1MB 主线程栈 | 深嵌套蓝图需抬栈（`/STACK` 或 spawn 线程） | 用户责任，已文档化 |

完整清单：`spec/resource-notes.md` §10。

---

## 文档入口

| 入口 | 面向 | 内容 |
| -------------------------------------------------- | ------- | ----------------------------------------------------------- |
| [`docs/src/derivation.md`](docs/src/derivation.md) | 工程师/研究者 | 设计推导与验证过程（工程论文：动机→公理→契约→审计→收敛） |
| [`docs/`](docs/)（mdBook） | 所有人 | 概述/架构/示例/路线图：`mdbook build docs` 后打开 `docs/book/index.html` |
| `spec/proof-obligations.md` | 审计 | 证明义务登记表（A1–A7 × P1–P5 证据链闭环） |
| `pdr.md` | 规范 | 完整设计规范 v3.2（权威源） |
| `contracts.md` | 规范 | 冻结契约与决策 D1–D19 |
| `crates/algeff-std/tests/e2e.rs` | 开发者 | 真实端到端示例（文件/TCP/管道/撤销） |
| `scripts/release.sh` | 发布 | 发布脚本（tag 检查 → dry-run → 真实发布 → 验证/回滚提示） |

开发：`cargo test --workspace`（全量 300+ 测试）、`cargo fmt --check`、`cargo clippy --workspace -- -D warnings`。CI 三平台（ubuntu/windows/macos）自动执行全部检查。
