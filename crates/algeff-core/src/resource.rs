//! 资源集、访问模式与类型安全包装 —— 契约冻结（pdr.md §2.3 / §3 / §9）。
//!
//! A3 拥有本文件的演进：冲突检测细化、coeffects、测试与边界打磨
//! （契约内变更可直接做；API 签名变更需 RFC → CTO）。

use std::collections::{HashMap, HashSet};
use std::marker::PhantomData;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use crate::action::Fd;
use crate::error::SysError;

/// 资源标识。Path 必须规范化（绝对路径、消除 `..`，见 `ResourceRegistry::canonicalize_path`）；
/// Fd 由运行时分配全局唯一句柄（非 OS fd）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Resource {
    Fd(Fd),
    Path(String),
    MemRange(usize, usize),
    Pid(u32),
    Signal,
    Foreign(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Read,
    Write,
    Append,
    Own,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceUsage {
    pub resource: Resource,
    pub mode: AccessMode,
}

pub type ResourceSet = Vec<ResourceUsage>;

// ── 类型状态包装（pdr.md §3，无宏）────────────────────────────────────

pub struct ReadOnly;
pub struct WriteOnly;
pub struct AppendOnly;
pub struct Owned;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceInner {
    Fd(Fd),
    Path(PathBuf),
    MemRange(usize, usize),
    Pid(u32),
    Signal,
    Foreign(u64),
}

impl From<ResourceInner> for Resource {
    fn from(inner: ResourceInner) -> Self {
        match inner {
            ResourceInner::Fd(fd) => Resource::Fd(fd),
            ResourceInner::Path(p) => Resource::Path(p.to_string_lossy().into_owned()),
            ResourceInner::MemRange(a, b) => Resource::MemRange(a, b),
            ResourceInner::Pid(pid) => Resource::Pid(pid),
            ResourceInner::Signal => Resource::Signal,
            ResourceInner::Foreign(id) => Resource::Foreign(id),
        }
    }
}

/// 类型状态资源包装（pdr.md §3，契约决策 D7：与 `Resource` 枚举同名冲突，
/// 故命名为 `TypedResource<M>`）。
pub struct TypedResource<M> {
    inner: ResourceInner,
    _mode: PhantomData<M>,
}

impl<M> TypedResource<M> {
    pub fn inner(&self) -> &ResourceInner {
        &self.inner
    }
}

impl TypedResource<ReadOnly> {
    pub fn new_read(inner: ResourceInner) -> Self {
        Self {
            inner,
            _mode: PhantomData,
        }
    }
    pub fn into_write(self) -> TypedResource<WriteOnly> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
    pub fn into_append(self) -> TypedResource<AppendOnly> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
    pub fn into_owned(self) -> TypedResource<Owned> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
}

impl TypedResource<WriteOnly> {
    pub fn new_write(inner: ResourceInner) -> Self {
        Self {
            inner,
            _mode: PhantomData,
        }
    }
    pub fn into_read(self) -> TypedResource<ReadOnly> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
    pub fn into_owned(self) -> TypedResource<Owned> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
}

impl TypedResource<AppendOnly> {
    pub fn new_append(inner: ResourceInner) -> Self {
        Self {
            inner,
            _mode: PhantomData,
        }
    }
    pub fn into_read(self) -> TypedResource<ReadOnly> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
    pub fn into_owned(self) -> TypedResource<Owned> {
        TypedResource {
            inner: self.inner,
            _mode: PhantomData,
        }
    }
}

impl TypedResource<Owned> {
    pub fn new_owned(inner: ResourceInner) -> Self {
        Self {
            inner,
            _mode: PhantomData,
        }
    }
    // Owned 不能降级为 Read/Write，防止意外共享（pdr.md §3.3）。
}

pub trait ModeMarker {
    fn access_mode() -> AccessMode;
}

impl ModeMarker for ReadOnly {
    fn access_mode() -> AccessMode {
        AccessMode::Read
    }
}
impl ModeMarker for WriteOnly {
    fn access_mode() -> AccessMode {
        AccessMode::Write
    }
}
impl ModeMarker for AppendOnly {
    fn access_mode() -> AccessMode {
        AccessMode::Append
    }
}
impl ModeMarker for Owned {
    fn access_mode() -> AccessMode {
        AccessMode::Own
    }
}

impl<M: ModeMarker> TypedResource<M> {
    pub fn into_usage(self) -> ResourceUsage {
        ResourceUsage {
            resource: self.inner.into(),
            mode: M::access_mode(),
        }
    }
}

// ── 物理句柄与资源注册表 ──────────────────────────────────────────────

/// 注册表持有的物理资源。所有变体 Arc 共享，便于 Dup/Fork COW。
#[derive(Debug, Clone)]
pub enum ResourceHandle {
    File(Arc<tokio::fs::File>),
    TcpListener(Arc<tokio::net::TcpListener>),
    TcpStream(Arc<tokio::net::TcpStream>),
    UdpSocket(Arc<tokio::net::UdpSocket>),
    PipeReader(Arc<tokio::io::ReadHalf<tokio::io::DuplexStream>>),
    PipeWriter(Arc<tokio::io::WriteHalf<tokio::io::DuplexStream>>),
    Mutex(Arc<tokio::sync::Mutex<()>>),
    Child(Arc<tokio::process::Child>),
}

/// 全局标识分配与冲突检测（pdr.md §2.3 / §12.3，公理 A3/A4/A7 的工程载体）。
/// 实现 Clone：Fork 并行时子任务隔离状态，完成后合并回主 registry（决策 D13）。
#[derive(Default, Clone)]
pub struct ResourceRegistry {
    next_fd: Fd,
    handles: HashMap<Fd, ResourceHandle>,
    /// A4 线性检查：Write 已消费的资源（每资源至多一次）。
    consumed: HashSet<Resource>,
    /// A4 线性检查：Own 已终结的资源（Own 之后该资源任何 usage 都拒绝）。
    owned_consumed: HashSet<Resource>,
    /// Fork 分支预留 fd 区间（F1 修复 + S6/A2 嵌套修复，由 `offset_next_fd`
    /// 记录）：`(未偏移基线, 偏移量)`。基线经 Clone 沿分支树传播：右分支首次
    /// 偏移以当前 `next_fd` 为基线，嵌套 Fork 的右分支沿用同一基线（见
    /// `offset_next_fd`）—— 区间只由全局唯一偏移序号决定，与路径无关。
    /// `merge` 归一化时若预留区间**从未实际分配**（`next_fd` 恰为 基线+偏移，
    /// 分配游标未被任何 `allocate` 移动），其游标收敛回基线 —— 否则「右分支
    /// 未分配新 fd」时父 `next_fd` 被大常数永久抬高（破坏 D1 单调序列紧凑性，
    /// 如 concurrency_stress 的 fd 序列断言 [1,2,3]）。一旦发生过任何分配，
    /// 游标只升不降（分配的 fd 可能已逃逸到用户值/执行器轮换映射，不得复用，
    /// D1）。
    fork_region: Option<(Fd, Fd)>,
}

/// A4 线性状态快照（取消传播协议，RFC-08/09/12 残余修复用）：捕获
/// `ResourceRegistry` 的 `consumed`（Write 消费）与 `owned_consumed`
/// （Own 终结）两集。由 `snapshot_linear` 产生、`rollback_linear_to`
/// 消费（按「时段」回滚取消子树新增的线性标记）。
#[derive(Default, Clone)]
pub struct LinearSnapshot {
    consumed: HashSet<Resource>,
    owned_consumed: HashSet<Resource>,
}

impl ResourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 分配全局唯一句柄（单调递增，永不复用）。
    pub fn allocate(&mut self, handle: ResourceHandle) -> Fd {
        let fd = self.next_fd;
        self.next_fd += 1;
        self.handles.insert(fd, handle);
        fd
    }

    pub fn lookup(&self, fd: Fd) -> Option<&ResourceHandle> {
        self.handles.get(&fd)
    }

    /// 取出句柄（Own 语义：Close/替换时使用）。
    pub fn take(&mut self, fd: Fd) -> Option<ResourceHandle> {
        self.handles.remove(&fd)
    }

    pub fn remove(&mut self, fd: Fd) {
        self.handles.remove(&fd);
    }

    /// 清空注册表状态（handles/consumed/owned_consumed），供 A2 的 `Replace`
    /// （决策 D10）使用：替换蓝图时释放当前执行路径积累的资源与线性标记。
    /// 注意：`next_fd` 不复位 —— Fd 全局唯一、单调递增、永不复用（决策 D1）。
    pub fn clear(&mut self) {
        self.handles.clear();
        self.consumed.clear();
        self.owned_consumed.clear();
    }

    /// Fork 分支 fd 区间预分割（F1 审查修复 + S6/A2 嵌套修复）：把 `next_fd`
    /// 移动到 `基线 + offset`，使本注册表后续分配的 fd 落入高位区间，与未偏移
    /// 的注册表（父/左分支）分配互不重叠；同时记录 `(基线, offset)` 供 `merge`
    /// 归一化（未实际分配时收敛回基线，见 `fork_region`）。
    ///
    /// 基线选择（S6/A2 嵌套修复）：本注册表**已是某 Fork 右分支**（`fork_region`
    /// 已记录）时，沿用其**未偏移基线**而非当前已偏移游标 —— 否则区间偏移沿
    /// 路径累加（绝对位置 = 根基线 + Σ k_i·2^48），两条不同路径的序号和可能
    /// 相等 → 并发区间碰撞。沿用未偏移基线后，区间只取决于全局唯一偏移序号
    /// 与根基线，**与路径无关**：任意嵌套深度下所有并发分支区间互斥。
    ///
    /// 背景：Fork 并行/顺序路径的左右分支都克隆自父（同源 `next_fd`），若两分支
    /// 都分配新 fd 会得到**相同 fd** —— merge 时 `HashMap::extend` 静默覆盖丢弃
    /// 一侧句柄，执行器内部轮换映射同样碰撞。调用方（A2 解释器）在 spawn 前给
    /// 右分支偏移全局唯一大常数（`k << 48`，远离任何实际 fd 规模），即得不相交
    /// 区间。
    ///
    /// 注意：只移动分配游标，**已分配的句柄 fd 不变**（fd 身份保留，D13 merge
    /// 以原 fd 并入后仍可 lookup；分支返回值中携带的 Fd 同样指向同一句柄）。
    pub fn offset_next_fd(&mut self, offset: Fd) {
        let base = match self.fork_region {
            Some((base, _)) => base,
            None => self.next_fd,
        };
        self.fork_region = Some((base, offset));
        self.next_fd = base + offset;
    }

    /// 合并另一个注册表的状态（决策 D13「完成后合并回父」，RFC-A3-2 / A1
    /// 审计偏差-1 落地）：`other` 的全部句柄按**原 fd** 直接插入（fd 不冲突由
    /// D1 单调性 + F1/S6/A2 全局唯一区间预分割保证：`other` 克隆自 `self` 或其
    /// 子，新分配的 fd 均 ≥ 自身 `next_fd`，且 Fork 分支经 `offset_next_fd`
    /// 全局唯一区间预分割后任意并发分支（含嵌套任意深度）区间互不重叠）、`consumed`/`owned_consumed` 取并集、`next_fd` 归一化
    /// 为 `max(self.next_fd, other.next_fd)`（即全部已分配 fd 的最大值 + 1，
    /// 父继续分配不冲突、不复用）。归一化细节（F1 修复配套）：`other` 若经
    /// `offset_next_fd` 预留高位区间但**从未实际分配**，其游标收敛回基线，
    /// 避免父 `next_fd` 被大常数永久抬高（见 `fork_region`）。
    ///
    /// 由 A2 解释器在 Fork 并行/顺序分支完成后调用（D14 升级 + F1/F2 修复）；
    /// 本方法为加法 API，不改变任何既有方法签名。
    pub fn merge(&mut self, other: Self) {
        self.handles.extend(other.handles);
        self.consumed.extend(other.consumed);
        self.owned_consumed.extend(other.owned_consumed);
        // F1 归一化：预留区间未实际分配（next_fd 恰为 基线+偏移）→ 收敛回基线。
        let other_next = match other.fork_region {
            Some((base, offset)) if other.next_fd == base + offset => base,
            _ => other.next_fd,
        };
        self.next_fd = self.next_fd.max(other_next);
    }

    /// 公理 A4 线性检查（运行时断言）：
    /// - Write：每资源至多一次（重复 Write 拒绝）；
    /// - Own：终结操作（Own 之后该资源任何 usage —— Read/Write/Append/Own —— 都拒绝）；
    /// - Read/Append：不限次数。
    ///
    /// Write 与 Own 分属不同消费集：`Write → Close(Own)` 是合法序列（pdr.md §14 示例）。
    pub fn check_linear(&mut self, usage: &ResourceUsage) -> Result<(), SysError> {
        let r = &usage.resource;
        // Own 为终结：先于一切模式检查。
        if self.owned_consumed.contains(r) {
            return Err(SysError::InvalidInput);
        }
        match usage.mode {
            AccessMode::Write => {
                if !self.consumed.insert(r.clone()) {
                    return Err(SysError::InvalidInput);
                }
            }
            AccessMode::Own => {
                self.owned_consumed.insert(r.clone());
            }
            AccessMode::Read | AccessMode::Append => {}
        }
        Ok(())
    }

    /// 失败路径线性标记回滚（RFC-12，R6-F2 修复）：`check_linear` 在 syscall
    /// **执行前**预插入 Write/Own 消费标记；物理执行失败时这些标记必须回滚
    /// （与 A7 仲裁「失败回滚」同原则）——否则失败后同路径再以 Write 模式
    /// 重试会被 A4 误拒（`InvalidInput`，线性标记残留毒化）。
    ///
    /// 前置条件：对传入切片内的**每个** usage，`check_linear` 均已返回 Ok
    /// （即：全部成功——或批内部分失败时对成功前缀调用）。此时每个 Write/Own
    /// 标记都是本批新插入的（Write 至多一次、Own 终结——重复插入会返回 Err 且
    /// 不进入执行阶段），故恰好移除一个标记是安全的：不会误删早前成功 syscall
    /// 的消费记录。Read/Append 不插标记，无操作。成功路径行为不变（公理 A4：
    /// Write/Own 恰好消费一次）。
    pub fn rollback_linear(&mut self, resources: &[ResourceUsage]) {
        for u in resources {
            match u.mode {
                AccessMode::Write => {
                    self.consumed.remove(&u.resource);
                }
                AccessMode::Own => {
                    self.owned_consumed.remove(&u.resource);
                }
                AccessMode::Read | AccessMode::Append => {}
            }
        }
    }

    /// 捕获当前 A4 线性状态快照（取消传播协议，RFC-08/09/12 残余修复用）：
    /// `check_linear` 预插入的 Write（`consumed`）/ Own（`owned_consumed`）
    /// 消费标记两集。快照**不含**句柄表与 `next_fd`——取消回滚只移除取消
    /// 子树新插入的线性标记，不动句柄（物理清理由 undo 负责）与 D1 单调
    /// 游标（fd 永不复用）。
    pub fn snapshot_linear(&self) -> LinearSnapshot {
        LinearSnapshot {
            consumed: self.consumed.clone(),
            owned_consumed: self.owned_consumed.clone(),
        }
    }

    /// 回滚线性状态至快照（取消传播协议，`Action::Timeout` 取消回滚用，
    /// RFC-12 残余）：**移除**取消子树期间新增的 Write/Own 标记（当前集 ∖
    /// 快照集），子树自身移除的标记（如 Replace 的 `clear`、失败路径的
    /// `rollback_linear`）保持移除、不回补——只撤销新增，尊重子树自身的
    /// 状态操作。与 `rollback_linear`（逐批回滚）互补：快照回滚按「时段」
    /// 而非「批次」工作，适用于无法逐批跟踪的取消路径（inner future 被
    /// 丢弃/中断后不再有逐批上下文）。
    pub fn rollback_linear_to(&mut self, snap: &LinearSnapshot) {
        self.consumed.retain(|r| snap.consumed.contains(r));
        self.owned_consumed.retain(|r| snap.owned_consumed.contains(r));
    }

    /// 公理 A3 / 冲突矩阵（pdr.md §9.1）。保守默认：Append∥Append 视为不可并行
    /// （除非调用方声明顺序无关）。
    pub fn can_parallel(&self, a: &ResourceSet, b: &ResourceSet) -> bool {
        self.can_parallel_with(a, b, false)
    }

    pub fn can_parallel_with(
        &self,
        a: &ResourceSet,
        b: &ResourceSet,
        append_order_insensitive: bool,
    ) -> bool {
        for ua in a {
            for ub in b {
                if ua.resource != ub.resource {
                    continue;
                }
                match (ua.mode, ub.mode) {
                    (AccessMode::Read, AccessMode::Read) => {}
                    (AccessMode::Append, AccessMode::Append) => {
                        if !append_order_insensitive {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
        }
        true
    }

    /// 路径规范化：绝对化 + 消除 `.`/`..`（词法，不触碰真实文件系统，
    /// 保证确定性；符号链接解析留给物理执行层）。
    pub fn canonicalize_path(&self, p: &Path, cwd: &Path) -> PathBuf {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        let mut out = PathBuf::new();
        for comp in abs.components() {
            match comp {
                Component::CurDir => {}
                Component::ParentDir => {
                    out.pop();
                }
                other => out.push(other.as_os_str()),
            }
        }
        out
    }
}

// ── 动态资源仲裁（公理 A7 的工程载体）────────────────────────────────────

/// 独占占坑标记（Write/Own/Append 持有）。见 `ResourceArbiter` 的占坑计数编码。
const EXCLUSIVE_CLAIM: usize = usize::MAX;

/// 动态资源仲裁原语：原子占坑 + 失败回滚 + 有限重试（pdr.md 公理 A7 的工程载体）。
///
/// 与静态层 `ResourceRegistry::can_parallel` 分层（pdr.md §9.1）：
/// - **静态层（Fork 级，零锁）**：调度前基于 `ResourceSet` 的冲突矩阵判定，
///   冲突 → 降级串行（公理 A3「否则串行」），从不进入等待，无死锁；
/// - **动态层（MutexLock 级）**：运行期互斥资源（如 `DataOp::MutexLock`）由
///   `try_claim` 原子占坑，失败**整体回滚**，调用方**有限重试**（失败回滚 +
///   有限重试，A7）。本原语**不提供阻塞等待**——不等待 = 不存在循环等待链
///   （命题 P5；同步互斥的 `.lock().await` 不得在解释器任务内直接使用）。
///
/// 仲裁表语义：`claims` 是动态占坑表，记录**所有当前占坑者**的占用；
/// 后续 `try_claim` 与该表比对，冲突即失败。解释器单线程（trampoline）内
/// 以 `&mut self` 串行访问即天然互斥；Fork 并行时按 D13 克隆隔离。
/// 跨任务的实际互斥由物理层（如 `tokio::sync::Mutex::try_lock`，A5 执行器）
/// 保证，本原语负责 set 级原子性：失败不残留部分占坑，可安全重试。
///
/// 占用模式对（对齐 pdr.md §9.1 冲突矩阵）：
/// - `Read` 可共享：同一资源可被多个 Read 占坑（占坑计数累加）；
/// - `Write` / `Own` 互斥：资源已被任何模式占坑时拒绝新占坑（独占）；
/// - `Append` 按互斥处理（保守默认，对齐决策 D6：Append∥Append 默认串行，
///   顺序无关的 opt-in 由静态层 `can_parallel_with` 表达）。
///
/// 占坑计数编码（`claims: HashMap<Resource, usize>`）：
/// - `0`：未占坑；`1..=usize::MAX-1`：Read 占坑数；`usize::MAX`：独占占坑标记。
///   独占与 Read 互斥，故任一资源要么是纯 Read 计数、要么是独占标记，不会混叠。
#[derive(Debug, Default, Clone)]
pub struct ResourceArbiter {
    claims: HashMap<Resource, usize>,
}

impl ResourceArbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 对 `set` 中每个资源尝试原子占坑：全部可占才成功，否则**整体回滚**
    /// 已尝试的占坑（原子性，A7「原子占坑 + 失败回滚」）。
    ///
    /// 失败时自身状态完全不变（无部分占坑残留），调用方可直接有限重试，
    /// 无需手动回滚。实现：先在副本上模拟全部占坑，全部成功才提交。
    pub fn try_claim(&mut self, set: &ResourceSet) -> bool {
        let mut trial = self.claims.clone();
        for usage in set {
            if !Self::claim_one(&mut trial, usage) {
                return false; // trial 被丢弃：self 状态不变（整体回滚）
            }
        }
        self.claims = trial;
        true
    }

    /// 在给定计数表上尝试单个占坑（Read 共享 / Write·Own·Append 独占）。
    fn claim_one(claims: &mut HashMap<Resource, usize>, usage: &ResourceUsage) -> bool {
        let count = claims.entry(usage.resource.clone()).or_insert(0);
        match usage.mode {
            AccessMode::Read => {
                if *count == EXCLUSIVE_CLAIM {
                    return false; // 已被独占占坑，Read 不可共享
                }
                *count += 1;
            }
            AccessMode::Write | AccessMode::Own | AccessMode::Append => {
                if *count != 0 {
                    return false; // 已被任何模式占坑，互斥
                }
                *count = EXCLUSIVE_CLAIM;
            }
        }
        true
    }

    /// 释放 `set` 中每个资源的占坑（Read 递减计数；独占直接清空）。
    /// 幂等：未占坑的资源被忽略，不会 panic。
    pub fn release(&mut self, set: &ResourceSet) {
        for usage in set {
            let r = &usage.resource;
            match self.claims.get(r).copied() {
                Some(EXCLUSIVE_CLAIM) => {
                    self.claims.remove(r);
                }
                Some(n) if n > 0 => {
                    let next = n - 1;
                    if next == 0 {
                        self.claims.remove(r);
                    } else {
                        self.claims.insert(r.clone(), next);
                    }
                }
                _ => {}
            }
        }
    }

    /// 资源当前是否被本仲裁器占坑（Read 计数 > 0 或独占）。
    pub fn held(&self, r: &Resource) -> bool {
        self.claims.get(r).is_some_and(|&c| c != 0)
    }

    /// 仲裁表是否为空（无任何占坑）。供泄漏检测断言：全部 `release` 后应返回
    /// `true`（批 4 属性测试的终止不变量）。
    pub fn is_clean(&self) -> bool {
        self.claims.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(r: Resource, m: AccessMode) -> ResourceUsage {
        ResourceUsage {
            resource: r,
            mode: m,
        }
    }

    #[test]
    fn typestate_usage_mode_matches() {
        let u = TypedResource::<ReadOnly>::new_read(ResourceInner::Fd(1)).into_usage();
        assert_eq!(u.mode, AccessMode::Read);
        let u = TypedResource::<WriteOnly>::new_write(ResourceInner::Fd(1)).into_usage();
        assert_eq!(u.mode, AccessMode::Write);
        let u = TypedResource::<Owned>::new_owned(ResourceInner::Fd(1)).into_usage();
        assert_eq!(u.mode, AccessMode::Own);
    }

    #[test]
    fn conflict_matrix_read_read_ok() {
        let a = vec![usage(Resource::Fd(1), AccessMode::Read)];
        let b = vec![usage(Resource::Fd(1), AccessMode::Read)];
        let reg = ResourceRegistry::new();
        assert!(reg.can_parallel(&a, &b));
    }

    #[test]
    fn conflict_matrix_write_blocks() {
        let r = Resource::Fd(1);
        let reg = ResourceRegistry::new();
        assert!(!reg.can_parallel(
            &vec![usage(r.clone(), AccessMode::Read)],
            &vec![usage(r.clone(), AccessMode::Write)]
        ));
        assert!(!reg.can_parallel(
            &vec![usage(r.clone(), AccessMode::Write)],
            &vec![usage(r.clone(), AccessMode::Write)]
        ));
        assert!(!reg.can_parallel(
            &vec![usage(r.clone(), AccessMode::Own)],
            &vec![usage(r.clone(), AccessMode::Read)]
        ));
    }

    #[test]
    fn disjoint_resources_parallel() {
        let a = vec![usage(Resource::Fd(1), AccessMode::Write)];
        let b = vec![usage(Resource::Fd(2), AccessMode::Write)];
        let reg = ResourceRegistry::new();
        assert!(reg.can_parallel(&a, &b));
    }

    #[test]
    fn append_parallel_needs_opt_in() {
        let r = Resource::Fd(1);
        let a = vec![usage(r.clone(), AccessMode::Append)];
        let b = vec![usage(r.clone(), AccessMode::Append)];
        let reg = ResourceRegistry::new();
        assert!(!reg.can_parallel(&a, &b));
        assert!(reg.can_parallel_with(&a, &b, true));
    }

    #[test]
    fn linearity_double_write_rejected() {
        let mut reg = ResourceRegistry::new();
        let u = usage(Resource::Fd(1), AccessMode::Write);
        assert!(reg.check_linear(&u).is_ok());
        assert_eq!(reg.check_linear(&u), Err(SysError::InvalidInput));
    }

    #[test]
    fn linearity_write_then_own_legal() {
        // Write → Close(Own) 是合法序列（pdr.md §14 示例）：Write 消费一次，
        // Own 终结，二者互不排斥。
        let mut reg = ResourceRegistry::new();
        let r = Resource::Fd(1);
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok());
        assert!(reg.check_linear(&usage(r.clone(), AccessMode::Own)).is_ok());
    }

    #[test]
    fn linearity_own_is_terminal() {
        let mut reg = ResourceRegistry::new();
        let r = Resource::Fd(1);
        assert!(reg.check_linear(&usage(r.clone(), AccessMode::Own)).is_ok());
        // Own 之后任何 usage（Read/Write/Append/Own）都拒绝
        for mode in [
            AccessMode::Read,
            AccessMode::Write,
            AccessMode::Append,
            AccessMode::Own,
        ] {
            assert_eq!(
                reg.check_linear(&usage(r.clone(), mode)),
                Err(SysError::InvalidInput),
                "Own 之后 {mode:?} 应被拒绝"
            );
        }
    }

    #[test]
    fn linearity_read_append_repeatable() {
        let mut reg = ResourceRegistry::new();
        let r = Resource::Fd(1);
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Read))
            .is_ok());
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Read))
            .is_ok());
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Append))
            .is_ok());
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Append))
            .is_ok());
    }

    #[test]
    fn clear_resets_linear_state_and_handles() {
        let mut reg = ResourceRegistry::new();
        let fd = reg.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        let r = Resource::Fd(fd);
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok());
        assert!(reg.check_linear(&usage(r.clone(), AccessMode::Own)).is_ok());
        assert_eq!(
            reg.check_linear(&usage(r.clone(), AccessMode::Read)),
            Err(SysError::InvalidInput)
        );

        reg.clear();
        // 句柄与线性标记全部清空；fd 分配仍单调递增（不重用）
        assert!(reg.lookup(fd).is_none());
        assert!(reg
            .check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok());
        assert!(reg.check_linear(&usage(r.clone(), AccessMode::Own)).is_ok());
    }

    #[test]
    fn merge_preserves_fd_identity() {
        // D13 合并（RFC-A3-2）：子注册表句柄以**原 fd** 直接并入父，fd 身份保留
        // （区别于 take+allocate 值迁移的 fd 重分配 workaround）。
        let mut parent = ResourceRegistry::new();
        let p1 = parent.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        let mut child = parent.clone();
        let c1 = child.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        assert!(parent.lookup(c1).is_none(), "合并前父不可见子句柄");
        parent.merge(child);
        assert!(parent.lookup(p1).is_some(), "父原有句柄保留");
        assert!(
            parent.lookup(c1).is_some(),
            "合并后父以原 fd 可见子句柄（fd 身份保留）"
        );
    }

    #[test]
    fn merge_unions_consumed() {
        // 子路径的线性消费（Write 至多一次 + Own 终结）随 merge 并入父：
        // 合并后父侧同键检查与子侧一致（spec/axioms.md 提示的 consumed 合并）。
        let mut parent = ResourceRegistry::new();
        let mut child = parent.clone();
        let r = Resource::Fd(1);
        assert!(child
            .check_linear(&usage(r.clone(), AccessMode::Write))
            .is_ok());
        assert!(child
            .check_linear(&usage(r.clone(), AccessMode::Own))
            .is_ok());
        // 合并前父对该键无消费记录（Read 通过）
        assert!(parent
            .check_linear(&usage(r.clone(), AccessMode::Read))
            .is_ok());
        parent.merge(child);
        // 合并后：Write 已消费（再 Write 拒绝）+ Own 已终结（任何 usage 拒绝）
        assert_eq!(
            parent.check_linear(&usage(r.clone(), AccessMode::Write)),
            Err(SysError::InvalidInput),
            "子路径 Write 消费记录应并入父"
        );
        assert_eq!(
            parent.check_linear(&usage(r.clone(), AccessMode::Read)),
            Err(SysError::InvalidInput),
            "子路径 Own 终结记录应并入父"
        );
    }

    #[test]
    fn merge_advances_next_fd() {
        // 子侧分配的 fd 高于父 next_fd：merge 后父 next_fd = max，
        // 父继续分配不会与子侧 fd 冲突（D1 单调 + 无重复 fd）。
        let mut parent = ResourceRegistry::new();
        let p1 = parent.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        let mut child = parent.clone();
        let c1 = child.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        let c2 = child.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        assert!(c1 > p1 && c2 > c1, "子新 fd 应高于父已有全部 fd");
        parent.merge(child);
        let n = parent.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        assert!(
            n > c2 && n > p1,
            "合并后父 next_fd = max，新分配不冲突（n={n} > c2={c2}）"
        );
    }

    #[test]
    fn conflict_matrix_exhaustive_4x4() {
        // 4×4 AccessMode 对 × 同资源/异资源，断言符合 pdr.md §9.1 冲突矩阵。
        let modes = [
            AccessMode::Read,
            AccessMode::Write,
            AccessMode::Append,
            AccessMode::Own,
        ];
        let reg = ResourceRegistry::new();
        for &m1 in &modes {
            for &m2 in &modes {
                // 同资源：仅 Read×Read 并行；Append×Append 需显式 opt-in（决策 D6）；
                // Read×Write / Write×Write / Own×任何 一律串行。
                let a = vec![usage(Resource::Fd(1), m1)];
                let b = vec![usage(Resource::Fd(1), m2)];
                let same_expected = matches!((m1, m2), (AccessMode::Read, AccessMode::Read));
                assert_eq!(
                    reg.can_parallel(&a, &b),
                    same_expected,
                    "同资源 {m1:?} × {m2:?}"
                );

                // 异资源：Δ(a) ∩ Δ(b) = ∅，任何模式组合都可并行（公理 A3）。
                let a2 = vec![usage(Resource::Fd(1), m1)];
                let b2 = vec![usage(Resource::Fd(2), m2)];
                assert!(reg.can_parallel(&a2, &b2), "异资源 {m1:?} × {m2:?}");
            }
        }
        // Append×Append 顺序无关时 opt-in 并行（pdr.md §9.1 / 决策 D6）
        let a = vec![usage(Resource::Fd(1), AccessMode::Append)];
        let b = vec![usage(Resource::Fd(1), AccessMode::Append)];
        assert!(reg.can_parallel_with(&a, &b, true));
    }

    #[test]
    fn handle_allocate_unique_and_take() {
        let mut reg = ResourceRegistry::new();
        let f1 = reg.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        let f2 = reg.allocate(ResourceHandle::Mutex(Arc::new(tokio::sync::Mutex::new(()))));
        assert_ne!(f1, f2);
        assert!(reg.take(f1).is_some());
        assert!(reg.lookup(f1).is_none());
    }

    #[test]
    fn canonicalize_absolute_and_parents() {
        let reg = ResourceRegistry::new();
        let cwd = Path::new("/app");
        assert_eq!(
            reg.canonicalize_path(Path::new("a/./b"), cwd),
            PathBuf::from("/app/a/b")
        );
        assert_eq!(
            reg.canonicalize_path(Path::new("../x"), cwd),
            PathBuf::from("/x")
        );
        assert_eq!(
            reg.canonicalize_path(Path::new("/a/b/../c"), cwd),
            PathBuf::from("/a/c")
        );
    }
}
