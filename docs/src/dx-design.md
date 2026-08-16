# DX 语法糖层设计（迭代 1→3-A5）：`do_!` 宏 + `dx` 模块

> 范围：迭代 1 提供「顺序命令式」语法糖；迭代 2 深化 DX 层——
> 错误处理 `dx::catch`、DataOp → dx 包装全覆盖（munmap/send_file/dup2 补齐）、
> 新示例测试；迭代 3-A5（本迭代）——`plan!` continuation 闭包改 `move`
> （do_! 块可直接内嵌）+ usage builder（`dx::usage`/`dx::fd_usage`/
> `dx::path_usage`/`dx::pid_usage`/`dx::signal_usage`）。控制流
> （`plan!`/`fork!`/`scope!`/`choose!`）与错误处理
> （`Action::Catch`）沿用既有机制；**不引入任何新 Action 节点**。冻结面
> （algeff-core 的 `action.rs`/`error.rs`/`syscall.rs`/`lib.rs`、
> `contracts.md`、`pdr.md`）零改动——本设计全部落在 A5 域纯增量层
> （`algeff-macro` 新宏 + `algeff-std::dx` 新模块 + 示例测试）。

## 1. 目标与不变量

### 1.1 目标

1. **最小文件 IO 示例 ≤15 行、接近普通 Rust**：`do_!` 块内直书
   `let fd = open(...); write(fd, ...); let data = read(fd, 64); close(fd); data`，
   无手动 `Box::new`/闭包嵌套/资源声明样板；
2. **资源 usage 自动推导**：op → usage 模式表（`infer_usage`），
   显式声明可覆盖（`syscall_with`）；
3. **不碰冻结面**：核心零改动，宏为纯 AST 构造；
4. **错误处理命令式化**（迭代 2）：`do_!` 内用 `dx::catch(action, |e| …)`
   捕获链内错误——`Action::Catch` 的 dx 级便捷包装，运行时语义零改动；
5. **DataOp 全覆盖**（迭代 2）：每个 `DataOp` 都有 `dx` 预包装操作
   （迭代 2 补齐 munmap/send_file/dup2）；
6. **组合免预构建 + 显式覆盖免样板**（迭代 3-A5）：`plan!` continuation 闭包
   为 `move`（do_! 块可直接内嵌）；`dx::usage*` 便捷构造 `ResourceUsage`
   （`syscall_with` 配套，见 §3.5）。

### 1.2 三条不变量（哲学底线，与 pdr.md §八/§13 对齐）

1. **蓝图 = 不可变数据**——`do_!` 展开产物只是 `Action` 值的 CPS 构造
   （`and_then` 链），不引入新节点、不做任何类型魔法；
2. **资源声明显式化**——展开后的每个 `Action::Syscall` 仍携带完整
   `ResourceSet`（冲突检测 / 撤销跟踪的根基）。自动推导 = 默认值，
   显式声明永远可以覆盖；
3. **执行与构造分离**——`dx` 模块只构造 `Action`（不触碰真实系统），
   执行仍由 `Runtime` + `TokioExecutor` 完成。

## 2. 为什么这种语法不破坏「蓝图 = 数据」承诺

### 2.1 展开产物与手写链逐节点同构

`do_!` 只是把语句序列折叠为 `and_then` 嵌套：

```text
do_! {                         手写等价（局部 CPS）
  let fd = open(p, f);   ⟹     and_then(open(p,f), move |fd| …)
  write(fd, d);          ⟹       and_then(write(fd,d), move |_| …)
  let data = read(fd,64)⟹        and_then(read(fd,64), move |data| …)
  close(fd);             ⟹          and_then(close(fd), move |_| …)
  data                   ⟹            Pure(data)
}
```

运行时 `interpret` 看到的 AST 与手写 `Action::Sequential` 链**完全相同**
（`and_then` 对非 Pure 的包装就是 `Sequential`）。解释器、冲突检测、撤销
跟踪、Fork 资源分裂全部无感知——「蓝图是可检查、可重放、可撤销的数据」
这一承诺在宏前后不变（见 §6 测试证据）。

### 2.2 不引入新节点、不做类型魔法

- 无新 `Action` 变体（冻结面零改动，`git diff` 可证）；
- 无 GADT 模拟、常量泛型区间、生命周期标记（pdr.md §13 已排除的机制）；
- 值传递经闭包参数：`let x = e;` → `move |x| { … }`，fd 等中间值
  词法贯穿整条链——与手写 CPS 一致，无隐式全局状态。

### 2.3 构造期零副作用

`dx::open` 等只做 `DataOp` 装箱与资源推导，不触碰真实文件系统；
同一蓝图可多次执行。宏是**语法糖**而非运行时行为差异。

## 3. 资源自动推导的设计

### 3.1 op → usage 模式表（`infer_usage`，与 adapters 手工声明对齐）

| DataOp | 推导的 ResourceUsage |
| --- | --- |
| `Open { flags }` | `flags.write` → Write(path)；否则 `flags.append` → Append(path)；否则 Read(path) |
| `Read` / `Seek` | Read(fd) |
| `Write` | Write(fd) |
| `Close` | Own(fd)（终结：唯一持有者释放） |
| `Stat` / `ReadDir` | Read(path) |
| `Mkdir` / `Chmod` / `Chown` / `Truncate` / `Rmdir` | Write(path) |
| `Unlink` | Own(path)（终结） |
| `Rename` | Write(from) + Write(to) |
| `TcpRead` / `UdpRecvFrom` | Read(fd) |
| `TcpWrite` / `UdpSendTo` / `Dup` | Write(fd) |
| `TcpShutdown` | 空集（半关闭不终结 fd，A4 不消费；显式声明用 `syscall_with` 覆盖） |
| `TcpAccept` | Read(listener) |
| `Kill` | Write(pid) |
| `SendSignal` | 空集（Signal 全局资源无仲裁层；二次发送允许——SIGTERM→SIGKILL 优雅停机模式，A4 不消费，成败由物理层决定） |
| `Wait` | Own(pid) |
| `MutexLock` | 空集（锁 id 仲裁在 executor 层经 arbiter（D16/R-1），与 A4 线性域正交；声明 Write(Fd(id)) 会被 A4 至多消费一次导致 lock→unlock→再 lock 二次必拒——空声明对齐 adversarial_r2 攻击面 2b，重入由 arbiter 占坑释放保证） |
| `MutexUnlock` | 空集（同上；executor 层释放 arbiter 占坑，幂等） |
| `Mmap` | prot.write → Write(path) : Read(path) |
| `SendFile` | Write(out) + Read(input) |
| `Dup2` | Write(new_fd) + Read(old_fd) |
| `TcpBind` / `TcpConnect` / `UdpBind` / `PipeOpen` / `Spawn` / `Munmap` / `GetTime` | 空集 |

### 3.2 为什么「自动推导 = 默认值，显式覆盖是公民」

- **覆盖优先级**：`syscall_with(op, resources)` > `infer_usage` > 空集。
  调用方也可完全手写 `Action::Syscall`，或使用 `adapters` 的类型状态包装；
  显式声明用迭代 3-A5 的 usage builder 便捷构造（§3.5）；
- **价值**：消灭「每个 op 重复声明资源」的样板，且缺省安全
  （`close`/`unlink`/`wait` 默认 Own 终结，不会误声明成 Read 泄漏线性标记）；
- **可审计**：推导表是纯函数 `&DataOp → ResourceSet`，逐条可测
  （`tests/dx_examples.rs::infer_usage_table` 全表断言）。

### 3.3 边界：运行时句柄为何空集

`TcpBind`/`TcpConnect`/`UdpBind`/`PipeOpen`/`Spawn` 返回**运行时才分配**的新
句柄——静态无法声明尚不存在的 fd/pid 资源（pdr.md §18 用户责任域）。
空集意味着「新句柄不参与冲突检测」，与 `adapters` 既有行为一致，
不改变契约语义。

### 3.4 错误处理语法：`dx::catch`（迭代 2 新增）

`do_!` 内的错误路径经 `dx::catch(action, handler)` 表达——`Action::Catch`
的 dx 级便捷包装（纯构造，无新节点、运行时零改动）：

```rust
let blueprint = do_! {
    let fd = dx::catch(
        dx::open(&missing, flags),
        move |e| {
            // handler: SysError → 替代 Action
            assert!(matches!(e, SysError::NotFound));
            dx::open(fb.clone(), create_flags)
        },
    );
    dx::write(&fd, b"recovered".to_vec());
    // …
};
```

语义（与手写 `Action::Catch { action, handler }` 完全一致，见 runtime.rs）：

- `action` 失败时，运行时把 `SysError` 交给 `handler`（`FnOnce(SysError) -> Action`），
  handler 返回的 Action 继续执行，其值成为 catch 表达式的结果值；
- `action` 成功时值**原样贯穿**，`handler` 不执行；
- Catch **仅处理错误值**，不触碰撤销栈（recover 语义仍在 Replace/recover 路径）——
  需整体回滚时用 `Replace` 包裹，二者职责不变；
- handler 需 `+ Send + 'static`：捕获的外部数据 clone 后 `move` 进闭包；
  替代 Action 可为 `do_!`/`plan!`/`dx::unit()` 等任意返回 `Action` 的表达式。

### 3.5 显式覆盖的样板消解：usage builder（迭代 3-A5 新增）

`syscall_with` 的 `ResourceSet` 手写 `ResourceUsage { resource, mode }`（或
adapters 风格 `TypedResource::new_*.into_usage()`）样板由便捷构造消解：

```rust
use algeff_core::{AccessMode, Resource};
use algeff_std::dx;

// 通用构造：dx::usage(Resource, AccessMode)
let _ = dx::usage(Resource::Fd(3), AccessMode::Read);
// 按资源类型便捷：fd/path/pid/signal + mode 组合
let _ = dx::fd_usage(3, AccessMode::Read);
let _ = dx::path_usage("/var/log/app", AccessMode::Write);
let _ = dx::pid_usage(42, AccessMode::Own);
let _ = dx::signal_usage(AccessMode::Write);
```

与 `syscall_with` 组合（显式覆盖自动推导）：

```rust
let a = dx::syscall_with(
    DataOp::Open { path: "/x".into(), flags },
    vec![dx::path_usage("/custom", AccessMode::Read)], // 覆盖 Write(path) 默认推导
);
```

设计要点：纯构造函数（无新类型/无状态），与 `infer_usage` 同表同模式；
`path_usage` 接受 `impl AsRef<Path>`（`&str`/`PathBuf` 直传）。
**MutexLock/MutexUnlock/SendSignal 同样空集，但理由不同**：
- 锁 id 的互斥语义由 executor 层 arbiter 动态仲裁（D16/R-1，占坑⟺持锁）；
  A4 线性域（Write 每资源至多消费一次）与之正交——若声明 `Write(Fd(id))`，
  lock→unlock→再 lock 第二次必被 A4 拒（R7 发现：arbiter release 只清占坑
  不清 A4 consumed）。空声明后同 id 并行争用仍由 arbiter 序列化
  （败者 WouldBlock，adversarial_r2 攻击面 2b），显式声明场景（攻击面 2a）
  不受影响（`syscall_with` 覆盖入口保留）；
- `SendSignal` 的 Signal 全局资源无仲裁层且语义上可重复（SIGTERM→SIGKILL
  优雅停机），A4 不应拒绝二次发送——空声明，成败由物理层决定。

## 4. 方案对比

| 方案 | 描述 | 优点 | 缺点 | 结论 |
| --- | --- | --- | --- | --- |
| A：纯函数 + 手写 CPS | `dx::open(...)` 返回 Action，用户手动嵌套闭包/`and_then` | 零新增、无解析器 | 样板深（每步一个闭包），可读性差，违背「接近普通 Rust」 | 基线，被 B 取代 |
| **B：`do_!` 宏折叠语句 → and_then 链** | 命令式块语法，`let` 绑定值经闭包贯穿 | 展开与手写逐节点同构；接近普通 Rust；无类型魔法；冻结面零改动 | 需维护 syn 2.x 解析（已修复：`Block` 定界符不入输入流、`LocalInit` 结构体化、`Spanned` 导入） | **采用** |
| C：自定义 DSL + 新 Action 节点 | 引入 While/For/Try 等节点 | 表达力更强 | 破坏冻结面；冲突检测/撤销/推导全表需重设计 | 拒绝（需 RFC 另立） |
| D：op 级操作宏 `open!`/`write!`/`read!`/`close!` | 运算符风格调用 | 更短 | `write!` 与 `std::write!` 名冲突是硬伤（同块内 shadowing 破坏 fmt 宏）；函数调用已足够接近普通 Rust | 拒绝（迭代 1；如未来需要可改名 `dx_write!` 等再议） |
| E：async/await 式 EDSL | `let fd = dx::open(p).await?` | 最像普通 Rust | 引入非确定性/隐式运行时依赖；与「蓝图 = 数据、可静态分析」承诺冲突 | 拒绝 |
| F：`catch!` 宏 vs `dx::catch` 函数（迭代 2） | `catch(action, \|e\| handler)` → `Action::Catch`（函数包装） | 与 ops-as-functions 决策一致（方案 D）；无宏解析器负担；do_! 内天然可读、可嵌套 | 比宏多一层函数名 | **采用 `dx::catch`**（`catch!` 宏同理被方案 D 理由拒绝：宏职责最小化，函数调用已足够接近普通 Rust） |

## 5. 选型理由

1. **为什么是宏而非函数**：把「语句序列 + 值绑定 + 尾值」折叠成 CPS 链是
   **语法**职责，Rust 函数无法在调用点重组语句——这正是宏存在的唯一理由
   （pdr.md §八：核心不依赖宏，宏仅为可选语法糖）。
2. **为什么 ops 是函数而非操作宏（方案 D）**：`dx::open`/`dx::write` 等是
   普通函数调用，已经「接近普通 Rust」；操作宏除了缩短几个字符外，还带来
   `write!` 与 `std::write!` 的命名冲突风险。宏职责最小化：`do_!` 只做
   CPS 折叠，ops 只做「op 装箱 + 自动推导」。
3. **为什么 `let` 仅支持标识符/通配**：保持展开确定性（模式绑定
   `let (a, b) = …` 会引入解构语义，留给后续迭代）；`let _ = e;` 与
   `e;` 都表示丢弃，语法上照顾普通 Rust 习惯。
4. **为什么命名为 `do_`**：`do` 是 Rust 保留关键字（loop 语法），
   下划线后缀沿用社区惯例。
5. **为什么 `plan!` 内嵌 `do_!` 无需预构建（迭代 3-A5 变更）**：`plan!` 的
   continuation 闭包已改 `move`（与 `do_!`/`choose!` 一致）——`do_!` 块（含
   `move` 闭包捕获外部路径/fd）可**直接内嵌** plan! 任意位置：首元素借用于
   构造期（`current` 求值即释放），后续元素体内引用的外部变量被 **move 闭包
   移入链**（被消费，如需保留先 clone）。修复前 continuation 闭包非 `move`，
   非首元素内的 `&外部路径` 被借用捕获 → `Box<dyn FnOnce + Send>` 要求
   `'static` → E0597（`path does not live long enough`），被迫预构建
   `let act = do_!{…}; plan!{ act; … }`。变更安全性：任何修复前能编译的
   程序行为不变（元素值本就 move 进 `current`，仅「借引用非 'static 局部」
   的失败用例改为「move 消费」成功），既有测试全绿为门槛（§6）。
   预构建写法仍然成立（值组合形态，见 `plan_move_keeps_value_composition_working`）。

## 6. 验证证据（tests/dx_examples.rs，15 项全绿）

| 测试 | 验证的承诺 |
| --- | --- |
| `minimal_file_io_roundtrip` | 7 行 do_! 块真实执行：写 → seek → 读回 → 尾值 Bytes，物理落盘 |
| `error_path_propagates_without_panic` | 打开缺失文件 → `SysError::NotFound` 沿链上抛，非 panic |
| `error_mid_chain_skips_tail` | 链中部失败，尾表达式不求值（整链 Err 短路） |
| `plan_wraps_do_blocks` | plan! 组合两个 do_! 阶段（mkdir + 写文件），真实执行 |
| `do_block_embeds_plan_and_choose` | do_! 内嵌 choose! 分支 + plan! 子步骤 |
| `infer_usage_table` | op → usage 模式表全量断言（含空集边界与 Mutex 读写降级） |
| `inferred_resources_flow_through_chain` | 推导的资源随 CPS 链流动（Open→Write(path)，Write/Close→Fd(fd)） |
| `explicit_override_wins_over_inference` | `syscall_with` 显式声明覆盖自动推导 |
| `empty_block_and_discard_statement` | 空块 → `Pure(Unit)`；`let _ = e;` 丢弃语句 |

迭代 2 新增：

| 测试 | 验证的承诺 |
| --- | --- |
| `file_ops_comprehensive_roundtrip` | 一个 do_! 覆盖 Mkdir/Open/Write/Seek/Read/Stat/Close/Unlink，尾值组合 List，真实执行 + stat 断言 + unlink 生效 |
| `tcp_bind_accept_skeleton` | bind → accept → close 骨架：结构断言（TcpBind 空集资源、accept 值流贯穿）+ bind 真实执行（accept 需并发客户端，不跑） |
| `catch_handles_error_and_continues` | Open 缺失文件失败 → handler 收 NotFound → 替代 Action 继续，值贯通（真实落盘） |
| `catch_success_passthrough_skips_handler` | 成功路径 handler 不执行、值原样贯穿（误执行会因 expect_fd panic 而测试失败） |
| `do_plan_fork_mix` | do_! 内嵌 plan!（声明式子步骤）+ fork!（并发分叉写不同文件），真实执行双文件落盘 |
| `remaining_op_wrappers_construct_syscall_nodes` | munmap/send_file/dup2 包装构造 + infer_usage 自动推导资源断言 |

迭代 3-A5 新增：

| 测试 | 验证的承诺 |
| --- | --- |
| `plan_embeds_do_blocks_directly` | plan! continuation 闭包改 move 后，两个 do_! 块（第二个引用外部路径——修复前 E0597）直接内嵌 plan!，真实执行落盘 |
| `plan_move_keeps_value_composition_working` | 预构建 Action 值作为 plan! 元素的旧形态不受影响（回归保障） |
| `usage_builder_constructs_usage` | `dx::usage`/`dx::fd_usage`/`dx::path_usage`/`dx::pid_usage`/`dx::signal_usage` 便捷构造 + 与 `syscall_with` 组合显式覆盖 |

## 7. 已知限制与后续迭代

- `let` 不支持解构模式（标识符/通配之外报编译期错误）；
- 分支/循环体多条语句需嵌套 `do_!` 块（或未来 While/For 节点——需 RFC）；
- **错误处理现经 `dx::catch`（§3.4）**：recover（撤销栈）语义仍在 Replace/recover
  路径，Catch 只处理错误值——职责边界以 runtime.rs 为准；
- **do_! 链内外部路径引用的捕获规则**：引用出现在闭包体内即被 `move` 闭包捕获，
  需 `'static`——链内多处使用的路径请 clone 后 move（示例见
  `file_ops_comprehensive_roundtrip` 的 `fc`）；
- **plan!/fork! 内嵌 do_! 直接书写可用**（迭代 3-A5：plan! continuation 闭包为
  move，do_! 块可直接内嵌任意位置，外部捕获被移入链——如需保留先 clone）；
  预构建 Action 值写法仍成立（值组合形态，二者等价）；
- **plan! 元素值被忽略**：plan! 链收敛为 `Pure(Unit)`，需值传递时用 do_! 作外层
  （见 `do_plan_fork_mix`）；
- **TCP accept 真实执行需并发客户端**：骨架测试只构造 + bind 执行；完整 echo
  链路见 `tests/e2e.rs`；
- **操作保持函数形态（方案 D 重申，迭代 2 未引入 `open!` 等操作宏）**：`write!`
  与 `std::write!` 名冲突是硬伤，函数调用已足够接近普通 Rust；
- 空集推导的操作（TcpBind/Spawn 等）其新句柄声明留给用户责任域，文档已注明；
- **显式覆盖的样板由 usage builder 消解（迭代 3-A5）**：`dx::syscall_with` 的
  `ResourceSet` 用 `dx::usage`/`dx::fd_usage`/`dx::path_usage`/`dx::pid_usage`/
  `dx::signal_usage` 便捷构造，无需手写 `ResourceUsage { resource, mode }` 或
  `TypedResource::new_*.into_usage()`。

## 文档入口

- 本设计：`docs/src/dx-design.md`
- 运行时支撑与推导表：`crates/algeff-std/src/dx.rs`（rustdoc，含 `dx::catch` 用法）
- 宏展开语义：`crates/algeff-macro/src/lib.rs`（`do_!` rustdoc）
- 示例测试：`crates/algeff-std/tests/dx_examples.rs`
