//! Algeff 可选语法糖宏 —— A4 交付（pdr.md §13 / §八）。
//!
//! 仅做 AST 构造，不参与类型系统：
//! - `plan!{ stmt; stmt; ... }` → `Action::Sequential` 链
//! - `fork!{ left: ..., right: ... }` → `Action::Fork`
//! - `scope!("/tmp", || { ... })` → `Action::Scope`
//! - `choose!{ cond, then: ..., else: ... }` → `Action::Choose`

use proc_macro::TokenStream;

fn not_yet(name: &str) -> TokenStream {
    format!(
        "compile_error!(\"algeff-macro: `{}` 尚未实现（A4 阶段 1 交付）\");",
        name
    )
    .parse()
    .expect("生成 compile_error token stream")
}

/// plan!{ ... }：构造 Action::Sequential 链（简单，~100 行展开）。
#[proc_macro]
pub fn plan(_input: TokenStream) -> TokenStream {
    not_yet("plan")
}

/// fork!{ left: ..., right: ... }：构造 Action::Fork（简单，~30 行展开）。
#[proc_macro]
pub fn fork(_input: TokenStream) -> TokenStream {
    not_yet("fork")
}

/// scope!("/tmp", || { ... })：构造 Action::Scope。
#[proc_macro]
pub fn scope(_input: TokenStream) -> TokenStream {
    not_yet("scope")
}

/// choose!{ cond, then: ..., else: ... }：构造 Action::Choose。
#[proc_macro]
pub fn choose(_input: TokenStream) -> TokenStream {
    not_yet("choose")
}
