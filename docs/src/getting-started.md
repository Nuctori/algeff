# 快速开始（Getting Started）

> 目标：用一个**可编译运行**的最小示例跑通 Algeff 的核心链路。本页是 `pdr.md` §14 的简化版：只用手写 `Action` 链（不依赖宏），演示 `Pure → Sequential → Syscall(Open/Read/Write)` 的蓝图构造与执行。

## 依赖

新建一个二进制 crate，并在 `Cargo.toml` 中加入：

```toml
[dependencies]
algeff-core = { path = "crates/algeff-core" }
algeff-std  = { path = "crates/algeff-std" }
tokio = { version = "1", features = ["rt", "macros"] }
```

> 说明：`algeff-core` 只负责**蓝图**（不可变的 Action AST），不含任何物理 IO。物理执行需要 `algeff-std` 的 `TokioExecutor`（实现 `SyscallExecutor` 契约，pdr.md §12.2）。当前工作区中 `TokioExecutor::execute` 为 A5 交付中的 `todo!()` 桩，蓝图构造与编译不受影响，运行需待 A5 完成。

## 最小示例（main.rs）

```rust
use algeff_core::prelude::*;
use algeff_core::OpenFlags; // OpenFlags 未在 prelude 中，从 crate 根导出
use algeff_std::TokioExecutor;
use std::path::PathBuf;

/// 蓝图：Open -> Read -> Write -> Pure（手写 Sequential/Syscall 链）。
///
/// 纯蓝图只依赖 algeff-core，可自由组合、缓存、重放；
/// 物理执行由 algeff-std 的 TokioExecutor 在 run 时提供。
fn open_read_write(path: PathBuf) -> Action {
    Action::Sequential {
        current: Box::new(Action::Syscall {
            op: DataOp::Open {
                path,
                flags: OpenFlags {
                    read: true,
                    write: true,
                    create: true,
                    ..Default::default()
                },
            },
            resources: ResourceSet::default(),
            next: Box::new(|value| match value {
                Value::Fd(fd) => Action::Syscall {
                    op: DataOp::Read { fd, len: 1024 },
                    resources: ResourceSet::default(),
                    next: Box::new(move |data| Action::Syscall {
                        op: DataOp::Write {
                            fd,
                            data: match data {
                                Value::Bytes(b) => b,
                                _ => Vec::new(),
                            },
                        },
                        resources: ResourceSet::default(),
                        next: Box::new(|_| Action::Pure(Value::Unit)),
                    }),
                },
                _ => Action::Pure(Value::Unit),
            }),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    }
}

fn main() {
    // D9：Runtime::new 必须在 tokio 上下文之外调用（普通 fn main，而非 #[tokio::main]）
    let mut runtime = Runtime::new(Box::new(TokioExecutor::new()));

    // run 是 async：用用户自建 tokio runtime 阻塞执行
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let action = open_read_write(PathBuf::from("hello.txt"));
    let result = rt.block_on(runtime.run(action));
    println!("result = {result:?}");
}
```

## 结构解读（对照 pdr.md §2）

| 节点 | 含义 | 本示例中的角色 |
| --- | --- | --- |
| `Action::Syscall` | 一次系统效应调用，携带 `DataOp` + `ResourceSet` + CPS 续体 `next` | Open / Read / Write 各一次 |
| `Action::Sequential` | 顺序组合：先执行 `current`，结果交给 `next` | 包住整条链 |
| `Action::Pure(Value::Unit)` | 单位元，链的终点 | Write 之后的收尾 |

- **CPS 续体**：每个 `Syscall` 的 `next: Box<dyn FnOnce(Value) -> Action>` 拿到上一步的 `Value` 后返回下一个 `Action`，形成「数据驱动」的链。
- **资源声明**：`resources: ResourceSet` 声明本次效应访问的资源与模式（本示例为空集）。声明正确性由运行时线性检查保证（pdr.md §8）。
- **不依赖宏**：本示例不用 `plan!`/`fork!`/`scope!`（A4 交付的可选语法糖，见 `docs/example.md`）。

## 运行

```bash
cargo run
```

预期输出：`result = Ok(Unit)`（执行器完成整条链后返回最终值）。

> ⚠️ 当前 `TokioExecutor::execute` 仍是 `todo!()` 桩（A5 交付中），`cargo run` 会在物理执行处 panic。在此期间，示例可用于验证**蓝图 API 的编译正确性**与文档示例的一致性。

## 下一步

- 完整 TCP 服务蓝图（fork!/scope!/replace!/sleep!）：`docs/example.md`
- 架构与分层：`docs/architecture.md`
- 权威规范：`pdr.md` §14
