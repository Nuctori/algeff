//! A4 批5：宏文档组合示例测试（pdr.md §14 蓝图形态）。
//!
//! 与 tests/macros.rs（单宏展开形状）、tests/blueprint.rs（§14 关闭蓝图逐层解构）、
//! tests/exec_integration.rs（宏×解释器执行级）互补：
//!
//! - lib.rs 中四个宏的文档各带「组合示例」doctest（plan 内 scope、fork 分支 plan、
//!   choose 分支 plan、scope 内 plan），证明单个宏可组合；
//! - 本文件以 1 个「全组合蓝图」编译测试锁定**多宏互嵌套**的组合契约：
//!   scope → plan → fork → plan → choose → plan 六层嵌套构造，match 断言
//!   最外层结构并沿各层检查组合形态（仅构造 Action，不执行）。

use std::path::PathBuf;

use algeff_core::{Action, Value};
use algeff_macro::{choose, fork, plan, scope};

/// 全组合蓝图（scope+plan+fork+choose 嵌套，pdr.md §14 蓝图形态）：
///
/// ```text
/// scope!("/data", || {
///     plan! {
///         fork! {
///             left:  plan! { choose!(cached,
///                                    then: plan! { ... },
///                                    else: plan! { ... }); },
///             right: plan! { Action::Pure(Value::U64(2)); },
///         };
///     }
/// })
/// ```
fn full_composite_blueprint(cached: bool) -> Action {
    scope!("/data", || {
        plan! {
            fork! {
                left: plan! {
                    choose!(
                        cached,
                        then: plan! {
                            Action::Pure(Value::Str("cache-hit".into()));
                        },
                        else: plan! {
                            Action::Pure(Value::Str("cache-miss".into()));
                        },
                    );
                },
                right: plan! {
                    Action::Pure(Value::U64(2));
                },
            };
        }
    })
}

/// 全组合蓝图：最外层为 scope!，逐层 match 断言
/// scope → plan → fork → plan → choose → plan 的嵌套结构。
#[test]
fn full_composite_blueprint_nesting_and_outermost_structure() {
    let blueprint = full_composite_blueprint(true);

    // ── 第 1 层：scope!("/data", || ...) ──────────────────────────────────
    let Action::Scope { base, inner, .. } = blueprint else {
        panic!("全组合蓝图最外层应为 scope! 展开的 Action::Scope");
    };
    assert_eq!(base, PathBuf::from("/data"), "scope! 应透传 base 路径");

    // ── 第 2 层：scope 内层为 plan! 单元素链，元素为 fork! ──────────────
    let Action::Sequential { current, next } = *inner else {
        panic!("scope! 内层应为 plan! 展开的 Sequential");
    };
    assert!(
        matches!(*current, Action::Fork { .. }),
        "scope! 内层 plan! 链的元素应为 fork! 展开的 Fork"
    );
    let cont = next(Value::Unit);
    assert!(
        matches!(cont, Action::Pure(Value::Unit)),
        "plan! 链应收敛为 Pure(Unit)"
    );

    // ── 第 3 层：fork! 左右分支各为 plan! 组合 ───────────────────────────
    let Action::Fork { left, right, .. } = *current else {
        unreachable!("current 已断言为 Fork");
    };
    assert!(
        matches!(*left, Action::Sequential { .. }),
        "fork! 左分支应为 plan! 组合"
    );
    assert!(
        matches!(*right, Action::Sequential { .. }),
        "fork! 右分支应为 plan! 组合"
    );

    // ── 第 4 层：左分支 plan! 链内为 choose!，右分支 plan! 链内为 Pure ──
    let Action::Sequential {
        current: left_current,
        ..
    } = *left
    else {
        unreachable!("left 已断言为 Sequential");
    };
    assert!(
        matches!(*left_current, Action::Choose { .. }),
        "fork! 左分支的 plan! 元素应为 choose! 展开的 Choose"
    );

    let Action::Sequential {
        current: right_current,
        ..
    } = *right
    else {
        unreachable!("right 已断言为 Sequential");
    };
    assert!(
        matches!(*right_current, Action::Pure(Value::U64(2))),
        "fork! 右分支的 plan! 元素应透传 Pure(U64(2))"
    );

    // ── 第 5 层：choose! 的 then/else 分支各为 plan! 组合 ────────────────
    let Action::Choose {
        cond,
        then_branch,
        else_branch,
        ..
    } = *left_current
    else {
        unreachable!("left_current 已断言为 Choose");
    };
    assert!(
        cond(&Value::Unit),
        "cond 闭包应反映 cached 标志（此处为 true）"
    );
    assert!(
        matches!(*then_branch, Action::Sequential { .. }),
        "choose! 的 then 分支应为 plan! 组合"
    );
    assert!(
        matches!(*else_branch, Action::Sequential { .. }),
        "choose! 的 else 分支应为 plan! 组合"
    );

    // ── 第 6 层：then/else 的 plan! 链内各为单元素 Pure ─────────────────
    let Action::Sequential {
        current: then_current,
        next: then_next,
    } = *then_branch
    else {
        unreachable!("then_branch 已断言为 Sequential");
    };
    assert!(
        matches!(*then_current, Action::Pure(Value::Str(s)) if s == "cache-hit"),
        "then 分支内容应为 'cache-hit'"
    );
    assert!(matches!(then_next(Value::Unit), Action::Pure(Value::Unit)));

    let Action::Sequential {
        current: else_current,
        next: else_next,
    } = *else_branch
    else {
        unreachable!("else_branch 已断言为 Sequential");
    };
    assert!(
        matches!(*else_current, Action::Pure(Value::Str(s)) if s == "cache-miss"),
        "else 分支内容应为 'cache-miss'"
    );
    assert!(matches!(else_next(Value::Unit), Action::Pure(Value::Unit)));
}
