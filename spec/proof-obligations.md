# 证明义务登记表（Proof Obligations）

> 目的：pdr.md §四 公理 A1–A7 与 §六 命题 P1–P5 的每一条形式化声明，都必须有可验证的
> **证据链闭环**：形式化陈述 → 数学证明/论证 → 测试证据（对抗 E2E + 属性测试）→ 审计结论。
> 本表由 5 轮「对抗审计（E2E 验证）× 形式逻辑审计（证明正确性）」串行驱动更新。
> 状态词：**已履行**（证明+测试+审计三方齐）/ **部分**（缺一方）/ **未履行**（仅形式化陈述）。

## 轮次日志

| 轮 | 对抗审计（E2E） | 形式逻辑审计 | 新证据 | 结论 |
| --- | --- | --- | --- | --- |
| R0（基线） | 既有 25 二进制测试 | proofs.md P1-P5 数学证明 | 见下 | 初始状态 |
| R1 | ✅ 完成（adversarial_r1.rs 17 测试，5d24166）：游标撤销修复 + Replace 旧 fd 可写反例 + 12 项声称验证 | ✅ 完成（formal-convergence）：P1 有效、P2 有缺口（combine 对称未入 A3 陈述）、P3/P4/P5 部分（证据补足中/句柄反例/映射修正） | 游标恢复（A6 新维度证据）；旧 fd 句柄活性反例（D10/P4 未闭环） | 4 处未收敛点 → 派发修复（A6 批8 隔离测试 / proofs·axioms 修正 / 登记表更新） |

## 义务明细

### 公理层（pdr.md §四）

| 义务 | 形式化陈述 | 数学论证位置 | 测试证据（文件:测试） | 对抗 E2E 补充 | 审计结论 |
| --- | --- | --- | --- | --- | --- |
| A1 结合律 | (a;b);c = a;(b;c) | spec/proofs.md | execution_axioms.rs:exec_A1_associativity；辅助压力证据：adversarial_r1.rs:conc_repeat_blueprint_100_rounds_deterministic（100 轮确定性——序列稳定维度，非直接结合律） | 轮 R1 ✅ | ✅ 有效（R1：演绎链严格，执行级+对抗双证据） |
| A2 单位元 | 1;a = a;1 = a | spec/proofs.md | execution_axioms.rs:exec_A2_identity | 轮 R1 ✅ | ✅ 有效（R1：Pure 前缀/后缀/双侧 op 序列一致） |
| A3 交换律 | Δ(a)∩Δ(b)=∅ ⇒ a∥b=b∥a | spec/proofs.md | commutation.rs:fork_commutation_disjoint；辅助压力证据：adversarial_r1.rs:conc_fork_parallel_two_files_both_handles_readable（并行双分支句柄活性——非直接交换律） | 轮 R1 ✅ | ⚠️ 有缺口（R1：combine 对称性未入 A3 形式化陈述——已记录待 R2 修） |
| A4 资源线性 | Write/Own 恰好消费一次 | spec/proofs.md | axioms.rs:a4_random_read_write_sequence + adversarial_r1.rs:lin_fork_conflict_double_write_then_parent_blocked（冲突 Fork 后父级拦截）+ lin_stale_fd_write_after_replace_succeeds（Replace 后线性标记清空但句柄残留反例） | 轮 R1 ✅ | ⚠️ 部分（R1：线性标记维度闭环；句柄活性反例→RFC-05 登记） |
| A5 分支隔离 | 左 Write 不影响右 Read | spec/proofs.md | concurrency_stress.rs:fork_same_fd_write（registry 副本隔离）；branch_isolation.rs:exec_P3_fork_left_write_right_read_isolated（读隔离，A6 批8 已合并） | 轮 R1 ✅ | ⚠️ 部分（R1：证据-义务不匹配已识别；读隔离测试已补足，make_mut 物理 COW 归阶段 3） |
| A6 撤销双态 | w;w̄=1 | spec/proofs.md | execution_axioms.rs:exec_A6_undo_roundtrip + adversarial_r1.rs:rev_undo_restores_file_cursor（游标维度新证据） | 轮 R1 ✅ | ⚠️ 部分（R1：内容+游标维度闭环；句柄活性维度有反例——Replace 后旧 fd 仍可写，RFC-05） |
| A7 无死锁 | 无循环等待链 | spec/proofs.md + tla/ | arbiter.rs:finite_retry_eventually_succeeds + arbiter_mutex.rs（R-1 强制） | 轮 R1 | ⚠️ 部分→已收敛（R1：模型/原语/执行器三层已交付，运行时载体为「冲突→顺序+分支不相交+单执行器锁」——axioms.md M4 修正后分层如实） |

### 命题层（pdr.md §六）

| 义务 | 陈述 | 证明位置 | 测试证据 | 审计结论 |
| --- | --- | --- | --- | --- |
| P1 幺半群 | (Action,;,1) | spec/proofs.md | exec_A1 + exec_A2 | ✅ 有效（R1：演绎链严格，隐含前提由执行级测试补足） |
| P2 交换律 | 资源不相交 ⇒ a∥b=b∥a | spec/proofs.md | commutation.rs | ⚠️ 有缺口（R1：combine 对称性未入 A3 形式化陈述——义务链不一致；Δ 覆盖未声明；已记录待 R2 修） |
| P3 分支写隔离 | Choose/Fork 写隔离 | spec/proofs.md | fork_same_fd_write + branch_isolation.rs（补足中） | ⚠️ 有缺口（R1：Choose 读隔离无测试 + make_mut 未实现——A6 批8 补测试，make_mut 推迟阶段 3 已记录） |
| P4 撤销双态 | w;w̄ 状态恢复 | spec/proofs.md | exec_A6 + e2e undo + adversarial_r1.rs | ⚠️ 部分（R1：证明有效、Full 边界正确；句柄活性维度反例未闭环——RFC-05 登记） |
| P5 无死锁 | 调度器无环 | spec/proofs.md + tla/scheduler.tla | arbiter + concurrent_arbiter_claims + arbiter_mutex.rs | ⚠️ 部分→已收敛（R1：模型论证有效；工程映射失真已修正——axioms.md M4 分层如实描述，不再声称运行时动态占坑） |

## 更新规则

- 每轮对抗审计合并后：将新增 E2E 测试填入「对抗 E2E 补充」列。
- 每轮形式逻辑审计后：将证明有效性结论填入「审计结论」列（含证明链缺口）。
- 5 轮结束时：每条义务必须为「已履行」或列出明确差距（缺证明 / 缺测试 / 缺审计）。
