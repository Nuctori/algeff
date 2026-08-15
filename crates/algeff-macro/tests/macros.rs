//! algeff-macro 集成测试：验证 plan!/fork!/scope!/choose! 的展开结构（pdr.md §13）。
//!
//! 宏只做 AST 构造，因此测试通过解构 `algeff_core::action::Action` 断言
//! 展开产物形状（嵌套深度、字段内容、收敛 continuation）。

use std::path::PathBuf;

use algeff_core::{Action, Value};
use algeff_macro::{choose, fork, plan, scope};

/// 沿 Sequential 链逐层剥取，把每层 `current` 压入 `out`；
/// 遇最终 `Pure(Unit)` 收敛即终止，其余节点原样压入。
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

// ---------------------------------------------------------------------------
// 1. plan! 三元素 → 嵌套 Sequential 深度 3，最终收敛 Pure(Unit)
// ---------------------------------------------------------------------------

#[test]
fn plan_three_elements_nested_sequential() {
    let action = plan! {
        Action::Pure(Value::U64(1));
        Action::Pure(Value::U64(2));
        Action::Pure(Value::U64(3));
    };
    let mut chain = Vec::new();
    peel_plan(action, &mut chain);
    assert_eq!(chain.len(), 3, "plan! 三元素应展开为 3 层 Sequential");
    assert!(matches!(&chain[0], Action::Pure(Value::U64(1))));
    assert!(matches!(&chain[1], Action::Pure(Value::U64(2))));
    assert!(matches!(&chain[2], Action::Pure(Value::U64(3))));
}

// ---------------------------------------------------------------------------
// 2. plan! 空列表 → Pure(Unit)
// ---------------------------------------------------------------------------

#[test]
fn plan_empty_is_pure_unit() {
    let action = plan! {};
    assert!(matches!(action, Action::Pure(Value::Unit)));
}

// ---------------------------------------------------------------------------
// 3. fork! → left/right 字段内容 + combine 收敛
// ---------------------------------------------------------------------------

#[test]
fn fork_fields_and_combine() {
    let action = fork! {
        left: Action::Pure(Value::Bool(true)),
        right: Action::Pure(Value::Bool(false)),
    };
    match action {
        Action::Fork {
            left,
            right,
            combine,
        } => {
            assert!(matches!(*left, Action::Pure(Value::Bool(true))));
            assert!(matches!(*right, Action::Pure(Value::Bool(false))));
            let combined = combine(Value::Unit, Value::Unit);
            assert!(matches!(combined, Action::Pure(Value::Unit)));
        }
        _ => panic!("fork! 应展开为 Action::Fork"),
    }
}

// ---------------------------------------------------------------------------
// 4. scope! 字符串 base → PathBuf；闭包调用结果作为 inner
// ---------------------------------------------------------------------------

#[test]
fn scope_string_base_becomes_pathbuf() {
    let action = scope!("/tmp", || Action::Pure(Value::Unit));
    match action {
        Action::Scope { base, inner, next } => {
            assert_eq!(base, PathBuf::from("/tmp"));
            assert!(matches!(*inner, Action::Pure(Value::Unit)));
            let cont = next(Value::Unit);
            assert!(matches!(cont, Action::Pure(Value::Unit)));
        }
        _ => panic!("scope! 应展开为 Action::Scope"),
    }
}

/// base 为现成 PathBuf 表达式时直接使用，不做转换。
#[test]
fn scope_pathbuf_expr_passthrough() {
    let base = PathBuf::from("/var/log");
    let action = scope!(base, || Action::Pure(Value::Unit));
    match action {
        Action::Scope { base, .. } => assert_eq!(base, PathBuf::from("/var/log")),
        _ => panic!("scope! 应展开为 Action::Scope"),
    }
}

/// scope! 的 inner 闭包体内可嵌套其他宏（宏可组合，仅 AST 拼接）。
#[test]
fn scope_inner_may_nest_macros() {
    let action = scope!("/tmp", || fork! {
        left: Action::Pure(Value::Unit),
        right: Action::Pure(Value::Unit),
    });
    match action {
        Action::Scope { inner, .. } => {
            assert!(matches!(*inner, Action::Fork { .. }));
        }
        _ => panic!("scope! 应展开为 Action::Scope"),
    }
}

// ---------------------------------------------------------------------------
// 5. choose! → cond 为闭包，分支字段正确
// ---------------------------------------------------------------------------

#[test]
fn choose_cond_closure_and_branches() {
    let flag = true;
    let action = choose!(
        flag,
        then: Action::Pure(Value::Bool(true)),
        else: Action::Pure(Value::Bool(false)),
    );
    match action {
        Action::Choose {
            cond,
            then_branch,
            else_branch,
        } => {
            assert!(cond(&Value::Unit), "cond 闭包应返回捕获的 bool 值");
            assert!(matches!(*then_branch, Action::Pure(Value::Bool(true))));
            assert!(matches!(*else_branch, Action::Pure(Value::Bool(false))));
        }
        _ => panic!("choose! 应展开为 Action::Choose"),
    }
}

// ---------------------------------------------------------------------------
// 6. 类型检查冒烟：展开产物可直接赋值给 Action 类型变量
// ---------------------------------------------------------------------------

#[test]
fn expansions_assignable_to_action() {
    let _: Action = plan! {
        Action::Pure(Value::Unit);
        Action::Pure(Value::Unit);
    };
    let _: Action = fork! {
        left: Action::Pure(Value::Unit),
        right: Action::Pure(Value::Unit),
    };
    let _: Action = scope!("/tmp", || Action::Pure(Value::Unit));
    let _: Action = choose!(true, then: Action::Pure(Value::Unit), else: Action::Pure(Value::Unit));
}
