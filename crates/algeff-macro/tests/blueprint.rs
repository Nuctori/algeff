//! 蓝图形态集成测试（pdr.md §14 简化版）。
//!
//! 与 `tests/macros.rs`（单个宏的展开形状）互补：本文件以「服务器关闭蓝图」形式
//! 组合使用 `plan!/fork!/choose!/scope!`，用 match 逐层解构断言嵌套关系与 base
//! 路径值，并覆盖边界形态（plan! 单元素/尾随分号、fork! 尾随逗号、choose! 的
//! else 关键字解析）。
//!
//! 蓝图不含实际 IO 函数（A5 适配层尚未合并）——以 `Pure`/`Alloc` 代替
//! `Syscall`，仅验证宏组合的 AST 拓扑与类型正确性。

use std::path::PathBuf;

use algeff_core::{Action, Value};
use algeff_macro::{choose, fork, plan, scope};

/// 沿 Sequential 链逐层剥取 `current`；遇最终 `Pure(Unit)` 收敛即终止。
fn peel_plan(action: Action, out: &mut Vec<Action>) {
    match action {
        Action::Sequential { current, next } => {
            out.push(*current);
            peel_plan(next(Value::Unit), out);
        }
        Action::Pure(Value::Unit) => {}
        other => out.push(other),
    }
}

/// 服务器关闭蓝图（pdr.md §14 简化版）。
///
/// 嵌套结构：`scope!("/var/log/myapp", || plan! { fork! { left: plan! { choose! } } })`
///
/// ```text
/// scope!("/var/log/myapp", || {
///     plan! {
///         fork! {                              // 步骤 1：并发关闭任务
///             left: plan! {
///                 choose!(graceful,            // 左路内再按模式分支
///                     then: plan! { ... },
///                     else: plan! { ... });
///             },
///             right: plan! { ... },
///         };
///         Action::Alloc { len: 4096, ... };    // 步骤 2：关闭缓冲
///     }
/// })
/// ```
fn shutdown_blueprint(graceful: bool) -> Action {
    scope!("/var/log/myapp", || {
        plan! {
            fork! {
                left: plan! {
                    choose!(
                        graceful,
                        then: plan! {
                            Action::Pure(Value::Str("graceful shutdown".into()));
                        },
                        else: plan! {
                            Action::Pure(Value::Str("force shutdown".into()));
                        },
                    );
                },
                right: plan! {
                    Action::Pure(Value::U64(2));
                },
            };
            Action::Alloc {
                len: 4096,
                next: Box::new(|_| Action::Pure(Value::Unit)),
            };
        }
    })
}

// ---------------------------------------------------------------------------
// 主蓝图：逐层 match 解构，验证 scope → plan → fork → plan → choose 嵌套
// ---------------------------------------------------------------------------

#[test]
fn shutdown_blueprint_nesting_and_base_path() {
    let blueprint = shutdown_blueprint(true);

    // ── 第 1 层：scope!("/var/log/myapp", || ...) ─────────────────────────
    let Action::Scope { base, inner, next } = blueprint else {
        panic!("蓝图顶层应为 scope! 展开的 Action::Scope");
    };
    assert_eq!(base, PathBuf::from("/var/log/myapp"), "scope! 应透传 base 路径");
    let cont = next(Value::Unit);
    assert!(
        matches!(cont, Action::Pure(Value::Unit)),
        "scope! 的 next 应收敛为 Pure(Unit)"
    );

    // ── 第 2 层：内层 plan! 链（步骤 1 = fork!，步骤 2 = Alloc）──────────
    let mut steps = Vec::new();
    peel_plan(*inner, &mut steps);
    assert_eq!(steps.len(), 2, "关闭蓝图应包含 2 个 plan! 步骤");
    let mut steps = steps.into_iter();
    let step_fork = steps.next().expect("步骤 1 缺失");
    let step_alloc = steps.next().expect("步骤 2 缺失");
    assert!(steps.next().is_none(), "不应存在第 3 个步骤");

    // ── 第 3 层：步骤 1 = fork!{ left, right } ───────────────────────────
    let Action::Fork { left, right, combine } = step_fork else {
        panic!("步骤 1 应为 fork! 展开的 Action::Fork");
    };
    let combined = combine(Value::Unit, Value::Unit);
    assert!(
        matches!(combined, Action::Pure(Value::Unit)),
        "fork! 的 combine 应忽略两侧值收敛为 Pure(Unit)"
    );

    // ── 第 4 层：left = plan!{ choose! }，right = plan!{ Pure(U64(2)) } ──
    let mut left_chain = Vec::new();
    peel_plan(*left, &mut left_chain);
    assert_eq!(left_chain.len(), 1, "fork! 左路应为单元素 plan! 链");

    let mut right_chain = Vec::new();
    peel_plan(*right, &mut right_chain);
    assert_eq!(right_chain.len(), 1, "fork! 右路应为单元素 plan! 链");
    assert!(
        matches!(&right_chain[0], Action::Pure(Value::U64(2))),
        "fork! 右路内容不符"
    );

    // ── 第 5 层：left 的 choose!（关闭模式分支）──────────────────────────
    let Action::Choose { cond, then_branch, else_branch } = left_chain.pop().unwrap() else {
        panic!("fork! 左路应为 choose! 展开的 Action::Choose");
    };
    assert!(
        cond(&Value::Unit),
        "choose! 的 cond 闭包应反映 graceful 标志（此处为 true）"
    );

    // ── 第 6 层：choose! 的 then/else 分支各为 plan! 单元素链 ────────────
    let mut then_chain = Vec::new();
    peel_plan(*then_branch, &mut then_chain);
    assert_eq!(then_chain.len(), 1, "then 分支应为单元素 plan! 链");
    assert!(
        matches!(&then_chain[0], Action::Pure(Value::Str(s)) if s == "graceful shutdown"),
        "then 分支应为 'graceful shutdown'"
    );

    let mut else_chain = Vec::new();
    peel_plan(*else_branch, &mut else_chain);
    assert_eq!(else_chain.len(), 1, "else 分支应为单元素 plan! 链");
    assert!(
        matches!(&else_chain[0], Action::Pure(Value::Str(s)) if s == "force shutdown"),
        "else 分支应为 'force shutdown'"
    );

    // ── 步骤 2 = Alloc{ len }，字段透传 ──────────────────────────────────
    let Action::Alloc { len, next: alloc_next } = step_alloc else {
        panic!("步骤 2 应为 Action::Alloc");
    };
    assert_eq!(len, 4096, "Alloc 的 len 应透传");
    let alloc_cont = alloc_next(Value::Unit);
    assert!(
        matches!(alloc_cont, Action::Pure(Value::Unit)),
        "Alloc 的 next 应收敛为 Pure(Unit)"
    );
}

// ---------------------------------------------------------------------------
// 边界：plan! 单元素（无尾随分号）与尾随分号等价
// ---------------------------------------------------------------------------

#[test]
fn plan_single_element_with_and_without_trailing_semicolon() {
    let cases = [
        plan! { Action::Pure(Value::U64(7)) },
        plan! { Action::Pure(Value::U64(7)); },
    ];
    for (i, action) in cases.into_iter().enumerate() {
        let Action::Sequential { current, next } = action else {
            panic!("plan! 单元素应展开为 1 层 Sequential（case {i}）");
        };
        assert!(
            matches!(*current, Action::Pure(Value::U64(7))),
            "case {i}：单元素内容不符"
        );
        let cont = next(Value::Unit);
        assert!(
            matches!(cont, Action::Pure(Value::Unit)),
            "case {i}：单元素 plan! 应直接收敛为 Pure(Unit)"
        );
    }
}

// ---------------------------------------------------------------------------
// 边界：fork! 尾随逗号可解析
// ---------------------------------------------------------------------------

#[test]
fn fork_trailing_comma_accepted() {
    let action = fork! {
        left: Action::Pure(Value::Bool(true)),
        right: Action::Pure(Value::Bool(false)),
    };
    let Action::Fork { left, right, .. } = action else {
        panic!("fork! 应展开为 Action::Fork");
    };
    assert!(matches!(*left, Action::Pure(Value::Bool(true))));
    assert!(matches!(*right, Action::Pure(Value::Bool(false))));
}

// ---------------------------------------------------------------------------
// 边界：choose! 的 else 关键字解析（Token![else]）+ 尾随逗号
// ---------------------------------------------------------------------------

#[test]
fn choose_else_keyword_parsing() {
    let action = choose!(
        true,
        then: Action::Pure(Value::U64(1)),
        else: Action::Pure(Value::U64(2)),
    );
    let Action::Choose { cond, then_branch, else_branch } = action else {
        panic!("choose! 应展开为 Action::Choose");
    };
    assert!(cond(&Value::Unit), "cond 闭包应返回 true");
    assert!(matches!(*then_branch, Action::Pure(Value::U64(1))));
    assert!(matches!(*else_branch, Action::Pure(Value::U64(2))));
}

// ---------------------------------------------------------------------------
// 类型正确性：展开产物可直接被 match 消费，无需转换
// ---------------------------------------------------------------------------

#[test]
fn expansions_consumable_by_match() {
    // plan!
    match plan! {
        Action::Pure(Value::U64(1));
        Action::Pure(Value::U64(2));
    } {
        Action::Sequential { current, next } => {
            assert!(matches!(*current, Action::Pure(Value::U64(1))));
            let cont = next(Value::Unit);
            assert!(matches!(cont, Action::Sequential { .. }));
        }
        _ => panic!("plan! 产物应可被 match 解构"),
    }

    // fork!
    match fork! {
        left: Action::Pure(Value::Unit),
        right: Action::Pure(Value::Unit),
    } {
        Action::Fork { .. } => {}
        _ => panic!("fork! 产物应可被 match 解构"),
    }

    // scope!
    match scope!("/tmp", || Action::Pure(Value::Unit)) {
        Action::Scope { base, .. } => assert_eq!(base, PathBuf::from("/tmp")),
        _ => panic!("scope! 产物应可被 match 解构"),
    }

    // choose!（else 为关键字仍可正常解析为分支）
    match choose!(false, then: Action::Pure(Value::Unit), else: Action::Pure(Value::Unit)) {
        Action::Choose { cond, .. } => assert!(!cond(&Value::Unit)),
        _ => panic!("choose! 产物应可被 match 解构"),
    }
}
