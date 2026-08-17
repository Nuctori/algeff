Algeff 完整设计规范 · v3.2
Algeff = Algebraic Effects
将 Unix 效应代数化的跨平台确定性运行时框架
原暂名：PDR（Planio Deterministic Runtime）
本规范基于 PDR v3.0 设计思想，吸收多轮讨论，修正形式化与工程实现的不一致，统一 Action 类型语义，明确可逆性适用范围，平衡效应覆盖与实现简洁性，并提供类型安全资源包装以降低误用。

〇、定位声明
Algeff 是一份独立于宿主语言的理论规范与工程实现指南。它不试图在 Rust 类型系统中模拟依赖类型或线性类型，而是将 线性逻辑与依赖逻辑的保证 从编译期下沉到运行时，由 Runtime 基于形式化代数模型（借鉴 Cordis 论文的可逆效应与反应式余效应）来保证。

工程实现放弃复杂的 GADT 模拟宏、常量泛型体操和生命周期标记，转而使用最朴素的 Rust 数据结构，将编译时间从“分钟级”降回“秒级”，同时获得更强的业务语义保证（可逆性、可重放性、依赖可管理性）。

核心信条

“Rust 编译器保证内存安全。运行时模型保证业务语义安全。两者分层，互不干扰。”

三个组件：

组件	定位	产出物
Algeff-Spec	理论规范，定义 Action AST、代数公理、资源追踪模型	本文档（理论部分）
Algeff-DSL	编写蓝图的外部/嵌入式 DSL	语法规范 + 极简宏（可选，仅语法糖）
Algeff-Runtime	执行蓝图、管理逆操作、协调依赖的引擎	Rust 实现（仅依赖 tokio）
一、哲学基础与设计原则
1.1 三大核心支柱
支柱一：效应代数化（Effect Algebraization）
将所有系统交互编码为不可变的代数数据类型——Action。副作用从“指令（动词）”转变为“数据（名词）”，使控制流可以被自由组合、缓存、重放。Algeff 的目标是尽可能覆盖 Unix 系统效应，将这些效应纳入统一的代数结构，即使某些效应无法满足全部代数律（如交换律），也要通过补偿或显式声明来处理，而不是将其排除在框架之外。

支柱二：运行时模型保证（Runtime Algebra）
借鉴 Cordis 论文的核心思想，将 可逆性 与 反应性 作为运行时的一等公民：

可逆效应：每个上下文变换携带显式逆变换，运行时追踪逆变换的复合，确保组件卸载时上下文被完全恢复。

反应式余效应：组件声明依赖规范，运行时在上下文变化时主动通知组件，驱动激活/停用。该特性作为可选模块提供，不影响核心稳定性。

支柱三：结构相似性原则（Structural Similarity）
物理实现可以替换，但控制流拓扑（分支/作用域/跳转）必须与代数蓝图 1:1 对应。Fork -> spawn 是合法的，因为两者都满足“并发分支”的拓扑结构。

1.2 分层安全保证
层级	安全保证	机制
物理层	内存安全、资源释放	Rust 所有权 + 生命周期 + Drop
运行时代数层	可逆性、依赖管理、重放	trackΓ/recoverΓ、notify、VirtualClock（可选）
业务蓝图层	控制流可组合性	Action AST + 解释器
API 辅助层	降低资源声明误用	类型状态包装（无宏）
关键洞察：Rust 编译期已经提供了足够强的内存安全地基，Algeff 不再需要繁琐的宏来模拟依赖类型或线性类型。这些保证由运行时模型在业务语义层面提供，同时 API 层通过轻量级类型状态包装减少用户误用。

二、核心类型：Action
2.1 类型定义
所有 Action 节点统一采用 CPS（Continuation-Passing Style），next 类型一致为 NextFn = Box<dyn FnOnce(Value) -> Action>。该枚举不含任何泛型参数，所有类型信息由运行时通过上下文追踪。

rust
type Value = ...; // 运行时值，可为 ()、字节块、fd 等
type NextFn = Box<dyn FnOnce(Value) -> Action>;

enum Action {
    Pure(Value),
    Syscall {
        op: DataOp,
        resources: ResourceSet,
        next: NextFn,
    },
    Choose {               // 条件分支
        cond: CondFn,
        then_branch: Action,
        else_branch: Action,
    },
    Fork {                 // 并发分叉
        left: Action,
        right: Action,
        combine: CombineFn,
    },
    Scope {
        base: Path,
        inner: Action,
        next: NextFn,
    },
    Alloc {
        len: usize,
        next: NextFn,
    },
    Replace {
        target: Action,
    },
    Invoke {
        foreign_id: Id,
        captures: ResourceSet,
        yields: ResourceSet,
        deterministic: bool,
        next: NextFn,
    },
    Sleep {
        duration: Duration,
        next: NextFn,
    },
    WatchSignal {
        signal: Signal,
        next: NextFn,
    },
    Timeout {
        action: Action,
        duration: Duration,
        on_timeout: Action,
    },
    Sequential {
        current: Action,
        next: NextFn,
    },
    Catch {
        action: Action,
        handler: HandlerFn,
    },
}
这些节点覆盖了：纯值、系统调用、条件分支、并发分叉、作用域、内存分配、控制流跳转、外部调用、时间等待、信号监听、超时、顺序组合、错误捕获。足以表达绝大多数 Unix 程序的控制流与效应。

2.2 数据平面原语（DataOp）
DataOp 覆盖尽可能多的 Unix 系统效应，包括文件、目录、网络、管道、进程、信号、内存映射、时间、同步等。

rust
enum DataOp {
    // 文件
    Open { path: Path, flags: OpenFlags },
    Read { fd: Fd, len: usize },
    Write { fd: Fd, data: Bytes },
    Close { fd: Fd },
    Seek { fd: Fd, offset: i64, whence: SeekWhence },
    Stat { path: Path },
    Chmod { path: Path, mode: u32 },
    Chown { path: Path, uid: u32, gid: u32 },
    Truncate { path: Path, len: usize },
    Unlink { path: Path },
    Rename { from: Path, to: Path },
    // 目录
    Mkdir { path: Path, mode: u32 },
    Rmdir { path: Path },
    ReadDir { path: Path },
    // 网络 TCP
    TcpBind { addr: SocketAddr },
    TcpAccept { listener: Fd },
    TcpConnect { addr: SocketAddr },
    TcpRead { fd: Fd, len: usize },
    TcpWrite { fd: Fd, data: Bytes },
    TcpShutdown { fd: Fd, how: ShutdownHow },
    // 网络 UDP
    UdpBind { addr: SocketAddr },
    UdpRecvFrom { fd: Fd, len: usize },
    UdpSendTo { fd: Fd, data: Bytes, addr: SocketAddr },
    // 管道
    PipeOpen { flags: PipeFlags },
    // 进程
    Spawn { cmd: Command },
    Kill { pid: Pid, signal: Signal },
    Wait { pid: Pid },
    // 信号
    SendSignal { signal: Signal, pid: Pid },
    // 内存
    Mmap { path: Path, len: usize, prot: MmapProt },
    Munmap { addr: usize, len: usize },
    // 时间
    GetTime,
    // 同步
    MutexLock { id: u64 },
    MutexUnlock { id: u64 },
    // 其他
    SendFile { out: Fd, in: Fd, offset: usize, len: usize },
    Dup { fd: Fd },
    Dup2 { old_fd: Fd, new_fd: Fd },
}
每个 DataOp 都可以在 ResourceSet 中声明其访问模式，运行时进行冲突检测和可逆性追踪。不可逆效应（如 UdpSendTo、Kill、SendSignal）通过 deterministic 标志识别，并在执行时要求用户提供补偿闭包或标记为 Skip。

2.3 资源集（ResourceSet）与访问模式
rust
enum Resource {
    Fd(i32),
    Path(String),
    MemRange(usize, usize),
    Pid(u32),
    Signal,
    Foreign(u64),
}

enum AccessMode { Read, Write, Append, Own }

struct ResourceUsage {
    resource: Resource,
    mode: AccessMode,
}

type ResourceSet = Vec<ResourceUsage>;
标识稳定性：Path 必须规范化（绝对路径、消除 .. 和符号链接）；Fd 由运行时分配全局唯一句柄（非 OS fd），避免重用冲突。

三、类型安全资源包装（无宏，辅助 API）
为了降低用户手写 ResourceSet 时误用访问模式的概率，Algeff 提供基于类型状态（typestate）的轻量级包装 Resource<M>。该包装不引入任何宏，仅通过手写泛型类型和零大小标记实现。它作为推荐默认路径，用户也可以直接构造 ResourceUsage 以获得灵活性。

3.1 状态标记类型
rust
pub struct ReadOnly;
pub struct WriteOnly;
pub struct AppendOnly;
pub struct Owned;
3.2 泛型资源包装
rust
use std::marker::PhantomData;

pub struct Resource<M> {
    inner: ResourceInner,
    _mode: PhantomData<M>,
}

enum ResourceInner {
    Fd(i32),
    Path(PathBuf),
    MemRange(usize, usize),
    Pid(u32),
    Signal,
    Foreign(u64),
}
3.3 构造函数与状态转换
为每种状态提供特定的构造器和转换方法，样板极少。

rust
impl Resource<ReadOnly> {
    pub fn new_read(inner: ResourceInner) -> Self { /* ... */ }
    pub fn into_write(self) -> Resource<WriteOnly> { /* ... */ }
    pub fn into_append(self) -> Resource<AppendOnly> { /* ... */ }
    pub fn into_owned(self) -> Resource<Owned> { /* ... */ }
}

impl Resource<WriteOnly> {
    pub fn new_write(inner: ResourceInner) -> Self { /* ... */ }
    pub fn into_read(self) -> Resource<ReadOnly> { /* ... */ }
    pub fn into_owned(self) -> Resource<Owned> { /* ... */ }
}

impl Resource<AppendOnly> {
    pub fn new_append(inner: ResourceInner) -> Self { /* ... */ }
    pub fn into_read(self) -> Resource<ReadOnly> { /* ... */ }
    pub fn into_owned(self) -> Resource<Owned> { /* ... */ }
}

impl Resource<Owned> {
    pub fn new_owned(inner: ResourceInner) -> Self { /* ... */ }
    // Owned 不能降级为 Read/Write，以防止意外共享
}
3.4 生成 ResourceUsage
通过 ModeMarker trait 自动将类型映射到访问模式，确保模式与类型一致。

rust
pub trait ModeMarker {
    fn access_mode() -> AccessMode;
}

impl ModeMarker for ReadOnly { fn access_mode() -> AccessMode { AccessMode::Read } }
impl ModeMarker for WriteOnly { fn access_mode() -> AccessMode { AccessMode::Write } }
impl ModeMarker for AppendOnly { fn access_mode() -> AccessMode { AccessMode::Append } }
impl ModeMarker for Owned { fn access_mode() -> AccessMode { AccessMode::Own } }

impl<M: ModeMarker> Resource<M> {
    pub fn into_usage(self) -> ResourceUsage {
        ResourceUsage {
            resource: self.inner,
            mode: M::access_mode(),
        }
    }
}
3.5 使用示例
rust
let fd = 3;
let read_res = Resource::new_read(ResourceInner::Fd(fd));
let usage = read_res.into_usage();   // mode 自动为 Read

// 编译错误：不能把 ReadOnly 标记为 Write
// let usage_wrong: ResourceUsage = Resource::<WriteOnly>::new_read(...); // 不存在
限制：用户仍可绕过 Resource<M> 直接构造 ResourceUsage，因此不提供绝对保证，但默认路径是安全的。此外，状态转换（如 into_owned）可以在方法内部调用运行时 API 检查资源当前是否只有唯一引用，若失败则返回错误或 panic，增强安全性。

四、形式化公理系统
4.1 基础签名
设 
A
,
B
,
C
A,B,C 为任意类型，定义：

1
1：单位元，对应 Pure(())

;
;：顺序组合，对应 and_then

∥
∥：并行组合，对应 Fork

Δ
(
a
)
Δ(a)：资源依赖函数，返回 
a
a 涉及的资源键集合

w
ˉ
w
ˉ
 ：操作 
w
w 的逆操作（撤销）

4.2 公理系统
公理 A1：结合律（顺序组合）

(
a
;
b
)
;
c
=
a
;
(
b
;
c
)
(a;b);c=a;(b;c)
公理 A2：单位元

1
;
a
=
a
a
;
1
=
a
1;a=aa;1=a
公理 A3：交换律（并行组合）
设 
a
,
b
a,b 满足 
Δ
(
a
)
∩
Δ
(
b
)
=
∅
Δ(a)∩Δ(b)=∅，且不存在 Write 或 Own 模式重叠，则：

a
∥
b
=
b
∥
a
a∥b=b∥a
注意：Append 并行虽然 OS 保证原子追加，但追加顺序不确定，违反确定性原则。因此 Append 并行仅在结果对顺序不敏感时允许；否则降级为顺序执行。

公理 A4：资源线性（use/move 拆分，D-0xx）
对于任何 
a
a，若 
r
∈
Δ
(
a
)
r∈Δ(a) 且访问模式为 Own，则 
r
r 在 
a
a 的执行路径中被恰好终结一次（move 语义：Own 之后任何 usage —— Read/Write/Append/Own —— 都拒绝，资源释放后无法确保语义）。Write 为 use 语义不限次数（物理现实：文件可多次写；运行时为每次写维护独立逆操作，LIFO 撤销仍正确）。Read/Append 不消费。

注意：互斥锁防重入由动态仲裁（A7 原子占坑）独立保证，不依赖 Write 消费。

公理 A5：分支隔离
Choose 分支中，左分支的 Write 不会影响右分支的 Read；Fork 子任务通过 COW 隔离。

公理 A6：撤销双态条件
对于任何可逆操作 
w
w，存在逆操作 
w
ˉ
w
ˉ
 ，使得：

w
;
w
ˉ
=
1
w; 
w
ˉ
 =1
不可逆操作（如 UDP 发送、进程信号）仅提供补偿挂钩，不满足该公理。

公理 A7：无死锁调度
动态资源获取采用原子占坑 + 失败回滚 + 有限重试，不存在循环等待链。

五、运行时模型：可逆效应与反应式余效应
5.1 可逆效应（Revertible Effects）
5.1.1 效果上下文（Effect Context）
给定上下文 
Γ
Γ，定义效果上下文：

∂
Γ
:
=
Γ
×
(
Γ
→
Γ
)
∂Γ:=Γ×(Γ→Γ)
即一个对 
(
当前状态
,
累积逆变换
)
(当前状态,累积逆变换)。初始效果上下文为 
(
γ
0
,
i
d
Γ
)
(γ 
0
​
 ,id 
Γ
​
 )。

5.1.2 追踪变换（trackΓ）
对于任意上下文变换 
f
:
Γ
→
Γ
f:Γ→Γ 及其逆 
g
:
Γ
→
Γ
g:Γ→Γ，定义：

t
r
a
c
k
Γ
(
f
,
g
)
(
γ
,
φ
)
=
(
f
(
γ
)
,
φ
∘
g
)
track 
Γ
​
 (f,g)(γ,φ)=(f(γ),φ∘g)
工程含义：每次执行操作时，运行时将正向变换应用到当前状态，并将逆变换复合到累积逆变换 
φ
φ 上。

定理（track 是同态）：
t
r
a
c
k
Γ
track 
Γ
​
  保持复合结构，即多个操作的追踪复合等于复合操作的追踪。

5.1.3 恢复变换（recoverΓ）
r
e
c
o
v
e
r
Γ
(
γ
,
φ
)
=
(
φ
(
γ
)
,
i
d
Γ
)
recover 
Γ
​
 (γ,φ)=(φ(γ),id 
Γ
​
 )
工程含义：卸载组件时，运行时将累积逆变换应用到当前状态，完整恢复上下文。

关键定理（可逆性）：

r
e
c
o
v
e
r
Γ
∘
t
r
a
c
k
Γ
(
f
1
,
g
1
)
∘
⋯
∘
t
r
a
c
k
Γ
(
f
n
,
g
n
)
(
γ
0
,
i
d
Γ
)
=
(
γ
0
,
i
d
Γ
)
recover 
Γ
​
 ∘track 
Γ
​
 (f 
1
​
 ,g 
1
​
 )∘⋯∘track 
Γ
​
 (f 
n
​
 ,g 
n
​
 )(γ 
0
​
 ,id 
Γ
​
 )=(γ 
0
​
 ,id 
Γ
​
 )
5.1.4 可逆效应函数（Effect Function）
实际效果函数不仅变换上下文，还要在调用时动态返回其逆：

E
Γ
:
=
Γ
→
Γ
×
(
Γ
→
Γ
)
E 
Γ
​
 :=Γ→Γ×(Γ→Γ)
带见证的效果函数要求：对于任意状态 
γ
γ，
e
(
γ
)
=
(
δ
,
g
)
e(γ)=(δ,g) 满足 
g
(
δ
)
=
γ
g(δ)=γ。

工程含义：每个组件提供的操作都是一个 Effect 函数，它执行操作并返回该操作的逆。运行时将这些逆压入 UndoStack。

5.2 反应式余效应（Reactive Coeffects，可选模块）
5.2.1 余效应上下文（Coeffect Context）
定义余效应上下文为一个依赖表：

Σ
:
=
(
k
:
K
)
⇀
V
k
Σ:=(k:K)⇀V 
k
​
 
即一个有限偏函数，为每个依赖键 
k
k 绑定一个类型为 
V
k
V 
k
​
  的值。

5.2.2 依赖规范与通知
组件声明其依赖规范 
d
⊆
K
d⊆K。对任意状态 
σ
,
σ
′
∈
Σ
σ,σ 
′
 ∈Σ，定义通知函数：

n
o
t
i
f
y
(
σ
,
σ
′
,
d
)
=
{
activating
如果 
σ
⊭
d
∧
σ
′
⊨
d
deactivating
如果 
σ
⊨
d
∧
σ
′
⊭
d
neutral
否则
notify(σ,σ 
′
 ,d)= 
⎩
⎨
⎧
​
  
activating
deactivating
neutral
​
  
如果 σ⊭d∧σ 
′
 ⊨d
如果 σ⊨d∧σ 
′
 ⊭d
否则
​
 
其中 
σ
⊨
d
σ⊨d 表示 
∀
k
∈
d
.
 
k
∈
d
o
m
(
σ
)
∀k∈d. k∈dom(σ)。

工程含义：每当上下文变化（组件加载/卸载导致依赖表变化），运行时计算所有已加载组件的 notify 状态，并相应驱动激活或停用。该特性通过 feature coeffects 启用，核心运行时默认不包含。

5.2.3 依赖作为效果的协同
余效应上下文的变化本身也是效果。set(k, v) 操作的类型恰为 
E
Σ
∗
E 
Σ
∗
​
 ——即可逆效应函数。因此，依赖的注册与撤销自动获得可逆性保证，实现效果与余效应的统一。

六、形式化命题与证明
命题 P1：Action 组合形成幺半群
陈述：
(
Action
,
;
,
1
)
(Action,;,1) 构成一个幺半群。
证明：由公理 A1（结合律）和 A2（单位元）直接得出。
□
□
工程含义：宏可在编译期进行恒等变换优化，而不改变程序语义。

命题 P2：资源不相交的并行组合满足交换律
陈述：若 
Δ
(
a
)
∩
Δ
(
b
)
=
∅
Δ(a)∩Δ(b)=∅ 且无 Write/Own 重叠，则 
a
∥
b
=
b
∥
a
a∥b=b∥a。
证明：资源不相交且访问模式兼容，操作互不影响。
□
□
工程含义：运行时可根据资源冲突矩阵零锁并行调度。

命题 P3：分支写隔离
陈述：Choose 分支中，左分支的 Write 不影响右分支的 Read；Fork 子任务通过 COW 隔离。
证明：由公理 A5 保证。工程上通过 Arc::make_mut（延迟复制）实现。
□
□

命题 P4：可逆操作满足撤销双态
陈述：对于可逆操作 
w
w，执行 
w
w 后执行 
w
ˉ
w
ˉ
 ，资源状态恢复至执行前。
证明：由公理 A6 保证。运行时通过 trackΓ 和 recoverΓ 实现。
□
□

命题 P5：Algeff 调度器无死锁
陈述：不存在任务互相等待资源形成的循环等待链。
证明：静态冲突降级为顺序执行；动态资源采用原子占坑 + 失败回滚 + 有限重试，任务不阻塞等待。
□
□

七、形式化公理与命题的工程映射表
公理/命题	数学表述	Rust 工程实现	验证方式
A1 结合律	
(
a
;
b
)
;
c
=
a
;
(
b
;
c
)
(a;b);c=a;(b;c)	运行时解释器保持 AST 结构	属性测试
A2 单位元	
1
;
a
=
a
1;a=a	Pure 节点被解释器直接跳过	单元测试
A3 交换律	资源不相交 + 无写重叠 ⇒ 可并行	ResourceRegistry::can_parallel()	模型检测
A4 资源线性	Write/Own 恰好消费一次	运行时引用计数 + 线性检查	编译期 + 动态断言
A5 分支隔离	写操作不跨分支传播	Arc::make_mut	并发测试（loom）
A6 撤销双态	
w
;
w
ˉ
=
1
w; 
w
ˉ
 =1	UndoStack + 逆操作闭包	单元测试
A7 无死锁	无循环等待链	原子占坑 + 回滚重试	TLA+ / Apalache
八、代数原语（DSL 名称与语义）
核心不需要任何宏，开发者可以手写 Action 链。以下可选宏仅作为语法糖，提供更简洁的构造方式，不参与类型系统，不增加编译负担。

蓝图原语	拓扑语义	物理实现（默认）	访问模式约束
choose! { cond, then, else }	条件分支	if cond	分支内资源隔离
fork! { left, right }	并发分叉	tokio::spawn	左/右资源集自动分裂
scope!("/tmp", || { ... })	局部路径上下文	PathBuf 堆栈拼接	路径资源作用域化
alloc!(4096)	线性内存分配	Box<[u8]>，延迟复制	生成 MemRange
replace!(new_plan)	蓝图跳转	截断当前流 + RAII Drop	放弃所有资源
invoke!(...)	不透明外部调用	FFI，强制补偿	captures/yields 声明
join!(h1, h2)	等待分支结束	JoinHandle::await	无
sleep!(1s)	逻辑时间等待	tokio::time::sleep	无（逻辑时钟可选）
watch_signal!(SIGINT)	信号监听	tokio::signal	Signal 作为资源
九、内存模型
9.1 访问模式与并发规则
左模式	右模式	判定
Read	Read	✅ 并行（读共享）
Read	Write	❌ 串行
Write	Write	❌ 串行
Append	Append	⚠️ 仅当结果顺序无关时并行，否则串行
Own	任何	❌ 串行（独占所有权）
9.2 Fork 内存行为
内存类型	Fork 时的行为	物理实现
ReadOnly	两个分支共享同一个 Arc<[u8]>	引用计数共享（零拷贝）
Mutable	延迟复制：仅克隆 Arc 句柄，首次写入时触发 clone_data()	用户态 COW（无需 OS 页表）
Own（独占）	仅允许一个分支持有	所有权转移（move）
注意：Algeff 的“延迟复制”以整块复制换取了跨平台一致性，与 OS mmap COW 有本质区别。

十、错误系统
10.1 核心错误枚举（14 种 POSIX 错误）
rust
enum SysError {
    NotFound,           // ENOENT
    PermissionDenied,   // EACCES
    WouldBlock,         // EAGAIN / EWOULDBLOCK
    Interrupted,        // EINTR
    TimedOut,           // ETIMEDOUT
    ConnectionReset,    // ECONNRESET
    ConnectionRefused,  // ECONNREFUSED
    BrokenPipe,         // EPIPE
    StorageFull,        // ENOSPC / EDQUOT
    InvalidInput,       // EINVAL
    AlreadyExists,      // EEXIST
    NotADirectory,      // ENOTDIR
    IsADirectory,       // EISDIR
    CrossDevice,        // EXDEV
    Other(i32),         // 兜底，不参与穷尽性检查
}
10.2 错误处理策略
Catch 强制处理上述 14 种错误。

业务层错误通过 Invoke 的 yields 显式声明，进入 Action 控制流而非 SysError。

Other(i32) 保留原始错误码，但破坏编译期穷尽性检查，用户需自行处理。

十一、撤销系统
11.1 撤销事务模型
所有撤销操作在一个 撤销事务 内执行：

可逆副作用（文件写入、内存分配）→ 自动生成逆操作。

不可逆副作用（UDP 发送、物理设备动作）→ 仅提供补偿挂钩。

补偿级联：若补偿操作失败，进入“人工介入”状态。

11.2 撤销策略分级
策略	行为	适用场景	满足公理 A6
Full	全量写前读，完整回滚	小文件（<1MB）、事务性关键操作	✅
BestEffort	仅记录 LSN，标记脏页	大文件、流式写入	❌
Skip	不撤销，仅警告	临时文件、日志（可重放）	❌
说明：只有 Full 策略被视为可逆操作，满足公理 A6；其他策略仅提供最佳努力或跳过撤销。

十二、运行时架构
12.1 架构层次
text
┌─────────────────────────────────────────────────────┐
│ 业务蓝图（Action AST）                              │ ← 开发者编写
├─────────────────────────────────────────────────────┤
│ 运行时模型（Cordis 理论）                           │ ← 管理逆操作、依赖通知、时间重放
│   - trackΓ / recoverΓ（撤销栈）                     │
│   - notify（组件激活/停用，可选）                   │
│   - VirtualClock（逻辑时间，可选）                  │
├─────────────────────────────────────────────────────┤
│ Rust 编译期安全层（所有权、生命周期、借用检查）      │ ← 编译器保证内存安全
│   - 资源线性由 move + Drop 保证                     │
│   - 内存安全由借用检查器保证                        │
├─────────────────────────────────────────────────────┤
│ 物理执行层（tokio）                                 │ ← 系统调用
└─────────────────────────────────────────────────────┘
12.2 物理后端
Algeff 默认使用 tokio 作为跨平台异步运行时，不直接依赖 mio 或 io_uring。tokio 已封装 epoll/kqueue/IOCP，并支持异步文件、网络、信号、定时器。

平台	网络后端	文件后端
Linux / macOS / Windows	tokio::net（基于 mio）	tokio::fs（线程池）
如需更高性能的文件 IO，可启用 io_uring feature（仅 Linux），但会增加构建复杂度，默认不启用。

12.3 运行时核心结构
rust
pub struct Runtime {
    // 当前上下文（含状态和累积逆变换）
    context: Context,
    // 撤销栈（逆操作闭包）
    undo_stack: Vec<UndoOp>,
    // 依赖表（余效应上下文，可选 feature）
    dependency_table: Option<DependencyTable>,
    // 已加载组件列表（可选）
    loaded_components: Option<Vec<Component>>,
    // 逻辑时钟（可选 feature）
    virtual_clock: Option<VirtualClock>,
    // 资源注册表（全局标识分配与冲突检测）
    resource_registry: ResourceRegistry,
    // 异步运行时
    reactor: tokio::runtime::Runtime,
}
注意：没有泛型参数、生命周期标记或常量泛型。所有复杂度由运行时内部管理。

十三、宏的使用（可选）
Algeff 核心不依赖任何宏。以下宏仅为可选语法糖，位于独立 crate algeff-macro 中，可按需启用：

宏	职责	复杂度
algeff::plan!	辅助构造 Action::Sequential 链	简单（~100 行展开）
algeff::fork!	辅助构造 Action::Fork	简单（~30 行展开）
algeff::scope!	辅助构造 Action::Scope	简单（~30 行展开）
algeff::choose!	辅助构造 Action::Choose	简单（~30 行展开）
不再需要：

❌ GADT 模拟（enum-typer）

❌ 常量泛型区间检查（const_assert）

❌ 生命周期标记（PhantomData 仅在类型状态资源包装中使用，非核心）

❌ 线性类型模拟（禁用 Copy + #[must_use]）

所有线性逻辑由 Rust 原生所有权 + 运行时 trackΓ 模型共同保证。

十四、使用示例
rust
use algeff::prelude::*;

#[algeff::blueprint] // 可选入口标记，普通函数也可
async fn main_blueprint() -> Result<(), SysError> {
    let listener = open_tcp("0.0.0.0:8080")?;

    // fork! 展开为 Action::Fork
    let (h1, h2) = fork! {
        left: handle_client(listener.accept()?),
        right: handle_client(listener.accept()?)
    };
    join!(h1, h2)?;

    // scope! 展开为 Action::Scope
    scope!("/var/log/myapp", || {
        write("shutdown.log", b"Server stopped")?;
        Ok(())
    })?;

    sleep!(Duration::from_secs(1))?;
    replace!(shutdown_blueprint())?;

    Ok(())
}

fn handle_client((fd, addr): (TcpStream, SocketAddr)) -> Action {
    // 使用类型安全资源包装
    let read_res = Resource::new_read(ResourceInner::Fd(fd));
    let write_res = Resource::new_write(ResourceInner::Fd(fd));
    let own_res = Resource::new_owned(ResourceInner::Fd(fd));

    Action::Sequential {
        current: Box::new(Action::Syscall {
            op: DataOp::Read { fd, len: 1024 },
            resources: vec![read_res.into_usage()],
            next: Box::new(|data| Action::Syscall {
                op: DataOp::Write { fd, data },
                resources: vec![write_res.into_usage()],
                next: Box::new(|_| Action::Syscall {
                    op: DataOp::Close { fd },
                    resources: vec![own_res.into_usage()],
                    next: Box::new(|_| Action::Pure(Value::Unit)),
                }),
            }),
        }),
        next: Box::new(|_| Action::Pure(Value::Unit)),
    }
}
十五、三层发布结构
Crate	职责	代码量估算	稳定性
algeff-core	Action、ResourceSet、Resource<M>、Runtime 内核	~2000 行	永久冻结
algeff-std	预包装适配层（open_tcp、read 等）	~2500 行	永久稳定
algeff-macro	极简语法糖宏（可选）	~300 行	极少修改
十六、性能预期
场景	原生 tokio	Algeff（静态路径）	Algeff（动态资源）
网络 Echo（无共享资源）	100%	~103%	~110%
并行读取 10 个不同文件	100%	~100%（零锁）	~100%（零锁）
并行追加同一文件（顺序无关）	100%	~100%	~105%
并行读取同一文件（只读共享）	100%	~100%（读-读可并行）	~100%
十七、已知局限
问题	状态	说明
物理层进展属性	未证明	磁盘 IO、网络延迟不在代数证明范围内
补偿操作的原子性	用户承诺	Invoke 补偿闭包的正确性由用户负责
Other(i32) 错误穷尽性	削弱	编译期穷尽性检查被 Other 兜底破坏
大文件写入撤销的双倍 IO	工程缓解	BestEffort 策略（LSN + 脏页标记）
动态资源仲裁的锁竞争	工程缓解	预分配池 + io_uring 注册缓冲区（可选）
Windows 兼容性	部分支持	文件后端使用线程池，性能低于 Linux 专用接口
十八、安全契约与用户责任
Algeff 在运行时提供业务语义安全，但某些正确性依赖用户遵守规范：

维度	Algeff 保证	用户责任
内存安全	Rust 编译器强制	不使用 unsafe
资源线性	Rust 所有权 + Drop 保证物理释放	正确声明 ResourceSet，不绕过运行时直接操作底层资源
业务可逆性	trackΓ / recoverΓ 保证（仅 Full 策略）	为不可逆操作提供补偿闭包或接受 Skip
并发安全	无死锁调度保证	正确声明资源访问模式，不隐瞒依赖
确定性	蓝图执行路径由输入唯一决定	不在 Invoke 中引入外部非确定性
错误处理	14 种 POSIX 错误强制处理	处理 Other(i32) 兜底错误
增强措施：类型状态资源包装（Resource<M>）作为推荐 API 可降低声明错误，但不能完全阻止绕过。

十九、8 Agent 并行 Loop 闭环开发实现路线
19.1 角色定义
Agent ID	名称	职责	交付物
A1	Spec Guardian	维护形式化规范，审计一致性，更新公理证明	spec/、proofs/
A2	Core Runtime	实现 Action 解释器、Context、UndoStack	algeff-core/runtime.rs
A3	Resource & Coeffects	实现 ResourceSet、冲突检测、依赖表 notify（可选）	algeff-core/resource.rs
A4	DSL & Macro	实现可选语法糖宏	algeff-macro/
A5	Std Adapters	实现数据平面原语（文件、网络、内存等）	algeff-std/
A6	Verification	属性测试、模型检测、形式化证明检查	tests/、tla/
A7	Integration & Perf	跨平台后端、基准测试、性能回归	benches/、ci/perf
A8	DevOps & CI	管理并行分支、自动合并、文档生成、发布	.github/workflows/
19.2 并行 Loop 闭环机制
接口冻结：A1 定义核心类型签名和公理，发布 contracts.md。

并行开发：A2-A5 在独立分支实现各自模块，通过 trait 契约交互。

持续集成：A8 配置 CI，每次推送自动运行 A6 测试套件和 A7 基准测试。

反馈循环：测试失败或性能不达标 → 自动创建 Issue → 对应 Agent 修复。

每周同步：A1 审查所有模块是否符合形式化公理，更新规范；接口变更走 RFC 流程。

闭环收敛：当所有公理被证明/测试覆盖，性能满足预期，合并主分支，冻结 API。

19.3 阶段划分
阶段	时长	主要任务
阶段 0：契约冻结	1 周	A1 输出核心类型与公理；A8 搭建 monorepo 和 CI
阶段 1：核心实现	2 周	A2-A5 并行实现各自模块；A6 编写第一批属性测试
阶段 2：集成与验证	2 周	合并代码；A6 运行 proptest/loom/TLA+；A1 修订公理
阶段 3：优化与跨平台	2 周	A7 优化 tokio 后端；A3 优化锁竞争；A6 并发压力测试
阶段 4：发布与冻结	1 周	A8 生成文档、发布 algeff-core 0.1.0；A1 冻结 spec
19.4 工具链
用途	工具
属性测试	proptest
并发测试	loom
模型检测	TLA+ / Apalache
形式化证明	Coq（可选）
基准测试	criterion
CI/CD	GitHub Actions
代码审查	reviewdog
文档	mdBook
19.5 关键风险与应对
风险	应对
接口频繁变更导致返工	阶段 0 冻结契约，RFC 流程变更
性能不达标	A7 提前介入，基准测试驱动优化
形式化证明过于耗时	A6 以属性测试为主，TLA+ 只覆盖调度器
Agent 之间沟通不畅	A8 每周同步会议 + 共享设计文档
二十、结论
Algeff v3.2 是一个 全面覆盖 Unix 效应、实现简洁、分层安全 的确定性运行时框架：

效应代数化：Action 枚举覆盖文件、网络、进程、信号、内存等绝大多数 Unix 效应，统一 CPS 组合。

运行时代数模型：trackΓ / recoverΓ 保证可逆性，notify 提供反应式余效应（可选），VirtualClock 支持确定性重放（可选）。

实现极简：无复杂宏，无编译期类型体操，仅依赖 tokio，编译时间秒级。

类型安全辅助：类型状态资源包装（Resource<M>）以无宏方式降低用户误用，保留灵活性。

分层安全：Rust 编译器保证内存安全；运行时模型保证业务语义安全；API 层辅助正确声明。

最终信条

“Rust 编译器保证内存安全。运行时模型保证业务语义安全。开发者只写蓝图，剩下的交给模型。”

