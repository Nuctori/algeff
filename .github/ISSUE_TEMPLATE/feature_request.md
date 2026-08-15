---
name: 功能请求（Feature Request）
about: 新功能 / 接口变更建议（需评估契约影响，走 RFC 流程）
title: "[feature] 简述功能"
labels: enhancement
assignees: ""
---

<!--
对齐 pdr.md §19.2 闭环机制与 contracts.md 契约冻结规则：
- 新功能若触碰冻结文件（contracts.md §1 所有权表标注「冻结」或 CTO 所有的文件），
  必须先走 RFC → CTO 裁决，禁止擅自修改。
- 跨 crate 需求（如 std 需要 core 新 API）：先自行绕开或本地实现，
  报告中写明「RFC 建议」，由 CTO 批量裁决（contracts.md §6 工作流）。
-->

## 涉及的模块（core/std/macro/ci）

<!-- 按 contracts.md §1 文件所有权表勾选，便于按模块归属负责 Agent：
     core（crates/algeff-core）→ A2/A3；std（crates/algeff-std）→ A5；
     macro（crates/algeff-macro）→ A4；ci（.github/、scripts/、docs/）→ A8 -->
- [ ] **core**（crates/algeff-core，A2/A3）
- [ ] **std**（crates/algeff-std，A5）
- [ ] **macro**（crates/algeff-macro，A4）
- [ ] **ci**（.github/、scripts/、docs/，A8）

## 动机

<!-- 为什么需要这个功能？解决什么问题？（对照 pdr.md 对应章节） -->

## 期望行为

<!-- 功能的具体表现，尽量给出伪代码或使用示例 -->

## 性能数据（如适用）

<!-- 若涉及性能：目标基准、与 pdr.md §16 预期值的对照；不涉及则填「不适用」 -->

## 契约影响评估

<!-- 是否触碰冻结文件（contracts.md §1 标注「冻结」的文件 / Cargo.toml / pdr.md）？
     若触碰，需要 RFC → CTO 裁决。 -->
- [ ] 不触碰冻结文件，可直接实施
- [ ] 触碰冻结文件，需要 RFC 流程

## 参考实现 / 文档

<!-- 可选：pdr.md 章节号、spec/ 公理编号、相关 Issue 链接 -->
