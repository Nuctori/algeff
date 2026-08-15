---
name: 缺陷报告（Bug）
about: 测试失败 / 运行时错误 / 性能不达标（对齐 pdr.md §19.2 反馈循环）
title: "[bug] 简述问题"
labels: bug
assignees: ""
---

<!--
本模板对齐 pdr.md §19.2「反馈循环」机制：
测试失败或性能不达标 → 自动创建 Issue → 对应 Agent 修复。
CI 失败自动创建的 Issue 会预填「涉及的模块 / 测试失败输出 / 性能数据」三节；
人工提交时请尽量补全，便于按 contracts.md §1 所有权表自动归属到负责 Agent。
-->

## 涉及的模块（core/std/macro/ci）

<!-- 按 contracts.md §1 文件所有权表勾选，决定 Issue 归属 Agent：
     core（crates/algeff-core）→ A2/A3；std（crates/algeff-std）→ A5；
     macro（crates/algeff-macro）→ A4；ci（.github/、scripts/、docs/）→ A8 -->
- [ ] **core**（crates/algeff-core，A2/A3）
- [ ] **std**（crates/algeff-std，A5）
- [ ] **macro**（crates/algeff-macro，A4）
- [ ] **ci**（.github/、scripts/、docs/，A8）

## 测试失败输出

<!-- 粘贴 `cargo test --workspace` 失败输出：测试名、断言、错误摘要；或 CI 日志链接。
     无测试失败时可填「不适用」。 -->

```text
（粘贴失败输出）
```

## 性能数据（如适用）

<!-- 若为性能不达标：基准命令（如 scripts/perf.sh）、前后基线、期望值（对照 pdr.md §16）。
     若不涉及性能，填「不适用」。 -->

## 复现步骤

1. 
2. 
3. 

## 期望行为

## 实际行为

## 环境信息

- OS：Ubuntu / Windows
- Rust 版本（`rustc --version`）：
- 分支 / 提交（`git rev-parse --abbrev-ref HEAD`）：
