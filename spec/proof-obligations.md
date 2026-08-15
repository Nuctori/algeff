# 证明义务登记表（Proof Obligations）

> 目的：pdr.md §四 公理 A1–A7 与 §六 命题 P1–P5 的每一条形式化声明，都必须有可验证的
> **证据链闭环**：形式化陈述 → 数学证明/论证 → 测试证据（对抗 E2E + 属性测试）→ 审计结论。
> 本表由 5 轮「对抗审计（E2E 验证）× 形式逻辑审计（证明正确性）」串行驱动更新。
> 状态词：**已履行**（证明+测试+审计三方齐）/ **部分**（缺一方）/ **未履行**（仅形式化陈述）。

## 轮次日志

| 轮 | 对抗审计（E2E） | 形式逻辑审计 | 新证据 | 结论 |
| --- | --- | --- | --- | --- |
| R0（基线） | 既有 25 二进制测试 | proofs.md P1-P5 数学证明 | 见下 | 初始状态 |
| R1 | 进行中（audit/r1） | 待派 | — | — |

## 义务明细

### 公理层（pdr.md §四）

| 义务 | 形式化陈述 | 数学论证位置 | 测试证据（文件:测试） | 对抗 E2E 补充 | 审计结论 |
| --- | --- | --- | --- | --- | --- |
| A1 结合律 | (a;b);c = a;(b;c) | spec/proofs.md | execution_axioms.rs:exec_A1_associativity | 轮 R1 | 待 R1 |
| A2 单位元 | 1;a = a;1 = a | spec/proofs.md | execution_axioms.rs:exec_A2_identity | 轮 R1 | 待 R1 |
| A3 交换律 | Δ(a)∩Δ(b)=∅ ⇒ a∥b=b∥a | spec/proofs.md | commutation.rs:fork_commutation_disjoint | 轮 R1 | 待 R1 |
| A4 资源线性 | Write/Own 恰好消费一次 | spec/proofs.md | axioms.rs:a4_random_read_write_sequence | 轮 R1 | 待 R1 |
| A5 分支隔离 | 左 Write 不影响右 Read | spec/proofs.md | concurrency_stress.rs:fork_same_fd_write | 轮 R1 | 待 R1 |
| A6 撤销双态 | w;w̄=1 | spec/proofs.md | execution_axioms.rs:exec_A6_undo_roundtrip | 轮 R1 | 待 R1 |
| A7 无死锁 | 无循环等待链 | spec/proofs.md + tla/ | arbiter.rs:finite_retry_eventually_succeeds | 轮 R1 | 待 R1 |

### 命题层（pdr.md §六）

| 义务 | 陈述 | 证明位置 | 测试证据 | 审计结论 |
| --- | --- | --- | --- | --- |
| P1 幺半群 | (Action,;,1) | spec/proofs.md | exec_A1 + exec_A2 | 待 R1 |
| P2 交换律 | 资源不相交 ⇒ a∥b=b∥a | spec/proofs.md | commutation.rs | 待 R1 |
| P3 分支写隔离 | Choose/Fork 写隔离 | spec/proofs.md | fork_same_fd_write | 待 R1 |
| P4 撤销双态 | w;w̄ 状态恢复 | spec/proofs.md | exec_A6 + e2e undo | 待 R1 |
| P5 无死锁 | 调度器无环 | spec/proofs.md + tla/scheduler.tla | arbiter + concurrent_arbiter_claims | 待 R1 |

## 更新规则

- 每轮对抗审计合并后：将新增 E2E 测试填入「对抗 E2E 补充」列。
- 每轮形式逻辑审计后：将证明有效性结论填入「审计结论」列（含证明链缺口）。
- 5 轮结束时：每条义务必须为「已履行」或列出明确差距（缺证明 / 缺测试 / 缺审计）。
