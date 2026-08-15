# Algeff 规范（A1 拥有）

形式化规范文档（A1 Spec Guardian 交付物，契约 §5 A1 任务）。

| 文件 | 内容 | 状态 |
| --- | --- | --- |
| `axioms.md` | 公理 A1–A7：形式化陈述 + 工程实现位置（文件:函数）+ 验证方式（A6 测试名）+ 风险备注 | 随契约冻结 |
| `proofs.md` | 命题 P1–P5：数学证明 + Rust 测试映射建议（供 A6 落地） | 随契约冻结 |
| `contracts-audit.md` | contracts.md 决策 D1–D13 与 pdr.md 一致性审计（只读 contracts.md） | G2 审计闭环 |

源规范：`pdr.md` v3.2（基线只读，A1 建议修订、CTO 裁决）。
修订流程：RFC → CTO 裁决，禁止擅自改动。
