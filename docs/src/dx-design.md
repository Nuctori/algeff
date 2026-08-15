# DX 语法糖层设计（迭代 1）：`do_!` 宏 + `dx` 模块

> 范围：迭代 1 只提供「顺序命令式」语法糖。控制流（`plan!`/`fork!`/`scope!`/
> `choose!`）与错误处理（`Action::Catch`）沿用既有机制；**不引入任何新 Action
> 节点**。冻结面（algeff-core 的 `action.rs`/`error.rs`/`syscall.rs`/`lib.rs`、
> `contracts.md`、`pdr.md`）零改动——本设计全部落在 A5 域纯增量层
> （`algeff-macro` 新宏 + `algeff-std::dx` 新模块 + 示例测试）。

## 1. 目标与不变量

### 1.1 目标

1. **最小文件 IO 示例 ≤15 行、接近普通 Rust**：`do_!` 块内直书
   `let fd = open(...); write(fd, ...); let data = read(fd, 64); close(fd); data`，
   无手动 `Box::new`/闭包嵌套/资源声明样板；
2. **资源 usage 自动推导**：op → usage 模式表（`infer_usage`），
   显式声明可覆盖（`syscall_with`）；
3. **不碰冻结面**：核心零改动，宏为纯 AST 构造。

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
| `TcpWrite` / `UdpSendTo` / `TcpShutdown` / `Dup` | Write(fd) |
| `TcpAccept` | Read(listener) |
| `Kill` / `SendSignal` | Write(pid)（+ Write(Signal)） |
| `Wait` | Own(pid) |
| `MutexLock` | Write(Fd(id)) |
| `MutexUnlock` | Read(Fd(id))（对齐 adversarial_r2 攻击面 2a：Write 会被 A4 每资源至多消费一次，unlock 必须降为 Read） |
| `Mmap` | prot.write → Write(path) : Read(path) |
| `SendFile` | Write(out) + Read(input) |
| `Dup2` | Write(new_fd) + Read(old_fd) |
| `TcpBind` / `TcpConnect` / `UdpBind` / `PipeOpen` / `Spawn` / `Munmap` / `GetTime` | 空集 |

### 3.2 为什么「自动推导 = 默认值，显式覆盖是公民」

- **覆盖优先级**：`syscall_with(op, resources)` > `infer_usage` > 空集。
  调用方也可完全手写 `Action::Syscall`，或使用 `adapters` 的类型状态包装；
- **价值**：消灭「每个 op 重复声明资源」的样板，且缺省安全
  （`close`/`unlink`/`wait` 默认 Own 终结，不会误声明成 Read 泄漏线性标记）；
- **可审计**：推导表是纯函数 `&DataOp → ResourceSet`，逐条可测
  （`tests/dx_examples.rs::infer_usage_table` 全表断言）。

### 3.3 边界：运行时句柄为何空集

`TcpBind`/`TcpConnect`/`UdpBind`/`PipeOpen`/`Spawn` 返回**运行时才分配**的新
句柄——静态无法声明尚不存在的 fd/pid 资源（pdr.md §18 用户责任域）。
空集意味着「新句柄不参与冲突检测」，与 `adapters` 既有行为一致，
不改变契约语义。

## 4. 方案对比

| 方案 | 描述 | 优点 | 缺点 | 结论 |
| --- | --- | --- | --- | --- |
| A：纯函数 + 手写 CPS | `dx::open(...)` 返回 Action，用户手动嵌套闭包/`and_then` | 零新增、无解析器 | 样板深（每步一个闭包），可读性差，违背「接近普通 Rust」 | 基线，被 B 取代 |
| **B：`do_!` 宏折叠语句 → and_then 链** | 命令式块语法，`let` 绑定值经闭包贯穿 | 展开与手写逐节点同构；接近普通 Rust；无类型魔法；冻结面零改动 | 需维护 syn 2.x 解析（已修复：`Block` 定界符不入输入流、`LocalInit` 结构体化、`Spanned` 导入） | **采用** |
| C：自定义 DSL + 新 Action 节点 | 引入 While/For/Try 等节点 | 表达力更强 | 破坏冻结面；冲突检测/撤销/推导全表需重设计 | 拒绝（需 RFC 另立） |
| D：op 级操作宏 `open!`/`write!`/`read!`/`close!` | 运算符风格调用 | 更短 | `write!` 与 `std::write!` 名冲突是硬伤（同块内 shadowing 破坏 fmt 宏）；函数调用已足够接近普通 Rust | 拒绝（迭代 1；如未来需要可改名 `dx_write!` 等再议） |
| E：async/await 式 EDSL | `let fd = dx::open(p).await?` | 最像普通 Rust | 引入非确定性/隐式运行时依赖；与「蓝图 = 数据、可静态分析」承诺冲突 | 拒绝 |

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
5. **为什么 `plan!` 内嵌 `do_!` 需预构建 Action 值**：`plan!` 的 continuation
   闭包非 `move`，直接内嵌 `do_! { dx::open(&local, …) }` 会对外部路径借用
   要求 `'static`。预构建（`let act = do_!{…}; plan!{ act; … }`）后 `plan!`
   只做值组合——这也是「蓝图即值、再组合」的自然形态，示例测试
   `plan_wraps_do_blocks` 即按此写法。如后续需要自由捕获，可给 `plan!`
   增加 `move` 变体（属宏语义变更，需独立决策）。

## 6. 验证证据（tests/dx_examples.rs，9 项全绿）

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

## 7. 已知限制与后续迭代

- `let` 不支持解构模式（标识符/通配之外报编译期错误）；
- 分支/循环体多条语句需嵌套 `do_!` 块（或未来 While/For 节点——需 RFC）；
- `Action::Catch` 尚无 `dx` 级便捷包装：错误路径当前经 `run*` 返回
  `SysError` 上抛（测试已验证），`dx::catch` 可作为后续迭代；
- 空集推导的操作（TcpBind/Spawn 等）其新句柄声明留给用户责任域，文档已注明。

## 文档入口

- 本设计：`docs/src/dx-design.md`
- 运行时支撑与推导表：`crates/algeff-std/src/dx.rs`（rustdoc）
- 宏展开语义：`crates/algeff-macro/src/lib.rs`（`do_!` rustdoc）
- 示例测试：`crates/algeff-std/tests/dx_examples.rs`
