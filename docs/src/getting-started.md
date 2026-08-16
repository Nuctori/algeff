# 快速开始（Getting Started）

> 目标：用**可编译运行**的最小示例跑通 Algeff 的核心链路——「构造蓝图（数据）→ 交给运行时执行（副作用）」。本页全部示例与 `crates/algeff-std/tests/readme_examples.rs` / `crates/algeff-std/tests/docs_examples.rs` **逐字一致**（照抄可跑，改动测试即失败）。

## 1. 添加依赖

新建一个二进制 crate，并在 `Cargo.toml` 中加入：

```toml
[dependencies]
algeff-core  = { path = "crates/algeff-core" }
algeff-std   = { path = "crates/algeff-std" }
algeff-macro = { path = "crates/algeff-macro" }   # 可选语法糖
tokio        = { version = "1", features = ["rt", "macros"] }
```

> - `algeff-core` 只负责**蓝图**（不可变的 `Action` AST），不含任何物理 IO；
> - 物理执行需要 `algeff-std` 的 `TokioExecutor`（实现 `SyscallExecutor` 契约）；
> - `algeff-macro` 提供 `plan!` / `do_!` / `fork!` / `choose!` 等**可选语法糖**——核心不依赖任何宏，但教程推荐使用（这就是「最小例子像正常操作」的写法）。

## 2. 第一个程序：纯蓝图

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

要点：

- `plan!` 只是把一串 `Action` 值**组合成蓝图**，构造时不触发任何副作用；
- `run_blocking` 是副作用发生的地方（这里只有纯值，没有系统调用）；
- 蓝图是纯数据：同一份蓝图可以缓存、复制、重放（跑 100 次结果一致）。

## 3. 真实文件 IO：写一个文件并读回来（7 行）

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

这就是用户核心诉求的答案：**最小文件 IO 示例体只有 7 行，语法就是普通 Rust**。

### do_! 是什么？

`do_!` 把这段「正常操作」折叠成一条 CPS 链（`and_then` 嵌套）：**展开后的 `Action` 与手写链逐节点同构**——仍然是纯数据，可缓存、可重放、可撤销。三个机制缺一不可：

| 机制 | 本示例中的体现 |
| --- | --- |
| `dx` 预包装操作 | `dx::open` / `dx::write` / `dx::seek` / `dx::read` / `dx::close` 都是普通函数，返回 `Action` |
| `let` 值绑定 | `let fd = dx::open(...)` 把上一步的**结果值**（`Value::Fd`）绑定给 `fd`，词法贯穿整条链 |
| 资源自动推导 | `dx` 按 `DataOp` 推导每个操作的资源声明（`infer_usage` 模式表）：写 → `Write(path)`、关闭 → `Own(fd)`……无需手写 |

> 需要精确控制资源声明时用 `dx::syscall_with` **显式覆盖**（优先级：`syscall_with` 显式声明 > `infer_usage` 自动推导 > 空集）。更低层的 `algeff_std::adapters`（`open_file` / `read` / `write` / …）仍可用，它们返回 `Action`，可直接作 `do_!` 的语句。

## 4. 错误处理：错误短路上抛 + `dx::catch` 恢复

`do_!` 块内**任一步失败，错误沿链上抛**（后续语句不执行），`run*` 返回 `Err(SysError)` 而非 panic：

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
```

需要**恢复并继续**时，用 `dx::catch(action, handler)`——失败时 `handler` 收到真实的 `SysError`，返回替代 `Action` 继续执行，其值成为 catch 表达式的结果值；成功时值原样贯穿、`handler` 不执行（`missing` 为不存在的路径，`fb` 为可创建的替代路径）：

```rust
let blueprint = do_! {
    let fd = dx::catch(
        dx::open(&missing, OpenFlags { read: true, ..Default::default() }),
        move |e| {
            // handler: SysError → 替代 Action
            assert!(matches!(e, SysError::NotFound));
            dx::open(fb.clone(), open_rw_create())
        },
    );
    dx::write(&fd, b"recovered".to_vec());
    dx::seek(&fd, 0, std::io::SeekFrom::Start(0));
    let data = dx::read(&fd, 64);
    dx::close(&fd);
    data
};
```

其中 `open_rw_create` 是通用 flags 辅助：

```rust
fn open_rw_create() -> OpenFlags {
    OpenFlags { read: true, write: true, create: true, ..Default::default() }
}
```

> 注意 `handler` 需 `+ Send + 'static`：捕获的外部数据要 `clone` 后 `move` 进闭包（示例中的 `fb`）。`dx::catch` 是 `Action::Catch` 的 dx 级便捷包装，语义与手写完全一致。

## 5. 并行：`fork!`

```rust
let blueprint = fork! {
    left: Action::Pure(Value::U64(10)),    // 左分支
    right: Action::Pure(Value::U64(20)),   // 右分支
};
```

- 宏版 `fork!` 的合并函数固定为「忽略两侧值，收敛为 `Value::Unit`」；需要自定义合并（如拼成 `Value::List`）时手写 `Action::Fork { left, right, combine }`；
- 两个分支若**静态冲突**（同一资源被两侧写/独占），运行时自动退化为顺序执行（确定性保证，D14/D17 契约）；
- 真实并发文件写入示例（含 `do_!` 与 `plan!` 混合）见 [使用示例](example.md)。

## 6. 深度守卫（重要！）

解释器递归处理嵌套蓝图，为防栈溢出设置了**深度上限 64**（2MB 线程栈下的安全阈值）。对你的代码有两条直接结论：

- **左结合链**（如 `adapters::seq` 逐层 `then` 嵌套）**≥ 65 步**返回 `Err(SysError::Other(105))`（「嵌套资源耗尽」语义）；
- **右结合 CPS**（`and_then` 风格——`do_!` 正是这种）恒为深度 1，**无限制**：`do_!` 块写多少条语句都不触发深度守卫。

错误可被 `Catch` 捕获。1MB 主线程栈用户需自行抬栈（`/STACK` 链接参数或放到 `spawn` 线程），详见 [FAQ](faq.md)。

## 7. 下一步

| 入口 | 内容 |
| --- | --- |
| [使用示例](example.md) | do_! 版完整示例：TCP echo、锁/信号、与 plan!/fork! 混合 |
| [DX 语法糖层设计](dx-design.md) | do_! 宏 + dx 模块的设计论证（为什么这种语法不破坏「蓝图 = 数据」） |
| [FAQ](faq.md) | 深度守卫 / undo 语义 / 锁重入 / TcpShutdown 空声明 / 1MB 栈 |
| [架构](architecture.md) | 三层 crate 分层与解释器/执行器 |
| [设计推导与验证过程](derivation.md) | 工程论文：动机 → 公理 → 契约 → 审计 → 收敛 |
| README | 常用模式速查（Catch / Timeout / Replace / 已知限制） |
