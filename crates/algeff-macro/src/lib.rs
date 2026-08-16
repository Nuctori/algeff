//! Algeff 可选语法糖宏（pdr.md §13「宏的使用（可选）」/ §八「代数原语」）。
//!
//! 本 crate 是**可选语法糖**：algeff-core 核心不依赖任何宏，所有蓝图均可手写
//! `Action` 链等价表达；宏仅做 AST 构造（syn/quote 拼接），**不参与类型系统**，
//! 不增加编译负担（pdr.md §八）。
//!
//! 提供五个宏：
//! - `plan!{ stmt; stmt; ... }` → `Action::Sequential` 链
//! - `fork!{ left: ..., right: ... }` → `Action::Fork`
//! - `scope!("/tmp", || { ... })` → `Action::Scope`
//! - `choose!(cond, then: ..., else: ...)` → `Action::Choose`
//! - `do_!{ stmt; ...; 尾表达式 }` → 命令式 CPS 链（配合 `algeff_std::dx`）
//!
//! 展开产物一律引用 `algeff_core::action::` 路径构造节点，宏本身不做任何类型
//! 检查。使用宏的代码需依赖 algeff-core，推荐配合其 prelude 使用：
//! `use algeff_core::prelude::*;`（提供 `Action`/`Value` 等类型）。

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Block, Expr, ExprMacro, Ident, Lit, Pat, Stmt, Token};

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

// ---------------------------------------------------------------------------
// do_!{ stmt; stmt; ... ; 尾表达式 }（命令式语法糖，配合 algeff_std::dx）
// ---------------------------------------------------------------------------

/// 命令式顺序链宏（DX 迭代 1，配套 `algeff_std::dx` 模块）。
///
/// 把一段「正常操作」风格的语句序列编译成 `Action` CPS 链（`and_then` 嵌套），
/// 语句间值传递经闭包参数完成（`let` 绑定的是原始 `Value`，使用处用
/// `dx::expect_*`/dx 操作提取）。**不引入任何新 Action 节点**——展开产物
/// 与手写 `Action::Sequential`/`and_then` 链完全等价，蓝图仍是纯数据。
/// 名称 `do_`（下划线）是因为 `do` 是 Rust 保留关键字。
///
/// 语法（`{}` 块）：
/// - `let <标识符> = <Action 表达式>;` —— 执行并把**结果值**绑定到标识符；
/// - `let _ = <Action 表达式>;` / `<Action 表达式>;` —— 执行并丢弃结果；
/// - 尾表达式（不带分号）—— 链的**最终值**，展开为 `dx::pure(尾表达式)`
///   （`pure: Value → Action`）；省略时收敛为 `Action::Pure(Value::Unit)`。
///
/// 语句内**任何返回 `Action` 的表达式**都可用（`dx::open`/`plan!`/`scope!`/
/// `choose!`/嵌套 `do_!`）。分支/循环体需要多条语句时，用嵌套 `do_!` 块。
///
/// **尾表达式必须是 `Value`**：若误写为 `Action`（如把 `dx::read(...)` 直接
/// 放在块末不加分号），编译器报 `expected Value, found Action`（诊断指向
/// `dx::pure` 的签名，A1 发现：经 `dx::pure` 包装比裸 `Action::Pure` 更贴近
/// DX 层语义）——补分号使其成为语句即可。
///
/// 展开说明：`let x = e;` → `algeff_std::dx::and_then(e, move |x| { 后续 })`；
/// 表达式语句 → `and_then(e, move |_| { 后续 })`；尾表达式 → 最内层
/// `dx::pure(尾表达式)`。使用本宏的代码需依赖 `algeff-std`（提供
/// `dx::and_then`）与 `algeff-core`。
///
/// 用法示例（配合 `algeff_std::dx`，资源声明自动推导）：
/// ```rust
/// use algeff_core::{Action, DataOp, Value};
/// use algeff_macro::do_;
/// use algeff_std::dx;
///
/// // 非 Pure 语句展开为 and_then 链；`let` 绑定 = 语句的结果值（Value）
/// let p: Action = do_! {
///     let t = dx::get_time();
///     t // 尾表达式 = 链的最终值
/// };
/// assert!(matches!(p, Action::Sequential { .. }));
/// let Action::Sequential { current, next } = p else {
///     panic!("do_! 应展开为 Sequential 链");
/// };
/// assert!(matches!(*current, Action::Syscall { op: DataOp::GetTime, .. }));
/// assert!(matches!(next(Value::U64(123)), Action::Pure(Value::U64(123))));
/// ```
///
/// 真实文件 IO 示例见 README §3 与 `algeff_std::dx` 模块文档。
#[proc_macro]
pub fn do_(input: TokenStream) -> TokenStream {
    // 调用点 `do_! { stmts }` 的定界符 `{}` 不进入输入流，补包一层再按 Block 解析
    // （否则 syn 的 `Block` parse 会报 "expected curly braces"）。
    let input2: TokenStream2 = input.into();
    let block: Block = match syn::parse2(quote!({ #input2 })) {
        Ok(b) => b,
        Err(e) => return e.to_compile_error().into(),
    };

    // 分离语句与尾表达式（最后一个无分号元素）。
    let mut stmts: Vec<Stmt> = block.stmts;
    let tail: Option<Expr> = match stmts.pop() {
        Some(Stmt::Expr(e, None)) => Some(e),
        Some(Stmt::Macro(sm)) if sm.semi_token.is_none() => Some(Expr::Macro(ExprMacro {
            attrs: sm.attrs,
            mac: sm.mac,
        })),
        Some(other) => {
            stmts.push(other);
            None
        }
        None => None,
    };

    // 从尾向前折叠成 and_then 链。
    let mut out: TokenStream2 = match tail {
        Some(e) => quote! {
            algeff_std::dx::pure(#e)
        },
        None => pure_unit(),
    };

    for stmt in stmts.into_iter().rev() {
        match stmt {
            Stmt::Local(local) => {
                let init = match local.init {
                    Some(init) => *init.expr,
                    None => {
                        return syn::Error::new(
                            local.let_token.span,
                            "do_! 的 let 必须有初始值（语法：let <标识符> = <Action 表达式>;）",
                        )
                        .to_compile_error()
                        .into();
                    }
                };
                match &local.pat {
                    Pat::Ident(pi) => {
                        let name = &pi.ident;
                        let mut_ = &pi.mutability;
                        out = quote! {
                            algeff_std::dx::and_then(#init, move |#mut_ #name| { #out })
                        };
                    }
                    Pat::Wild(_) => {
                        out = quote! {
                            algeff_std::dx::and_then(#init, move |_| { #out })
                        };
                    }
                    _ => {
                        return syn::Error::new(
                            local.pat.span(),
                            "do_! 仅支持 `let <标识符> = <表达式>;` 或 `let _ = <表达式>;`",
                        )
                        .to_compile_error()
                        .into();
                    }
                }
            }
            Stmt::Expr(e, _) => {
                out = quote! {
                    algeff_std::dx::and_then(#e, move |_| { #out })
                };
            }
            Stmt::Macro(sm) => {
                let mac = Expr::Macro(ExprMacro {
                    attrs: sm.attrs,
                    mac: sm.mac,
                });
                out = quote! {
                    algeff_std::dx::and_then(#mac, move |_| { #out })
                };
            }
            Stmt::Item(_) => {
                return syn::Error::new(
                    proc_macro2::Span::call_site(),
                    "do_! 不支持块内 item 声明（fn/struct/use 等请放在宏外）",
                )
                .to_compile_error()
                .into();
            }
        }
    }
    out.into()
}
