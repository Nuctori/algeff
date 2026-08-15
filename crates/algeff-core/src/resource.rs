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
