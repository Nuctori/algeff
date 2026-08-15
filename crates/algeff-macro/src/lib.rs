//! Algeff 可选语法糖宏 —— A4 交付（pdr.md §13 / §八）。
//!
//! 仅做 AST 构造，不参与类型系统：
//! - `plan!{ stmt; stmt; ... }` → `Action::Sequential` 链
//! - `fork!{ left: ..., right: ... }` → `Action::Fork`
//! - `scope!("/tmp", || { ... })` → `Action::Scope`
//! - `choose!{ cond, then: ..., else: ... }` → `Action::Choose`
//!
//! 展开产物一律引用 `algeff_core::action::` 路径；宏只拼 AST，
//! 不做任何类型检查（pdr.md §13：核心不依赖宏，宏仅为语法糖）。

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

/// `plan!{ ... }`：构造 `Action::Sequential` 链（pdr.md §13）。
///
/// 每个元素为返回 `Action` 的表达式；`current` 逐个装箱，
/// `next` 忽略前一值继续链，最后一个 `next` 收敛为 `Pure(Unit)`。
/// 空列表展开为 `Action::Pure(Value::Unit)`。
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
            return Err(syn::Error::new(label.span(), "fork! 期望标签 `left:`"));
        }
        input.parse::<Token![:]>()?;
        let left: Expr = input.parse()?;
        input.parse::<Token![,]>()?;

        let label: Ident = input.parse()?;
        if label != "right" {
            return Err(syn::Error::new(label.span(), "fork! 期望标签 `right:`"));
        }
        input.parse::<Token![:]>()?;
        let right: Expr = input.parse()?;

        // 容忍尾随逗号，再拒绝任何多余 token
        let _ = input.parse::<Token![,]>();
        if !input.is_empty() {
            return Err(input.error("fork! 存在多余 token"));
        }
        Ok(Self { left, right })
    }
}

/// `fork!{ left: ..., right: ... }`：构造 `Action::Fork`（pdr.md §13）。
///
/// `combine` 固定为忽略两侧值的 `Pure(Unit)` 收敛闭包。
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
            return Err(input.error("scope! 存在多余 token"));
        }
        Ok(Self { base, inner })
    }
}

/// `scope!(base, || expr)`：构造 `Action::Scope`（pdr.md §13）。
///
/// `base` 支持字符串字面量（转 `PathBuf::from`）或现成的 `PathBuf` 表达式；
/// `inner` 为闭包调用结果（`Box::new(闭包())`），`next` 收敛为 `Pure(Unit)`。
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
            return Err(syn::Error::new(label.span(), "choose! 期望标签 `then:`"));
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
            return Err(input.error("choose! 存在多余 token"));
        }
        Ok(Self {
            cond,
            then_branch,
            else_branch,
        })
    }
}

/// `choose!(cond, then: ..., else: ...)`：构造 `Action::Choose`（pdr.md §13）。
///
/// `cond` 为 bool 表达式，包进 `move |_| cond` 闭包（`CondFn`），
/// 分支字段分别装箱。
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
