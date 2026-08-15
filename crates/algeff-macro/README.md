# algeff-macro

Algeff 可选语法糖宏（pdr.md §13「宏的使用（可选）」/ §八「代数原语」）。

## 定位

本 crate 是**可选语法糖**：核心（`algeff-core`）不依赖任何宏，所有蓝图都可以
手写 `Action` 链等价表达。宏只做 AST 构造（syn/quote 拼接），**不参与类型系统、
不增加编译负担**（pdr.md §八）。发布层面属于三层结构中的极简可选项（pdr.md §15：
~300 行、极少修改）。

## 四个宏

| 宏 | 展开为 | 对应原语（pdr.md §八） |
| --- | --- | --- |
| `plan!` | `Action::Sequential` 链 | 顺序执行 |
| `fork!` | `Action::Fork` | 并发分叉（左/右资源集自动分裂） |
| `scope!` | `Action::Scope` | 局部路径上下文（路径资源作用域化） |
| `choose!` | `Action::Choose` | 条件分支（分支内资源隔离） |

原语拓扑语义、物理实现与访问模式约束见 pdr.md §八「代数原语（DSL 名称与语义）」表。

## 与 algeff-core Action 的关系

宏的展开产物是 `algeff_core::action::Action` 值：一律引用 `algeff_core::action::`
路径构造节点，宏本身不做任何类型检查，展开后的类型检查由编译器对 `Action` 定义
完成。因此使用宏的代码需依赖 algeff-core，推荐配合其 prelude 使用：

```rust
use algeff_core::prelude::*; // 提供 Action / Value 等类型
use algeff_macro::plan;
```

## 用法示例

### plan!（构造 Sequential 链）

```rust
use algeff_macro::plan;
use algeff_core::{Action, Value};

let p: Action = plan! {
    Action::Pure(Value::U64(1));
    Action::Pure(Value::U64(2));
};
assert!(matches!(p, Action::Sequential { .. }));
```

### fork!（并发分叉）

```rust
use algeff_macro::fork;
use algeff_core::{Action, Value};

let f: Action = fork! {
    left: Action::Pure(Value::U64(1)),
    right: Action::Pure(Value::U64(2)),
};
assert!(matches!(f, Action::Fork { .. }));
```

### scope!（路径作用域）

```rust
use algeff_macro::scope;
use algeff_core::{Action, Value};

let s: Action = scope!("/var/log/myapp", || Action::Pure(Value::Unit));
assert!(matches!(s, Action::Scope { .. }));
```

### choose!（条件分支）

```rust
use algeff_macro::choose;
use algeff_core::{Action, Value};

let c: Action = choose!(
    true,
    then: Action::Pure(Value::U64(1)),
    else: Action::Pure(Value::U64(2)),
);
assert!(matches!(c, Action::Choose { .. }));
```

> 说明：`scope!` 的 `base` 接受字符串字面量（自动转 `PathBuf`）或现成的
> `PathBuf` 表达式；`choose!` 的 `else` 为关键字分支标签。宏可组合
> （如 `scope!` 闭包体内嵌套其他宏），仅 AST 拼接。

## 参考

- pdr.md §八：代数原语（DSL 名称与语义）
- pdr.md §13：宏的使用（可选）
- pdr.md §15：三层发布结构
