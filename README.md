# Algeff

将 Unix 效应代数化的跨平台确定性运行时框架（Rust，仅依赖 tokio）。

- 完整设计规范：`pdr.md`（v3.2）
- 阶段 0 冻结契约：`contracts.md`
- 工作区成员：`algeff-core`（Action AST + Runtime 内核）/ `algeff-std`（tokio 物理后端）/ `algeff-macro`（可选语法糖）

```bash
cargo check        # 全工作区
cargo test         # 全工作区
```
