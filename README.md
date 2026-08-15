# Algeff

将 Unix 效应代数化的跨平台确定性运行时框架（Rust，仅依赖 tokio）。

- 完整设计规范：`pdr.md`（v3.2）
- 阶段 0 冻结契约：`contracts.md`
- 工作区成员：`algeff-core`（Action AST + Runtime 内核）/ `algeff-std`（tokio 物理后端）/ `algeff-macro`（可选语法糖）

```bash
cargo check        # 全工作区
cargo test         # 全工作区
```

## 项目简介

Algeff（Algebraic Effects）是一份独立于宿主语言的理论规范与工程实现指南：所有系统交互被编码为不可变的代数数据类型（Action），副作用从「指令（动词）」转变为「数据（名词）」，使控制流可以被自由组合、缓存、重放。可逆性（`trackΓ`/`recoverΓ`）与反应性（`notify`）由运行时模型保证；Rust 编译器负责内存安全，运行时模型负责业务语义安全，两者分层、互不干扰（pdr.md §0–§1）。实现仅依赖 tokio，编译时间秒级。

## 三层 crate 结构（pdr.md §15）

| Crate | 职责 | 代码量估算 | 稳定性 |
| --- | --- | --- | --- |
| `algeff-core` | Action、ResourceSet、Resource\<M\>、Runtime 内核 | ~2000 行 | 永久冻结 |
| `algeff-std` | 预包装适配层（open_tcp、read 等） | ~2500 行 | 永久稳定 |
| `algeff-macro` | 极简语法糖宏（可选） | ~300 行 | 极少修改 |

发布顺序（依赖方向）：`algeff-core` → `algeff-std` → `algeff-macro`，见 `scripts/release.sh`。

## 快速开始

最小 Action 蓝图（完整示例见 pdr.md §14 与 `docs/example.md`）：

```rust
use algeff_core::prelude::*;

// 读取 fd 中的字节：Syscall -> 拿到 Value -> 交给 next
fn read_once(fd: Fd) -> Action {
    Action::Syscall {
        op: DataOp::Read { fd, len: 1024 },
        resources: ResourceSet::default(),
        next: Box::new(|data| Action::Pure(data)),
    }
}
```

> 说明：pdr.md §14 示例风格为 `use algeff::prelude::*`（统一 façade crate，随发布提供）；当前工作区以 `algeff_core` 名义发布。`Action` 全部节点 CPS，递归字段一律 `Box<Action>`（契约 D2）。

## 文档入口

| 入口 | 内容 |
| --- | --- |
| `docs/` | mdBook 文档（概述/架构/示例/路线图）：`mdbook build docs` 后打开 `docs/book/index.html` |
| `spec/` | 形式化规范（A1 拥有，阶段 1 交付后补充 axioms/proofs/audit） |
| `pdr.md` | 完整设计规范 v3.2（权威源） |
| `contracts.md` | 阶段 0 冻结契约、文件所有权表、决策 D1–D12 |
| `scripts/release.sh` | 发布预览脚本（tag 检查 → dry-run → 发布顺序提示） |
