# 设计推导与验证过程（工程论文）

> 本文档是 Algeff 设计从动机到验证的完整推导链，按工程论文结构组织。所有数字与结论均提炼自
> 仓库内证据源（`spec/`、`contracts.md`、`pdr.md`、`perf/`、决策链 `D-` 编号）；本文档为
> **阅读入口**，权威源以各证据文件为准，两者冲突时以证据源为准。

## 摘要

Algeff（Algebraic Effects）将 Unix 系统效应从「指令（动词）」代数化为「数据（名词）」——
不可变的 Action 蓝图——使控制流可组合、可缓存、可重放。本文给出从**动机**到**验证**的完整
推导链：

1. **公理化**：A1–A7 七条公理定义组合律、资源线性、分支隔离、撤销双态与无死锁调度；
2. **命题**：P1–P5 五条可证明性质建立于公理之上（幺半群、并行交换律、写隔离、撤销双态、无死锁）；
3. **契约冻结**：D1–D19 十九项决策把承诺边界钉死在 `contracts.md`，作为 8 Agent 并行开发的唯一接口事实来源；
4. **实现**：三层 crate（core 解释器 / std tokio 执行器 / macro 语法糖），约 2000+1200+300 行；
5. **验证**：305 个测试函数（约 297 个二进制测试 + 8 条 doc-test）+ 41 个测试二进制，叠加
   `tla/scheduler.tla` 模型检测；
6. **审计**：5 轮「对抗审计（120 个 E2E 测试）× 形式逻辑审计」串行收官——P1/P2/P3/P5 终判
   「有效（附声明前提）」，P4 部分收敛（差距 RFC-05，阶段 3+ 已裁决）。

**结论**：语义正确性定案，可作研究/原型基线；并行性能（executor 锁串行化，R-6）为**已知开放面**，生产采用需先完成阶段 3+ 缺口清单（§10）；跨平台错误语义（RFC-10）已修复（executor 层归一化，fdd0cfe）。

---

## 1 动机与问题陈述

**观察（pdr.md §0–§1）**：传统 Unix 编程模型中，系统效应以**指令**形式存在——「打开文件」、
「发送字节」是动词，以过程调用/系统调用的方式嵌入控制流。指令形态有三个结构性缺陷：

1. **不可组合**：效应发生的顺序与嵌套被调用栈固化，难以正交地拼接；
2. **不可缓存**：指令在执行瞬间才产生效果，无法把「做过的事」作为值保存复用；
3. **不可重放**：指令不携带语义结构，无法在同一蓝图下确定性重放或撤销。

**问题陈述**：能否把系统效应编码为**不可变数据**（Action），使副作用从「动词」变为「名词」，
从而获得组合性、可缓存性、可重放性，同时保留可逆性（撤销）与反应性（依赖通知）？

**方法（支柱一：效应代数化）**：所有系统交互编码为不可变的代数数据类型 Action；副作用从
「指令」转变为「数据」，控制流可自由组合、缓存、重放。可逆性（`trackΓ`/`recoverΓ`）与反应性
（`notify`）由运行时模型保证；Rust 编译器负责内存安全，运行时模型负责业务语义安全，两者分层、
互不干扰（pdr.md §1.2）。实现仅依赖 tokio，编译时间秒级。

→ 详见 `pdr.md` §0–§1（完整推导见 `spec/axioms.md` §记号 与 §1.1）

## 2 公理化与命题

### 2.1 公理 A1–A7（`spec/axioms.md`）

| 公理 | 陈述（直觉） | 工程载体 |
| --- | --- | --- |
| A1 结合律 | `(a;b);c = a;(b;c)`——顺序组合可自由分组 | `Action::Sequential` + `runtime.rs::interpret` |
| A2 单位元 | `1;a = a;1 = a`（`1 = Pure(Unit)`） | `Action::Pure` |
| A3 交换律 | `Δ(a)∩Δ(b)=∅ ∧ Sym(f) ∧ Cov(Δ) ⇒ Fork(a,b,f) ≡ Fork(b,a,f)`——资源不相交且 combine 对称时并行可交换 | `ResourceRegistry::can_parallel(_with)`（冲突矩阵） |
| A4 资源线性 | Write/Own 恰好消费一次；Read/Append 不消费可重复 | `ResourceRegistry::check_linear` |
| A5 分支隔离 | Choose 左写不影响右读；Fork 子任务 COW 隔离 | `ResourceRegistry: Clone`（D13）+ `ResourceHandle` 全 Arc |
| A6 撤销双态 | 可逆操作 `w` 存在逆 `w̄`：`w;w̄ = 1`（Full 撤销策略） | `UndoOp`（D4）+ `UndoStack` LIFO |
| A7 无死锁 | 动态获取 = 原子占坑 + 失败回滚 + 有限重试 ⇒ 无循环等待链 | `ResourceArbiter` + 静态串行化 |

公理层的**关键设计选择**：等价关系一律取**执行语义等价**（可观察 trace / 状态变换），而非
AST 语法相等——Action 含 `NextFn` 闭包，语法不可比（A1 风险备注）。

### 2.2 命题 P1–P5（`spec/proofs.md`）

| 命题 | 陈述 | 依赖 |
| --- | --- | --- |
| P1 | `(Action, ;, 1)` 构成**幺半群** | A1 + A2 |
| P2 | 资源不相交 + combine 对称 ⇒ 并行组合满足**交换律** | A3 |
| P3 | Choose/Fork 分支**写隔离**（左写不影响右读） | A5 |
| P4 | 可逆操作满足**撤销双态**（`w;w̄` 恢复状态至执行前） | A6 |
| P5 | 调度器**无死锁**（无循环等待链） | A7 + D6/§9.1（静态降级） |

依赖图：`A1+A2 → P1`；`A3 → P2`；`A5 → P3`；`A6 → P4`；`A7+D6 → P5`。P5 是唯一需要
「公理 + 工程决策」共同支撑的命题（静态冲突降级是 P5 的第一道防线）。

→ 详见 `spec/proofs.md`（证明全文 + Rust 测试映射 + TLA 模型对照）

## 3 契约设计（D1–D19）

契约（`contracts.md` §3）把公理/命题的承诺边界落为 19 项工程决策。分组如下（分组为阅读组织，
部分决策跨组，按其主语义归组）：

### 3.1 语义组（Action 语义与执行模型）

| # | 决策 | 一句话理由 |
| --- | --- | --- |
| D2 | Action 递归字段一律 `Box<Action>` | E0072 无限大小；CPS 续延装箱 |
| D3 | `SyscallExecutor` 方法返回 `BoxFuture` | async fn 不 dyn 兼容，Runtime 需 `Box<dyn>` |
| D4 | `UndoOp = Pin<Box<dyn Future<Output=()> + Send>>` | tokio IO 撤销必然异步 |
| D6 | `can_parallel` 保守：Append∥Append 默认串行 | OS 保证原子追加但不保证顺序，确定性优先；调用方显式 opt-in |
| D8 | `SendFile.in` → `input` | Rust 保留字 |
| D9 | `Runtime::new` 须在 tokio 上下文之外调用（自持 reactor） | pdr.md §12.3 reactor 字段 |
| D10 | `Replace` 语义：先 recover 再执行 target | 安全默认（资源不泄漏） |
| D11 | `Alloc` 返回 `Value::Bytes(vec![0; len])` | 确定性；COW 优化留给实现层自选 |
| D12 | 路径规范化：纯词法（绝对化 + 消除 `.`/`..`），不碰真实 FS | 确定性可重放；符号链接解析属物理层 |
| D14 | Fork 阶段 1 语义：静态冲突检测 + 顺序执行（left→right→combine） | 交换律是「可并行」而非「必须并行」；顺序执行零状态共享风险 |

### 3.2 资源组（句柄与注册表）

| # | 决策 | 一句话理由 |
| --- | --- | --- |
| D1 | `Fd = u64` 全局唯一句柄，单调不复用 | 避免 fd 重用冲突；**边界注（RFC-06）**：承诺范围为不溢出前缀（~362 轮后 u64 溢出，阶段 3+ 优先修复） |
| D5 | `PipeOpen` 用 tokio duplex 实现 | 跨平台（Windows 无 OS pipe 的 tokio 封装）；语义 = 内存管道 |
| D7 | typestate 包装命名 `TypedResource<M>` | 与 `Resource` 枚举同名冲突 |
| D13 | `ResourceRegistry` 实现 `Clone` | Fork 子任务隔离状态，完成后合并回父（A5 的工程载体） |
| D15 | undo 闭包只捕获物理资源数据（Arc 句柄/原内容/路径），禁止捕获 registry 引用 | 闭包是 `'static`，execute 只拿 `&mut registry` |

### 3.3 错误组（失败路径语义）

| # | 决策 | 一句话理由 |
| --- | --- | --- |
| D16 | `ResourceArbiter`：动态仲裁 = 原子占坑 + 失败回滚 + 有限重试（8×1ms），超限 `WouldBlock` 快速失败 | 竞争从阻塞等待改为「有限重试后失败回滚上抛」，配合 A7 无死锁（其并发面在 §6/P5 展开） |

### 3.4 并发组（并行与 Send 边界）

| # | 决策 | 一句话理由 |
| --- | --- | --- |
| D17 | Fork 并行路径：executor 经 `Arc<Mutex<Box<dyn SyscallExecutor>>>` 共享；子任务隔离 registry/undo/context，完成后合并回父（handles/consumed/owned_consumed 并入、next_fd 取 max、undo 按 right-left 合并保 LIFO）；Send 边界不满足时回退顺序 | D13 的完整落地；审计 blocker-1 修复 |
| D18 | 四个闭包类型别名（NextFn/CondFn/CombineFn/HandlerFn）加 `+ Send`，Action 变为 Send | Fork 线程级并行（`tokio::spawn`）前提；否决 unsafe impl Send |
| D19 | `SyscallExecutor: Send` 超 trait；`Runtime::new(Box<dyn SyscallExecutor + Send>)`；删除 unsafe 包装 | 消除 unsafe 健全性风险；编译期强制执行器 Send |

**冻结面 = 正确性承诺边界**：D1–D19 一旦冻结，变更必须走 RFC → CTO 裁决（`contracts.md` §1
文件所有权表 + §4 阶段门禁 G0–G4）。

→ 详见 `contracts.md` §2–§3

## 4 关键决策推导

### 4.1 深度守卫阈值 96 的实测-裁决链（RFC-11 / D-051 / D-052 / D-054 / D-055）

| 环节 | 事实 | 证据 |
| --- | --- | --- |
| 发现（R4c） | `run_sub_impl` 对嵌套子 Action 递归，每层 `Box::pin` 栈帧 ~13–20KB（debug）；Windows 默认 2MB 测试线程栈下深度 ~110–120 即 `STATUS_STACK_OVERFLOW` 进程级 abort；release 1000 层同样溢出 | `spec/resource-notes.md` §10 RFC-11；`adversarial_r4c.rs` |
| 登记 | RFC-11 登记为 HIGH（不受信任蓝图嵌套 ~百层可致宿主崩溃 = 拒绝服务面），修复方向 = 嵌套深度计数器 | D-051（`b9a2868`） |
| 实测 | 未守卫探针：深度 **104 Ok / 108 abort**（0xc00000fd）→ 崩溃边界 ~104–108；原定阈值 128 在 2MB 栈下晚于崩溃触发（128 帧 ≈2.2MB+ 已越过边界，守卫自身先崩） | D-052 上下文（A2 批 7 `444b6708`） |
| 裁决 | 阈值取 **96** = 实测边界 104 留 ~8% 余量（帧大小随嵌套构造/编译器版本波动）；错误 = `Err(SysError::Other(105))`（ENOBUFS 语义近似，冻结面内零契约变更），可被外层 Catch 捕获 | D-052（supervisor 裁决 `f0de9812`） |
| 方向勘误 | 左结合形式 `(a;b);c` 型 current 嵌套**先触及阈值**（消耗深度 = 链长 − 1）；右结合 `a;(b;c)` 型 next-CPS 延续**恒为深度 1**；超限行为不在 P1 结合律承诺内 | D-053/D-054 + `spec/proof-obligations.md` P1 行 |
| 范围修正 | 守卫保证限于 ≥2MB 栈；Windows 主线程默认 1MB（实测崩溃边界 ~50–54 帧），55~95 层会在守卫触发前 abort → 属用户责任；正确缓解 = 链接器 `/STACK` / spawn 线程（`RUST_MIN_STACK` 只影响新线程）/ Catch Other(105) | D-055（`b756e03`） |

**深度公式（D-054 勘误后）**：

- 左结合链（`adapters::seq()` 左折叠，current 嵌套）：解释深度 = 链长 − 1；链长 **L ≥ 97** →
  `Err(Other(105))`，用户需改右结合（and_then CPS）或 Catch。
- 右结合链（next-CPS 延续）：恒深 1，不受守卫限制（受内存约束）。
- 实测安全深度：64（`deep_nesting_under_limit_ok`，与 R4c 一致）；回归对照
  `nested_sequential_64_deep_recursive_frames_values_flow`。

### 4.2 Fork 并行：静态冲突判定 → 双路径（D6/D14/D17）

1. **冲突矩阵（D6）**：`can_parallel_with` 按 §9.1 矩阵判定——Read×Read 可并行；Read×Write、
   Write×Write、Own×任意串行；Append×Append 默认串行（顺序无关时显式 opt-in）。矩阵是 A3/P2
   的工程载体，**保守方向**与公理一致。
2. **阶段 1 顺序语义（D14）**：Fork 一律顺序执行 left→right→combine——交换律是「可并行」而非
   「必须并行」，顺序执行保持 combine 语义且零状态共享风险。
3. **并行路径（D17）**：`can_parallel=true` 时真并行——`spawn_blocking × 2` + current-thread
   runtime 驱动；executor 经 `Arc<Mutex<Box<dyn SyscallExecutor>>>` 共享；子任务隔离
   registry/undo/context，完成后合并回父；Send 边界不满足时回退顺序。
4. **已知代价**：并行路径实测被 executor 互斥锁串行化（§9），D17 的收益留待 R-6 锁重构兑现。

### 4.3 其余关键决策

- **Fd=u64 单调不复用（D1）**：全局唯一句柄消除「关闭后 fd 复用」的悬挂风险；代价是 u64 值域
  在 RFC-06 场景（Fork 右分支 `k<<48` 预留区间 + merge 归一化失效）下 ~362 轮二次增长溢出。
- **Replace = recover + clear（D10）**：先撤销当前路径全部效果，再 `registry.clear()` 释放
  handles/consumed/owned_consumed（`next_fd` **不复位**，保 D1 单调性），随后新蓝图在同一
  注册表继续分配/消费（`replace_semantics` 测试实证）。
- **CPS + Box（D2）**：全部 Action 节点 CPS，递归字段装箱——这是「蓝图可组合」的实现前提，
  也是深度守卫问题的根源（§4.1）。

→ 详见 `contracts.md` §3 与决策链 `.pi/decision-auditor/chain.md`（D-051~D-055）

## 5 实现架构（三层 crate）

| 层 | crate | 职责 | 代码量估算 | 稳定性 |
| --- | --- | --- | --- | --- |
| 内核 | `algeff-core` | Action AST（13 种节点）+ ResourceSet/Resource\<M\> + Runtime 内核（interpret trampoline、UndoStack、ResourceArbiter、Fork 并行 D17、深度守卫 RFC-11；coeffects/virtual-clock 为可选特性） | ~2000 行 | 永久冻结 |
| 物理 | `algeff-std` | TokioExecutor（全部 DataOp，Full 撤销策略）+ 预包装适配器 + 值流组合器 | ~1200 行 | 永久稳定 |
| 语法糖 | `algeff-macro` | `plan!`/`fork!`/`scope!`/`choose!`（纯 AST 构造） | ~300 行 | 极少修改 |

**冻结面所有权表**（`contracts.md` §1 摘要）：

| 文件 | 拥有者 | 说明 |
| --- | --- | --- |
| `core/src/action.rs`、`error.rs`、`syscall.rs`、`lib.rs`；`std/src/lib.rs` | **CTO 冻结** | 类型面零漂移 |
| `core/src/resource.rs` | A3 | 冲突检测、typestate、registry |
| `core/src/runtime.rs` | A2 | 解释器、UndoStack、Runtime、深度守卫 |
| `std/src/executor.rs`、`adapters.rs` | A5 | TokioExecutor 物理后端 |
| `macro/src/lib.rs` | A4 | 宏 |
| `tla/scheduler.tla` | A6 | 调度器模型 |
| `docs/`、`scripts/`、`README.md`、CI | A8 | 文档与发布 |
| `std/benches/` | A7 | criterion 基准 |

规则：Cargo.toml 一律不许改；新文件只能建在自己的目录；`cargo test --workspace` 必须保持绿色。

→ 详见 `contracts.md` §1 与 `pdr.md` §15、§19

## 6 验证方法

### 6.1 测试分层

| 层 | 载体 | 示例 |
| --- | --- | --- |
| 单元 | `resource.rs` / `arbiter.rs` 内单测 | `linearity_double_write_rejected`、`conflict_matrix_exhaustive_4x4` |
| 属性（proptest） | `tests/axioms.rs`、`commutation.rs`、`arbiter.rs` | A3 对称 combine 交换、arbiter 三不变量（单调不减/原子快照/无泄漏） |
| 执行级 | `tests/execution_axioms.rs` | `exec_A1_associativity`、`exec_A2_identity`、`exec_A6_undo_roundtrip` |
| 端到端 | `algeff-std/tests/` | 文件往返、TCP echo、撤销往返、深度守卫 |
| 对抗 E2E | `tests/adversarial_r{1..5}.rs` | 120 个测试（R1=17、R2=19、R3=28、R4=32、R5=24） |
| 模型检测 | `tla/scheduler.tla` | TLC 通过 `TypeOK`/`ExclusiveHold`/`ExactHold`/`NoCircularWait` 4 不变式 + `Progress` 时序属性 |

总量：`cargo test --workspace` **305 个测试函数**（约 297 个 `#[test]`/`#[tokio::test]` +
8 条 doc-test 断言；41 个测试二进制 + 3 个 doc-test 运行）。特性测试（coeffects/virtual-clock）
由 feature 门控，CI 三平台补跑（并含 release 编译验证）。

### 6.2 审计协议（5 轮串行）

每轮按「**对抗审计（E2E 验证）× 形式逻辑审计（证明正确性）**」串行闭环：

1. 对抗审计派发：对新攻击面写 E2E 测试（尽早提交纪律，避免产物丢失）；
2. 产出**外部行为证据**（通过/失败/文档化偏差）；发现缺陷 → 修复或登记 RFC；
3. 数学审计对 P1–P5 证明做收敛判定（有效 / 有效附声明前提 / 部分 / 有缺口）；
4. 义务表（`spec/proof-obligations.md`）更新：每轮结论写入轮次日志与 A1–A7 × P1–P5 明细。

→ 详见 `spec/proof-obligations.md`（协议 + 轮次日志）、`spec/verification-plan.md`（分层计划）

## 7 审计轮次详表（R1–R5）

| 轮 | 对抗审计（E2E） | 形式逻辑审计 | 发现 / 修复 / 登记 | 收敛判定 |
| --- | --- | --- | --- | --- |
| R1 | `adversarial_r1.rs` 17 测试（5d24166）：游标撤销修复 + Replace 旧 fd 可写反例 + 12 项声称验证 | P1 有效；P2 有缺口（combine 对称未入 A3 陈述）；P3/P4/P5 部分 | 游标撤销缺口 **已修复**（D-031）；RFC-05 句柄活性反例证据化（留阶段 3+） | 4 处未收敛点 → 派发修复 |
| R2 | `adversarial_r2.rs` 19 测试（2eb7312）：fd 区间压力/arbiter 争用/R1 回归/错误路径/时间面 + flaky 修复（写后 flush，D-039） | P2 收敛「有效（附声明前提）」；A3 陈述并入 Sym(f)+Δ-覆盖；RFC-06→D1 边界反例；RFC-08→P4/A6 部分可撤销反例 | 登记 RFC-06（fd 二次增长 u64 溢出）、RFC-07（管道半端）、RFC-08（Timeout 孤儿分支） | 3 处登记级未收敛点已修 |
| R3 | `adversarial_r3{a,b,c}.rs` 28 测试：Catch/Scope/撤销栈 + Alloc/确定性/用户责任 + 网络/R2 回归 | P3/P5 升级「有效（附范围声明）」；盲区 = 系统性（链长 ≥2 仅首 op 可见）→ 前提入 spec 三处；A6/P4 范围限定句（trackΓ + RFC-08/09） | 盲区实证（2 测试）；SendFile flush 缺口 **已修复**（D-046）；登记 RFC-09（Timeout 锁饥饿）与 LOW（退避串行化） | 5 处登记级修正已落地 |
| R4 | `adversarial_r4{a,b,c}.rs` 32 测试：组合态深挖/多 Runtime 隔离/Open 矩阵/规模栈深 | P1 有效（1000 链规模证据）；P2 有效附前提（16 路）；P3/P5 有效附范围；P4 收敛中 | 登记 RFC-10（Windows 错误码）；登记并 **修复 RFC-11**（深度守卫阈值 96，栈溢出 → 可捕获错误） | 收口：README 300、make_mut 残留清零 |
| R5 | `r5a` 8 + `r5b` 16 测试（945eb52/1ce734c）：五连回归/守卫边界 95/96/97/50 轮风暴/修复点交互/Invoke 假执行器/蓝图复用 | **终轮**：P1/P2/P3/P5 = 有效（附声明）终判；P4 = 部分（RFC-05 未闭环——唯一开放差距，阶段 3+ 已裁决） | 守卫边界 95/96/97 证据；跨 Runtime 别名 undo 非恒等偏差（文档化） | **5 轮收官**：A1–A7 终态齐备，无 UNKNOWN 残留 |

→ 详见 `spec/proof-obligations.md` 轮次日志与义务明细（权威）

## 8 已知缺陷与边界

### 8.1 RFC 缺陷登记表（`spec/resource-notes.md` §10）

| # | 缺陷 | 状态 | 修复方向（阶段 3+） |
| --- | --- | --- | --- |
| RFC-05 | Replace 后旧 fd 仍可写（句柄活性反例：registry clear 与 executor 强引用解耦） | 已登记（R1 发现，R5 补录）；**阶段 3+** | executor↔registry 通道（undo 携带句柄回收信息） |
| RFC-06 | Fork 右分支分配使父 next_fd 二次增长，~362 轮 u64 溢出（release 回绕 = fd 复用，违反 D1） | 已登记（R2）；**阶段 3+ 优先** | `offset_next_fd`/`merge` 区间归一化 |
| RFC-07 | 管道半端经 D13 Clone 共享 Arc → executor `Arc::get_mut` 失败 → 分支内管道 IO 被错误拒绝 | 已登记（R2）；**阶段 3+** | 管道双表改造或 make_mut + 代际标记 |
| RFC-08 | Timeout 内并行 Fork 孤儿分支副作用不可撤销（P4/A6 边界反例） | 已登记（R2 编号修正）；**阶段 3+** | 取消传播/分支取消协议 |
| RFC-09 | Timeout 取消持锁分支 → 锁 id 全局饥饿至 recover（可恢复，非永久毒化） | 已登记（R3）；**阶段 3+** | 取消传播协议 |
| RFC-10 | Windows 错误码未映射 POSIX 语义（Other(80) vs AlreadyExists 等） | **已修复**（R5 后，fdd0cfe：executor 层 `to_sys_err` + `normalize_windows_errno`，冻结面零改动） | ErrorKind 优先 + Win32/WSA 码表兑底 |
| RFC-11 | 解释器嵌套蓝图递归栈溢出（进程级 abort） | **已修复**（R4→A2 批 7，阈值 96 深度守卫，D-052） | — |

另：审计期内另有 2 项**非 RFC 编号**缺陷已修复——R1 游标撤销（D-031）、R3 SendFile flush
（D-046）。合计审计共修复 3 项缺陷；R5 后新增修复 RFC-10（Windows 错误码归一化，fdd0cfe）——共 4 项。

### 8.2 边界与已知盲区

- **闭包盲区（系统性）**：`fork_conflict` 静态资源收集只遍历 AST 可见节点——`Sequential` 仅
  收集 `current`（next 闭包不可见）、`Catch` 仅收集 `action`（handler 不可见）→ **所有链长 ≥2
  的 Sequential 分支只有首 op 资源可见**。对 P2 定理本体无证伪（语义 Δ 下前提不满足，条件句
  保真）；「零锁并行调度」仅对顶层可见资源成立。失败模式：MutexLock → WouldBlock 安全失败；
  同资源 Write → 交错不确定（确定性违反，A4 线性保持）。
- **锁串行化（R-6）**：executor 互斥锁（`Arc<Mutex>`）覆盖整个 execute（含物理 IO await）——
  D17 并行路径已触发但 Syscall 全部串行（§9 实测）。另 LOW：arbiter 退避 sleep 发生在锁内，
  争用风暴下全分支串行化放大。
- **1MB 栈缺口（D-055）**：守卫保证限于 ≥2MB 栈；Windows 主线程默认 1MB（崩溃边界 ~50–54 帧），
  55~95 层在守卫前 abort → 用户责任；缓解 = `/STACK` / spawn 线程 / Catch Other(105)。
- **make_mut 物理 COW 未实现**：语义层（registry Clone 隔离 + 读隔离测试）已闭环；物理延迟复制
  归阶段 3（resource-notes §9，与 Dup 真共享语义冲突需专门设计）。
- **撤销原子性**：补偿闭包正确性由用户承诺（pdr.md §17）；大文件写入撤销的双倍 IO 用 BestEffort
  策略工程缓解。

→ 详见 `spec/resource-notes.md` §10、`pdr.md` §17–§18

## 9 性能评估

### 9.1 方法

- criterion 0.5，**等负载同参数双对比项**：原生 tokio 参照臂与 Algeff 臂在同一 bench 文件内
  测量（同 setup、同 runtime 线程池、同服务端）；对比% = Algeff 中位 / 原生中位 × 100%。
- 环境：Windows 10 Pro 19045、AMD Ryzen 9 5950X、64 GiB、rustc 1.96.0。
- 基线文件：`perf/baseline-2026-08-15.txt`（批 2 原生参照 + 批 4 D17 并行后复测）。

### 9.2 数据（批 4，D17 并行 Fork 后）

| 场景 | 原生 tokio（中位） | Algeff（中位） | 对比 | 批 3（D14 顺序）历史 |
| --- | --- | --- | --- | --- |
| echo（100 连接 × 1 往返 1KB） | 27.771 ms | 28.624 ms | **103.1%** | 100.0% |
| parallel_reads（10 文件 × 1MB 并行读） | 3.2327 ms | 11.837 ms | **366.2%** | 340.0% |
| shared_read（8 任务同文件 8MB） | 1.5036 ms | 8.5845 ms | **570.9%** | 307.6% |
| append（10 任务 × 32KB 顺序追加） | 6.0947 ms | 1.4814 ms | **24.3%** | 29.4% |

### 9.3 归因

- **echo 103.1%**：无 Fork（单链 Sequential），与 D17 无关；在每样本 1 iter 的 ±10% 噪声带内，
  顺带确认无回归。
- **parallel_reads 366.2%**：D17 并行路径**已触发**（10 文件零冲突 → `can_parallel=true` →
  分支线程并发），但读仍串行化——执行器互斥锁（`ExecAccess::Shared` 的 `Arc<Mutex<SendExecutor>>`）
  在 `exec_via` 中对**整个 execute（含物理 IO await）**持锁，跨分支所有 Syscall 串行通过该锁；
  逐 Fork 节点 spawn_blocking + current-thread runtime 创建开销进一步抵消收益。**R-6**：锁边界
  收窄（物理 IO 移出锁外）属 A2 域，待阶段 3+ 重构。
- **shared_read 570.9%**：同 fd 游标读共用 `files[fd]` 文件互斥锁与游标（`op_read` 按序推进），
  即使锁边界收窄也不并行，需位置读原语（执行器层，A5 域待办）；实测 8.58ms 反超 D14 顺序基线
  （6.41ms）——D17 并行对同 fd 游标读是纯损失（诚实数据，不修饰）。
- **append 24.3%**：走 D6 默认串行路径（顺序 Open{append}+Write+Close）；小负载下串行追加
  （1.48ms）显著快于原生 10 路并行追加（6.09ms）。原生臂含每任务 flush()，Algeff 臂不 flush
  （小量不对称，如实记录）。opt-in 并行留待后续基准驱动。
- **pdr §16 预期**：并行读目标 ~100%（对照列），未达成的原因全部指向 R-6 锁串行化与游标共享，
  属**已知工程局限**（pdr.md §17「动态资源仲裁的锁竞争」），非语义正确性缺口。

→ 详见 `perf/baseline-2026-08-15.txt`（完整数据 + 命令清单 + 已知限制）、`crates/algeff-std/benches/README.md`

## 10 结论与后续工作

收官（P1/P2/P3/P5 有效附声明、P4 部分收敛且差距明确）、305 测试全绿、契约冻结。并行性能为
**已知开放面**（RFC-10 跨平台错误语义已修复），本阶段不提供稳定性承诺。

**阶段 3+ 工作清单**：

1. **RFC-05**：executor↔registry 句柄回收通道（Replace 旧 fd 活性闭环，P4 收敛前置）；
2. **RFC-06**：`offset_next_fd`/`merge` 区间归一化（D1 边界注兑现，优先）；
3. **RFC-07**：管道半端双表改造或 make_mut + 代际标记；
4. **RFC-08/09**：Timeout 取消传播协议（孤儿副作用可撤销 + 锁饥饿消除）；
5. ~~RFC-10~~（已修复 fdd0cfe，executor 层归一化，无需 D20 授权）；
6. **R-6 锁重构**：`exec_via` 锁边界收窄（物理 IO 移出锁外），兑现 D17 并行收益（pdr §16 ~100%）；
7. **位置读原语**：shared_read 并行前置（执行器层）；
8. **make_mut 物理 COW**：阶段 3 并行写前置（resource-notes §9.4 裁决，需先补分支破坏性操作契约测试）；
9. **LOW**：arbiter 退避移出锁内；
10. **发布**：G4 终验——`cargo publish` algeff-core → algeff-std → algeff-macro（0.1.0）。

→ 详见 `spec/resource-notes.md` §9–§10、`pdr.md` §19 阶段划分
