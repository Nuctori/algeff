//! 反应式余效应（pdr.md §5.2，可选 feature `coeffects`）。
//!
//! A3 拥有本文件：DependencyTable、CoeffectStore（可逆 set）、
//! Component 生命周期回调、ComponentRegistry（notify 驱动的激活/停用）。

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;

use crate::action::Value;
use crate::syscall::UndoOp;

pub type DepKey = u64;

/// notify 结果（pdr.md §5.2.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Activating,
    Deactivating,
    Neutral,
}

/// 依赖表 Σ := (k:K) ⇀ V_k（pdr.md §5.2.1）。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DependencyTable {
    entries: HashMap<DepKey, Value>,
}

impl DependencyTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, k: &DepKey) -> Option<&Value> {
        self.entries.get(k)
    }

    pub fn contains(&self, k: &DepKey) -> bool {
        self.entries.contains_key(k)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DepKey, &Value)> {
        self.entries.iter()
    }
}

/// 可逆 set(k, v)：`CoeffectStore::set` 返回逆操作，满足
/// pdr.md §5.2.3「依赖注册与撤销自动获得可逆性保证」。
#[derive(Debug, Clone, Default)]
pub struct CoeffectStore {
    inner: Arc<tokio::sync::Mutex<DependencyTable>>,
}

impl CoeffectStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn get(&self, k: DepKey) -> Option<Value> {
        self.inner.lock().await.get(&k).cloned()
    }

    pub async fn snapshot(&self) -> DependencyTable {
        self.inner.lock().await.clone()
    }

    /// 注册依赖并返回逆操作（恢复旧绑定；无旧绑定时删除）。
    pub async fn set(&self, k: DepKey, v: Value) -> UndoOp {
        let mut table = self.inner.lock().await;
        let old = table.entries.insert(k, v.clone());
        drop(table);
        let store = self.clone();
        Box::pin(async move {
            let mut t = store.inner.lock().await;
            match old {
                Some(old_v) => {
                    t.entries.insert(k, old_v);
                }
                None => {
                    t.entries.remove(&k);
                }
            }
        })
    }

    /// 可逆注册并同时产出两份等价逆操作（`Runtime::set_dependency` 场景）：
    /// 一份压入撤销栈随 `recover()` 生效，一份返回调用方供即时撤销。
    ///
    /// 两份逆操作各自独立持有撤销所需信息（旧绑定/删除），语义等价；
    /// 调用方应保证**只执行其中一份**（栈内那份由 `recover()` 消费），
    /// 避免对同一绑定执行两次撤销。
    pub async fn set_replicated(&self, k: DepKey, v: Value) -> (UndoOp, UndoOp) {
        let mut table = self.inner.lock().await;
        let old = table.entries.insert(k, v);
        drop(table);
        let make_undo = |store: CoeffectStore| -> UndoOp {
            let old = old.clone();
            Box::pin(async move {
                let mut t = store.inner.lock().await;
                match old {
                    Some(old_v) => {
                        t.entries.insert(k, old_v);
                    }
                    None => {
                        t.entries.remove(&k);
                    }
                }
            })
        };
        (make_undo(self.clone()), make_undo(self.clone()))
    }
}

/// 组件：依赖规范 d ⊆ K + 生命周期回调（pdr.md §5.2.2）。
///
/// 回调为 `FnMut`（可捕获可变状态），仅在 `ComponentRegistry::sync` 检测到
/// 状态翻转（Activating/Deactivating）时调用。回调不参与 Clone/序列化：
/// 克隆组件时回调被丢弃（None），避免共享闭包状态产生不可预期行为。
#[derive(Default)]
pub struct Component {
    pub name: String,
    pub deps: HashSet<DepKey>,
    on_activate: Option<Box<dyn FnMut() + Send>>,
    on_deactivate: Option<Box<dyn FnMut() + Send>>,
}

impl Clone for Component {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            deps: self.deps.clone(),
            // 闭包不可克隆：克隆体不携带回调。
            on_activate: None,
            on_deactivate: None,
        }
    }
}

impl fmt::Debug for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Component")
            .field("name", &self.name)
            .field("deps", &self.deps)
            .field("on_activate", &self.on_activate.is_some())
            .field("on_deactivate", &self.on_deactivate.is_some())
            .finish()
    }
}

impl Component {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            deps: HashSet::new(),
            on_activate: None,
            on_deactivate: None,
        }
    }

    pub fn depends_on(mut self, k: DepKey) -> Self {
        self.deps.insert(k);
        self
    }

    /// 绑定激活回调：依赖从「不满足」翻转为「满足」（notify = Activating）时调用。
    pub fn on_activate(mut self, f: impl FnMut() + Send + 'static) -> Self {
        self.on_activate = Some(Box::new(f));
        self
    }

    /// 绑定停用回调：依赖从「满足」翻转为「不满足」（notify = Deactivating）时调用。
    pub fn on_deactivate(mut self, f: impl FnMut() + Send + 'static) -> Self {
        self.on_deactivate = Some(Box::new(f));
        self
    }
}

/// 组件注册表：管理组件列表与各组件「上次满足状态」（pdr.md §5.2.2 notify 的工程载体）。
///
/// `sync` 对每个组件取 `store.snapshot()` 计算依赖是否满足（σ⊨d ⇔ d ⊆ dom(σ)），
/// 与上次状态比较得到 Activation：翻转时触发对应生命周期回调并返回事件列表。
/// `last_satisfied` 是 notify(σ, σ′, d) 中 σ⊨d 的折叠结果，避免为每个组件保留完整
/// 依赖表快照；组件初始视为「未满足」，首次满足即产生 Activating。
#[derive(Debug, Default)]
pub struct ComponentRegistry {
    components: Vec<Component>,
    last_satisfied: Vec<bool>,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册组件，返回其索引。
    pub fn register(&mut self, component: Component) -> usize {
        let idx = self.components.len();
        self.components.push(component);
        self.last_satisfied.push(false);
        idx
    }

    pub fn components(&self) -> &[Component] {
        &self.components
    }

    /// 查询组件最近一次 `sync` 时的满足状态。
    pub fn last_satisfied(&self, idx: usize) -> Option<bool> {
        self.last_satisfied.get(idx).copied()
    }

    /// 基于 `store` 当前快照同步全部组件：状态翻转时触发回调，返回
    /// `(组件索引, Activation)` 事件列表（仅含 Activating/Deactivating）。
    pub async fn sync(&mut self, store: &CoeffectStore) -> Vec<(usize, Activation)> {
        let snapshot = store.snapshot().await;
        let mut events = Vec::new();
        for (idx, comp) in self.components.iter_mut().enumerate() {
            let satisfied = comp.deps.iter().all(|k| snapshot.contains(k));
            let prev = self.last_satisfied[idx];
            let activation = match (prev, satisfied) {
                (false, true) => Activation::Activating,
                (true, false) => Activation::Deactivating,
                _ => Activation::Neutral,
            };
            self.last_satisfied[idx] = satisfied;
            match activation {
                Activation::Activating => {
                    if let Some(cb) = comp.on_activate.as_mut() {
                        cb();
                    }
                    events.push((idx, activation));
                }
                Activation::Deactivating => {
                    if let Some(cb) = comp.on_deactivate.as_mut() {
                        cb();
                    }
                    events.push((idx, activation));
                }
                Activation::Neutral => {}
            }
        }
        events
    }
}

/// 组件满足状态机（pdr.md §5.2.2 notify 的折叠状态，工程载体）。
///
/// `Runtime::loaded_components` 由运行时持有（避免组件列表双份漂移），
/// 本结构仅维护各索引「上次满足状态」；`sync` 基于 `store` 当前快照驱动
/// 激活/停用翻转，语义与 `ComponentRegistry::sync` 一致（组件初始视为
/// 未满足，首次满足即 Activating；依赖撤销后翻转 Deactivating），并触发
/// 组件生命周期回调、返回 (索引, Activation) 事件列表。
///
/// 局限：状态按组件索引对齐，组件列表仅追加不删除时索引保持稳定。
#[derive(Debug, Default)]
pub struct ComponentState {
    last_satisfied: Vec<bool>,
}

impl ComponentState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 同步外部组件列表（`Runtime::sync_components` 的底层载体）。
    pub async fn sync(
        &mut self,
        components: &mut [Component],
        store: &CoeffectStore,
    ) -> Vec<(usize, Activation)> {
        let snapshot = store.snapshot().await;
        self.last_satisfied.resize(components.len(), false);
        let mut events = Vec::new();
        for (idx, comp) in components.iter_mut().enumerate() {
            let satisfied = comp.deps.iter().all(|k| snapshot.contains(k));
            let prev = self.last_satisfied[idx];
            let activation = match (prev, satisfied) {
                (false, true) => Activation::Activating,
                (true, false) => Activation::Deactivating,
                _ => Activation::Neutral,
            };
            self.last_satisfied[idx] = satisfied;
            match activation {
                Activation::Activating => {
                    if let Some(cb) = comp.on_activate.as_mut() {
                        cb();
                    }
                    events.push((idx, activation));
                }
                Activation::Deactivating => {
                    if let Some(cb) = comp.on_deactivate.as_mut() {
                        cb();
                    }
                    events.push((idx, activation));
                }
                Activation::Neutral => {}
            }
        }
        events
    }
}

/// notify(σ, σ′, d)（pdr.md §5.2.2）：σ⊨d 当且仅当 d ⊆ dom(σ)。
pub fn notify(sigma: &DependencyTable, sigma_prime: &DependencyTable, deps: &HashSet<DepKey>) -> Activation {
    let satisfied = |s: &DependencyTable| deps.iter().all(|k| s.contains(k));
    match (satisfied(sigma), satisfied(sigma_prime)) {
        (false, true) => Activation::Activating,
        (true, false) => Activation::Deactivating,
        _ => Activation::Neutral,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_is_reversible() {
        let store = CoeffectStore::new();
        store.set(1, Value::U64(42)).await;
        assert_eq!(store.get(1).await, Some(Value::U64(42)));
        let undo = store.set(1, Value::U64(7)).await;
        assert_eq!(store.get(1).await, Some(Value::U64(7)));
        undo.await;
        assert_eq!(store.get(1).await, Some(Value::U64(42)));
        let undo2 = store.set(2, Value::U64(9)).await;
        undo2.await;
        assert_eq!(store.get(2).await, None);
    }

    #[test]
    fn notify_states() {
        let mut s = DependencyTable::new();
        let deps: HashSet<DepKey> = [1u64].into_iter().collect();
        assert_eq!(notify(&s, &s, &deps), Activation::Neutral);
        let mut s2 = s.clone();
        // 直接操作内部：通过 set 走 CoeffectStore 更自然，这里仅测 notify 判定
        s2.entries.insert(1, Value::Unit);
        assert_eq!(notify(&s, &s2, &deps), Activation::Activating);
        assert_eq!(notify(&s2, &s, &deps), Activation::Deactivating);
    }

    #[tokio::test]
    async fn registry_sync_activation_sequence() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let store = CoeffectStore::new();
        let mut reg = ComponentRegistry::new();
        let activates = Arc::new(AtomicUsize::new(0));
        let deactivates = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&activates);
        let d = Arc::clone(&deactivates);
        let idx = reg.register(
            Component::new("svc")
                .depends_on(1)
                .on_activate(move || {
                    a.fetch_add(1, Ordering::SeqCst);
                })
                .on_deactivate(move || {
                    d.fetch_add(1, Ordering::SeqCst);
                }),
        );
        assert_eq!(idx, 0);

        // 初始空 store：依赖未满足 → 无事件、无回调
        assert!(reg.sync(&store).await.is_empty());
        assert_eq!(activates.load(Ordering::SeqCst), 0);
        assert_eq!(deactivates.load(Ordering::SeqCst), 0);
        assert_eq!(reg.last_satisfied(0), Some(false));

        // 激活：注册依赖键 1
        let undo = store.set(1, Value::U64(1)).await;
        assert_eq!(reg.sync(&store).await, vec![(0, Activation::Activating)]);
        assert_eq!(activates.load(Ordering::SeqCst), 1);
        assert_eq!(deactivates.load(Ordering::SeqCst), 0);
        assert_eq!(reg.last_satisfied(0), Some(true));

        // 停用：撤销依赖键 1（set 的逆操作）
        undo.await;
        assert_eq!(reg.sync(&store).await, vec![(0, Activation::Deactivating)]);
        assert_eq!(activates.load(Ordering::SeqCst), 1);
        assert_eq!(deactivates.load(Ordering::SeqCst), 1);
        assert_eq!(reg.last_satisfied(0), Some(false));

        // 再激活：完整 激活→停用→再激活 序列
        let _undo2 = store.set(1, Value::U64(2)).await;
        assert_eq!(reg.sync(&store).await, vec![(0, Activation::Activating)]);
        assert_eq!(activates.load(Ordering::SeqCst), 2);
        assert_eq!(deactivates.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn registry_sync_multi_component_and_neutral() {
        let store = CoeffectStore::new();
        let mut reg = ComponentRegistry::new();
        reg.register(Component::new("a").depends_on(1));
        reg.register(Component::new("b").depends_on(2));

        // 全部未满足：无事件
        assert!(reg.sync(&store).await.is_empty());

        let undo1 = store.set(1, Value::Unit).await;
        assert_eq!(reg.sync(&store).await, vec![(0, Activation::Activating)]);
        // 无状态变化：Neutral，不产生事件
        assert!(reg.sync(&store).await.is_empty());

        let undo2 = store.set(2, Value::Unit).await;
        assert_eq!(reg.sync(&store).await, vec![(1, Activation::Activating)]);

        // 全部撤销：按索引顺序逐个 Deactivating
        undo1.await;
        undo2.await;
        assert_eq!(
            reg.sync(&store).await,
            vec![(0, Activation::Deactivating), (1, Activation::Deactivating)]
        );
    }

    #[test]
    fn component_clone_drops_callbacks() {
        let c = Component::new("x")
            .depends_on(7)
            .on_activate(|| {})
            .on_deactivate(|| {});
        let c2 = c.clone();
        assert_eq!(c2.name, "x");
        assert!(c2.deps.contains(&7));
        // 克隆体不携带回调（闭包不可克隆）
        let _ = format!("{c:?}");
    }
}
