//! A2 批3：Runtime×coeffects/virtual-clock 特性接线集成测试（pdr.md §5.2 / §12.3）。
//!
//! feature 分门别类：`coeffects` 与 `virtual-clock` 各占一个 cfg 模块；
//! 默认 features（两者皆关）下本文件整体不编译——`cargo test --workspace`
//! 全绿不受影响。
//!
//! 说明：`Runtime::new` 须在 tokio 上下文之外调用（D9），因此测试先建
//! Runtime，再以本地 current-thread runtime（`drive`）驱动异步访问器。

#![cfg(any(feature = "coeffects", feature = "virtual-clock"))]

use algeff_core::action::{DataOp, Value};
use algeff_core::error::SysError;
use algeff_core::resource::ResourceRegistry;
use algeff_core::runtime::Runtime;
use algeff_core::syscall::{BoxFuture, SyscallExecutor, UndoOp};

/// 本地 current-thread runtime 驱动（仅 coeffects 测试需要）。
#[cfg(feature = "coeffects")]
use std::future::Future;

/// 本地 current-thread runtime 驱动（仅 coeffects 测试需要）。
#[cfg(feature = "coeffects")]
fn drive<F: Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("无法创建 current-thread tokio runtime")
        .block_on(f)
}

/// 最小执行器：所有 op 返回 Ok((Unit, None))，不产生真实 IO/撤销操作。
struct NoopExecutor;

impl SyscallExecutor for NoopExecutor {
    fn execute<'a>(
        &'a mut self,
        _op: &'a DataOp,
        _registry: &'a mut ResourceRegistry,
    ) -> BoxFuture<'a, Result<(Value, Option<UndoOp>), SysError>> {
        Box::pin(async { Ok((Value::Unit, None)) })
    }
}

#[cfg(feature = "coeffects")]
mod coeffects {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use algeff_core::coeffects::{Activation, Component};

    #[test]
    fn accessors_and_empty_sync() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        // 依赖表/组件列表公开存取路径可用（此前字段无公开访问）。
        assert!(rt.dependency_table().is_some());
        assert!(rt.components().is_some());
        assert!(rt.components().unwrap().is_empty());
        // 空组件列表：sync 无事件。
        assert!(drive(rt.sync_components()).is_empty());
    }

    #[test]
    fn set_dependency_pushes_undo_and_recover_restores() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        // 初始无依赖。
        assert_eq!(drive(rt.dependency_table().unwrap().get(1)), None);

        // set_dependency：注册依赖 + 撤销栈压入逆操作（效果与余效应统一）。
        let _undo = drive(rt.set_dependency(1, Value::U64(42))).unwrap();
        assert_eq!(rt.undo_stack().len(), 1);
        assert_eq!(
            drive(rt.dependency_table().unwrap().get(1)),
            Some(Value::U64(42))
        );

        // 覆盖旧绑定：撤销栈继续累积（LIFO 逆序恢复）。
        let _undo2 = drive(rt.set_dependency(1, Value::U64(7))).unwrap();
        assert_eq!(rt.undo_stack().len(), 2);
        assert_eq!(
            drive(rt.dependency_table().unwrap().get(1)),
            Some(Value::U64(7))
        );

        // recover()：按 LIFO 撤销全部依赖操作，恢复初始空表。
        drive(rt.recover());
        assert!(rt.undo_stack().is_empty());
        assert_eq!(drive(rt.dependency_table().unwrap().get(1)), None);
    }

    #[test]
    fn returned_undo_reverts_dependency_directly() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        let undo = drive(rt.set_dependency(1, Value::U64(9))).unwrap();
        assert_eq!(
            drive(rt.dependency_table().unwrap().get(1)),
            Some(Value::U64(9))
        );

        // 返回的逆操作可即时撤销（栈内副本由 recover() 消费，两份只执行一份）。
        drive(undo);
        assert_eq!(drive(rt.dependency_table().unwrap().get(1)), None);
        assert_eq!(rt.undo_stack().len(), 1);
    }

    #[test]
    fn sync_components_event_sequence() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        let activates = Arc::new(AtomicUsize::new(0));
        let deactivates = Arc::new(AtomicUsize::new(0));
        let a = Arc::clone(&activates);
        let d = Arc::clone(&deactivates);
        rt.components().unwrap().push(
            Component::new("svc")
                .depends_on(1)
                .on_activate(move || {
                    a.fetch_add(1, Ordering::SeqCst);
                })
                .on_deactivate(move || {
                    d.fetch_add(1, Ordering::SeqCst);
                }),
        );

        // 初始空依赖表：依赖未满足 → 无事件、无回调。
        assert!(drive(rt.sync_components()).is_empty());
        assert_eq!(activates.load(Ordering::SeqCst), 0);
        assert_eq!(deactivates.load(Ordering::SeqCst), 0);

        // 注册依赖 k=1 → Activating（回调触发）。
        let undo = drive(rt.set_dependency(1, Value::U64(1))).unwrap();
        assert_eq!(
            drive(rt.sync_components()),
            vec![(0, Activation::Activating)]
        );
        assert_eq!(activates.load(Ordering::SeqCst), 1);

        // 无状态变化：Neutral，不产生事件。
        assert!(drive(rt.sync_components()).is_empty());

        // undo（恢复依赖表）→ Deactivating（回调触发）。
        drive(undo);
        assert_eq!(
            drive(rt.sync_components()),
            vec![(0, Activation::Deactivating)]
        );
        assert_eq!(activates.load(Ordering::SeqCst), 1);
        assert_eq!(deactivates.load(Ordering::SeqCst), 1);

        // 再激活：完整 激活→停用→再激活 序列（回调计数递增）。
        let _undo2 = drive(rt.set_dependency(1, Value::U64(2))).unwrap();
        assert_eq!(
            drive(rt.sync_components()),
            vec![(0, Activation::Activating)]
        );
        assert_eq!(activates.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn sync_components_multiple_components() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        rt.components()
            .unwrap()
            .push(Component::new("a").depends_on(1));
        rt.components()
            .unwrap()
            .push(Component::new("b").depends_on(2));

        // 全部未满足：无事件。
        assert!(drive(rt.sync_components()).is_empty());

        let _u1 = drive(rt.set_dependency(1, Value::Unit)).unwrap();
        assert_eq!(
            drive(rt.sync_components()),
            vec![(0, Activation::Activating)]
        );

        let _u2 = drive(rt.set_dependency(2, Value::Unit)).unwrap();
        assert_eq!(
            drive(rt.sync_components()),
            vec![(1, Activation::Activating)]
        );

        // recover() 撤销全部依赖 → 按索引顺序逐个 Deactivating。
        drive(rt.recover());
        assert_eq!(
            drive(rt.sync_components()),
            vec![(0, Activation::Deactivating), (1, Activation::Deactivating)]
        );
    }
}

#[cfg(feature = "virtual-clock")]
mod virtual_clock {
    use super::*;
    use std::time::{Duration, Instant};

    use algeff_core::action::Action;

    #[test]
    fn sleep_does_not_wait_under_virtual_clock() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        let start = Instant::now();
        let v = rt.run_blocking(Action::Sleep {
            duration: Duration::from_millis(200),
            next: Box::new(Action::Pure),
        });
        assert_eq!(v, Ok(Value::Unit));
        // virtual clock：推进逻辑时钟，不真实等待（墙钟断言 < 阈值）。
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "virtual clock 不应真实等待，elapsed {:?}",
            start.elapsed()
        );
        assert_eq!(
            rt.virtual_clock().expect("virtual clock 存在").now(),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn virtual_clock_accumulates_across_sleeps() {
        let mut rt = Runtime::new(Box::new(NoopExecutor));
        assert_eq!(
            rt.run_blocking(Action::Sleep {
                duration: Duration::from_millis(250),
                next: Box::new(Action::Pure),
            }),
            Ok(Value::Unit)
        );
        assert_eq!(
            rt.run_blocking(Action::Sleep {
                duration: Duration::from_millis(750),
                next: Box::new(Action::Pure),
            }),
            Ok(Value::Unit)
        );
        assert_eq!(
            rt.virtual_clock().expect("virtual clock 存在").now(),
            Duration::from_millis(1000)
        );
    }
}
