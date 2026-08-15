//! 反应式余效应（pdr.md §5.2，可选 feature `coeffects`）。
//!
//! A3 拥有本文件：DependencyTable、CoeffectStore（可逆 set）、
//! Component 生命周期回调与 notify 驱动的激活/停用。

use std::collections::{HashMap, HashSet};
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
}

/// 组件：依赖规范 d ⊆ K + 生命周期回调（A3 完善回调语义）。
#[derive(Debug, Default, Clone)]
pub struct Component {
    pub name: String,
    pub deps: HashSet<DepKey>,
}

impl Component {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), deps: HashSet::new() }
    }

    pub fn depends_on(mut self, k: DepKey) -> Self {
        self.deps.insert(k);
        self
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
}
