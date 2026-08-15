# 使用示例

> 本页是 `pdr.md`（v3.2）§14「使用示例」的转述。权威源以 `pdr.md` 为准。

## 最小 Action 示例

所有系统交互都被编码为不可变的 `Action` 节点（CPS 风格，`next: NextFn`）。下面是一个「读取 fd 中数据」的最小蓝图：

```rust
use algeff_core::prelude::*;

// 读取 fd 中的字节：Syscall -> 拿到 Value -> 交给 next
fn read_once(fd: Fd) -> Action {
    Action::Syscall {
        op: DataOp::Read { fd, len: 1024 },
        resources: ResourceSet::default(),
        next: Box::new(|data| Action::Pure(data)),
    }
}
```

> 说明：`pdr.md` §14 示例风格为 `use algeff::prelude::*`（统一 façade crate，随发布提供）；当前工作区以 `algeff_core` 名义发布，`prelude` 由 `algeff-core` 提供。

## 完整蓝图示例（§14 转述）

`pdr.md` §14 给出一个 TCP 服务蓝图，展示主要控制流节点：

- `fork!`（展开为 `Action::Fork`）：并发处理两个客户端连接，`join!` 汇合；
- `scope!`（展开为 `Action::Scope`）：在 `/var/log/myapp` 作用域内写 `shutdown.log`，作用域退出时自动撤销；
- `sleep!`（展开为 `Action::Sleep`）：等待 1 秒；
- `replace!`（展开为 `Action::Replace`）：控制流跳转到 `shutdown_blueprint()`；
- 每个客户端处理函数用 `Resource::new_read/new_write/new_owned` 声明资源访问模式（类型安全资源包装），并以 `Action::Sequential` 链式组合 `Read -> Write -> Close`。

完整代码见 `pdr.md` §14。

## 可选宏（§13）

核心不依赖任何宏。以下宏仅为可选语法糖，位于独立 crate `algeff-macro`：

| 宏 | 职责 | 复杂度 |
| --- | --- | --- |
| `algeff::plan!` | 辅助构造 `Action::Sequential` 链 | 简单（~100 行展开） |
| `algeff::fork!` | 辅助构造 `Action::Fork` | 简单（~30 行展开） |
| `algeff::scope!` | 辅助构造 `Action::Scope` | 简单（~30 行展开） |
| `algeff::choose!` | 辅助构造 `Action::Choose` | 简单（~30 行展开） |

不再需要的机制：GADT 模拟、常量泛型区间检查、生命周期标记、线性类型模拟宏——所有线性逻辑由 Rust 原生所有权 + 运行时 `trackΓ` 模型共同保证。

## 文档入口

- `pdr.md` §14：权威示例代码。
- `crates/algeff-core`：rustdoc（本地运行 `cargo doc -p algeff-core --open`），`action.rs` 定义了全部 `Action` 节点与 `DataOp`（§2.2 共 39 个变体，含 `SendFile` 改名决策 D8），`resource.rs` 定义了 `Resource`/`TypedResource<M>` 与冲突检测。
