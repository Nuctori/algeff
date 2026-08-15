//! Algeff 可选语法糖宏（pdr.md §13「宏的使用（可选）」/ §八「代数原语」）。
//!
//! 本 crate 是**可选语法糖**：algeff-core 核心不依赖任何宏，所有蓝图均可手写
//! `Action` 链等价表达；宏仅做 AST 构造（syn/quote 拼接），**不参与类型系统**，
//! 不增加编译负担（pdr.md §八）。
//!
//! 提供四个宏：
//! - `plan!{ stmt; stmt; ... }` → `Action::Sequential` 链
//! - `fork!{ left: ..., right: ... }` → `Action::Fork`
//! - `scope!("/tmp", || { ... })` → `Action::Scope`
//! - `choose!(cond, then: ..., else: ...)` → `Action::Choose`
//!
//! 展开产物一律引用 `algeff_core::action::` 路径构造节点，宏本身不做任何类型
//! 检查。使用宏的代码需依赖 algeff-core，推荐配合其 prelude 使用：
//! `use algeff_core::prelude::*;`（提供 `Action`/`Value` 等类型）。

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, Ident, Lit, Token};

/// 终止 continuation：`|_| Action::Pure(Value::Unit)`。
fn pure_unit() -> TokenStream2 {
    quote! {
        algeff_core::action::Action::Pure(algeff_core::action::Value::Unit)
    }
}

// ---------------------------------------------------------------------------
// plan!{ e1; e2; e3; ... }
// ---------------------------------------------------------------------------

struct PlanInput {
    exprs: Punctuated<Expr, Token![;]>,
}

impl Parse for PlanInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(Self {
            exprs: Punctuated::<Expr, Token![;]>::parse_terminated(input)?,
        })
    }
}

#[doc = "构造 `Action::Sequential` 链（pdr.md §13「宏的使用（可选）」/ §八「代数原语（DSL 名称与语义）」）。"]
#[doc = ""]
#[doc = "语法：`plan!{ e1; e2; ... }`，每个元素为返回 `Action` 的表达式。"]
#[doc = ""]
#[doc = "展开说明：元素反向逐个装箱为 `Action::Sequential { current, next }`；"]
#[doc = "`current` 为当前元素，`next` 为忽略前一值的 continuation，最后一个 `next`"]
#[doc = "收敛为 `Action::Pure(Value::Unit)`。空列表直接展开为 `Action::Pure(Value::Unit)`。"]
#[doc = "宏仅拼 AST，不做任何类型检查（pdr.md §八：核心不依赖宏，宏仅为语法糖）。"]
#[doc = ""]
#[doc = "用法示例："]
#[doc = "```rust"]
#[doc = "use algeff_macro::plan;"]
#[doc = "use algeff_core::{Action, Value};"]
#[doc = "let p: Action = plan! {"]
#[doc = "    Action::Pure(Value::U64(1));"]
#[doc = "    Action::Pure(Value::U64(2));"]
#[doc = "};"]
#[doc = "assert!(matches!(p, Action::Sequential { .. }));"]
#[doc = "```"]
#[doc = ""]
#[doc = "组合示例（宏可组合，pdr.md §14 蓝图形态）：plan! 链内嵌套 scope!，scope! 闭包体内再返回 plan! 链："]
#[doc = "```rust"]
#[doc = "use algeff_core::prelude::*;"]
#[doc = "use algeff_macro::*;"]
#[doc = ""]
#[doc = "let p: Action = plan! {"]
#[doc = "    Action::Pure(Value::U64(1));"]
#[doc = "    scope!(\"/var/log/myapp\", || plan! {"]
#[doc = "        Action::Pure(Value::Str(\"boot\".into()));"]
#[doc = "        Action::Pure(Value::U64(2));"]
#[doc = "    });"]
#[doc = "    Action::Pure(Value::U64(3));"]
#[doc = "};"]
#[doc = ""]
#[doc = "let Action::Sequential { current, next } = p else {"]
#[doc = "    panic!(\"plan! 最外层应为 Sequential\");"]
#[doc = "};"]
#[doc = "assert!(matches!(*current, Action::Pure(Value::U64(1))));"]
#[doc = "let Action::Sequential { current, .. } = next(Value::Unit) else {"]
#[doc = "    panic!(\"plan! 第 2 步应为 Sequential\");"]
#[doc = "};"]
#[doc = "assert!(matches!(*current, Action::Scope { .. }), \"plan! 链内可嵌套 scope!\");"]
#[doc = "```"]
#[proc_macro]
pub fn plan(input: TokenStream) -> TokenStream {
    let PlanInput { exprs } = syn::parse_macro_input!(input as PlanInput);
    let mut out = pure_unit();
    for expr in exprs.into_iter().rev() {
        out = quote! {
            algeff_core::action::Action::Sequential {
                current: Box::new(#expr),
                next: Box::new(|_| #out),
            }
        };
    }
    out.into()
}

// ---------------------------------------------------------------------------
// fork!{ left: e1, right: e2 }
// ---------------------------------------------------------------------------

struct ForkInput {
    left: Expr,
    right: Expr,
}

impl Parse for ForkInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let label: Ident = input.parse()?;
        if label != "left" {
            return Err(syn::Error::new(
                label.span(),
                "fork! 期望标签 `left:`（语法：fork!{ left: <expr>, right: <expr> }）",
            ));
        }
        input.parse::<Token![:]>()?;
        let left: Expr = input.parse()?;
        input.parse::<Token![,]>()?;

        let label: Ident = input.parse()?;
        if label != "right" {
            return Err(syn::Error::new(
                label.span(),
                "fork! 期望标签 `right:`（语法：fork!{ left: <expr>, right: <expr> }）",
            ));
        }
        input.parse::<Token![:]>()?;
        let right: Expr = input.parse()?;

        // 容忍尾随逗号，再拒绝任何多余 token
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(
                input.error("fork! 存在多余 token（语法：fork!{ left: <expr>, right: <expr> }）")
            );
        }
        Ok(Self { left, right })
    }
}

#[doc = "构造 `Action::Fork`（pdr.md §13「宏的使用（可选）」/ §八「代数原语」）。"]
#[doc = ""]
#[doc = "语法：`fork!{ left: e1, right: e2 }`（容忍尾随逗号）。"]
#[doc = ""]
#[doc = "展开说明：`left`/`right` 分别装箱为 `Action::Fork` 的两个分支；"]
#[doc = "`combine` 固定为忽略两侧值、收敛为 `Action::Pure(Value::Unit)` 的闭包"]
#[doc = "（pdr.md §八：并发分叉，左/右资源集自动分裂）。"]
#[doc = ""]
#[doc = "用法示例："]
#[doc = "```rust"]
#[doc = "use algeff_macro::fork;"]
#[doc = "use algeff_core::{Action, Value};"]
#[doc = "let f: Action = fork! {"]
#[doc = "    left: Action::Pure(Value::U64(1)),"]
#[doc = "    right: Action::Pure(Value::U64(2)),"]
#[doc = "};"]
#[doc = "assert!(matches!(f, Action::Fork { .. }));"]
#[doc = "```"]
#[doc = ""]
#[doc = "组合示例（宏可组合，pdr.md §14 蓝图形态）：左右分支分别用 plan! 组合成 Sequential 链："]
#[doc = "```rust"]
#[doc = "use algeff_core::prelude::*;"]
#[doc = "use algeff_macro::*;"]
#[doc = ""]
#[doc = "let f: Action = fork! {"]
#[doc = "    left: plan! {"]
#[doc = "        Action::Pure(Value::U64(1));"]
#[doc = "        Action::Pure(Value::U64(2));"]
#[doc = "    },"]
#[doc = "    right: plan! {"]
#[doc = "        Action::Pure(Value::Str(\"right\".into()));"]
#[doc = "    },"]
#[doc = "};"]
#[doc = ""]
#[doc = "let Action::Fork { left, right, .. } = f else {"]
#[doc = "    panic!(\"fork! 最外层应为 Fork\");"]
#[doc = "};"]
#[doc = "assert!(matches!(*left, Action::Sequential { .. }), \"fork! 左分支应为 plan! 组合\");"]
#[doc = "assert!(matches!(*right, Action::Sequential { .. }), \"fork! 右分支应为 plan! 组合\");"]
#[doc = "```"]
#[proc_macro]
pub fn fork(input: TokenStream) -> TokenStream {
    let ForkInput { left, right } = syn::parse_macro_input!(input as ForkInput);
    quote! {
        algeff_core::action::Action::Fork {
            left: Box::new(#left),
            right: Box::new(#right),
            combine: Box::new(|_, _| algeff_core::action::Action::Pure(algeff_core::action::Value::Unit)),
        }
    }
    .into()
}

// ---------------------------------------------------------------------------
// scope!(base, || expr)
// ---------------------------------------------------------------------------

struct ScopeInput {
    base: Expr,
    inner: Expr,
}

impl Parse for ScopeInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let base: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let inner: Expr = input.parse()?;

        // 容忍尾随逗号，再拒绝任何多余 token
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(input.error("scope! 存在多余 token（语法：scope!(base, || expr)）"));
        }
        Ok(Self { base, inner })
    }
}

#[doc = "构造 `Action::Scope`（pdr.md §13「宏的使用（可选）」/ §八「代数原语」）。"]
#[doc = ""]
#[doc = "语法：`scope!(base, || expr)`：`base` 为路径（字符串字面量或现成的 `PathBuf` 表达式），"]
#[doc = "`expr` 为返回 `Action` 的闭包体。"]
#[doc = ""]
#[doc = "展开说明：`base` 为字符串字面量时转换为 `PathBuf::from(base)`，否则原样使用；"]
#[doc = "`inner` 为闭包调用结果（`Box::new(闭包())`），`next` 收敛为 `Action::Pure(Value::Unit)`"]
#[doc = "（pdr.md §八：局部路径上下文，路径资源作用域化）。"]
#[doc = ""]
#[doc = "用法示例："]
#[doc = "```rust"]
#[doc = "use algeff_macro::scope;"]
#[doc = "use algeff_core::{Action, Value};"]
#[doc = "let s: Action = scope!(\"/var/log/myapp\", || Action::Pure(Value::Unit));"]
#[doc = "assert!(matches!(s, Action::Scope { .. }));"]
#[doc = "```"]
#[doc = ""]
#[doc = "组合示例（宏可组合，pdr.md §14 蓝图形态）：scope! 内层闭包体用 plan! 组合成链："]
#[doc = "```rust"]
#[doc = "use algeff_core::prelude::*;"]
#[doc = "use algeff_macro::*;"]
#[doc = ""]
#[doc = "let s: Action = scope!(\"/var/log/myapp\", || {"]
#[doc = "    plan! {"]
#[doc = "        Action::Pure(Value::Str(\"flush\".into()));"]
#[doc = "        Action::Pure(Value::U64(0));"]
#[doc = "    }"]
#[doc = "});"]
#[doc = ""]
#[doc = "let Action::Scope { base, inner, .. } = s else {"]
#[doc = "    panic!(\"scope! 最外层应为 Scope\");"]
#[doc = "};"]
#[doc = "assert_eq!(base, std::path::PathBuf::from(\"/var/log/myapp\"));"]
#[doc = "assert!(matches!(*inner, Action::Sequential { .. }), \"scope! 内层应为 plan! 组合\");"]
#[doc = "```"]
#[proc_macro]
pub fn scope(input: TokenStream) -> TokenStream {
    let ScopeInput { base, inner } = syn::parse_macro_input!(input as ScopeInput);
    let base = match &base {
        Expr::Lit(lit) if matches!(lit.lit, Lit::Str(_)) => {
            quote! { std::path::PathBuf::from(#base) }
        }
        _ => quote! { #base },
    };
    quote! {
        algeff_core::action::Action::Scope {
            base: #base,
            inner: Box::new((#inner)()),
            next: Box::new(|_| algeff_core::action::Action::Pure(algeff_core::action::Value::Unit)),
        }
    }
    .into()
}

// ---------------------------------------------------------------------------
// choose!(cond, then: e1, else: e2)
// ---------------------------------------------------------------------------

struct ChooseInput {
    cond: Expr,
    then_branch: Expr,
    else_branch: Expr,
}

impl Parse for ChooseInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let cond: Expr = input.parse()?;
        input.parse::<Token![,]>()?;

        let label: Ident = input.parse()?;
        if label != "then" {
            return Err(syn::Error::new(
                label.span(),
                "choose! 期望标签 `then:`（语法：choose!(cond, then: <expr>, else: <expr>)）",
            ));
        }
        input.parse::<Token![:]>()?;
        let then_branch: Expr = input.parse()?;
        input.parse::<Token![,]>()?;

        // `else` 是关键字，需用 Token![else] 而非 Ident
        input.parse::<Token![else]>()?;
        input.parse::<Token![:]>()?;
        let else_branch: Expr = input.parse()?;

        // 容忍尾随逗号，再拒绝任何多余 token
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(input.error(
                "choose! 存在多余 token（语法：choose!(cond, then: <expr>, else: <expr>)）",
            ));
        }
        Ok(Self {
            cond,
            then_branch,
            else_branch,
        })
    }
}

#[doc = "构造 `Action::Choose`（pdr.md §13「宏的使用（可选）」/ §八「代数原语」）。"]
#[doc = ""]
#[doc = "语法：`choose!(cond, then: e1, else: e2)`（容忍尾随逗号；`else` 为关键字，"]
#[doc = "按 `Token![else]` 解析）。"]
#[doc = ""]
#[doc = "展开说明：`cond` 为 bool 表达式，包进 `move |_| cond` 闭包（`CondFn`）；"]
#[doc = "`then_branch`/`else_branch` 分别装箱"]
#[doc = "（pdr.md §八：条件分支，分支内资源隔离）。"]
#[doc = ""]
#[doc = "用法示例："]
#[doc = "```rust"]
#[doc = "use algeff_macro::choose;"]
#[doc = "use algeff_core::{Action, Value};"]
#[doc = "let c: Action = choose!("]
#[doc = "    true,"]
#[doc = "    then: Action::Pure(Value::U64(1)),"]
#[doc = "    else: Action::Pure(Value::U64(2)),"]
#[doc = ");"]
#[doc = "assert!(matches!(c, Action::Choose { .. }));"]
#[doc = "```"]
#[doc = ""]
#[doc = "组合示例（宏可组合，pdr.md §14 蓝图形态）：then/else 分支分别用 plan! 组合成链："]
#[doc = "```rust"]
#[doc = "use algeff_core::prelude::*;"]
#[doc = "use algeff_macro::*;"]
#[doc = ""]
#[doc = "let use_cache = true;"]
#[doc = "let c: Action = choose!("]
#[doc = "    use_cache,"]
#[doc = "    then: plan! {"]
#[doc = "        Action::Pure(Value::Str(\"cache-hit\".into()));"]
#[doc = "        Action::Pure(Value::U64(1));"]
#[doc = "    },"]
#[doc = "    else: plan! {"]
#[doc = "        Action::Pure(Value::Str(\"cache-miss\".into()));"]
#[doc = "        Action::Pure(Value::U64(2));"]
#[doc = "    },"]
#[doc = ");"]
#[doc = ""]
#[doc = "let Action::Choose { cond, then_branch, else_branch, .. } = c else {"]
#[doc = "    panic!(\"choose! 最外层应为 Choose\");"]
#[doc = "};"]
#[doc = "assert!(cond(&Value::Unit), \"cond 闭包应返回捕获的 bool\");"]
#[doc = "assert!(matches!(*then_branch, Action::Sequential { .. }), \"then 分支应为 plan! 组合\");"]
#[doc = "assert!(matches!(*else_branch, Action::Sequential { .. }), \"else 分支应为 plan! 组合\");"]
#[doc = "```"]
#[proc_macro]
pub fn choose(input: TokenStream) -> TokenStream {
    let ChooseInput {
        cond,
        then_branch,
        else_branch,
    } = syn::parse_macro_input!(input as ChooseInput);
    quote! {
        algeff_core::action::Action::Choose {
            cond: Box::new(move |_| #cond),
            then_branch: Box::new(#then_branch),
            else_branch: Box::new(#else_branch),
        }
    }
    .into()
}
