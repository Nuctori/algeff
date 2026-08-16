# 证明义务

> 本页呈现 `spec/proof-obligations.md`（证明义务登记表）的文档视图；**权威源以 `spec/proof-obligations.md` 为准**，该表由 A1（Spec Guardian）维护、随 5 轮对抗审计更新。测试证据文件位于 `crates/algeff-core/tests/`（A2/A6 所有）与 `tla/`（A6 所有），数学证明位于 `spec/proofs.md`。

## 目的与证据链

pdr.md §四 公理 A1–A7 与 §六 命题 P1–P5 的每一条形式化声明，都必须有可验证的**证据链闭环**：

1. **形式化陈述** → 2. **数学证明/论证**（`spec/proofs.md`）→ 3. **测试证据**（对抗 E2E + 属性测试，`crates/algeff-core/tests/`）→ 4. **审计结论**（对抗审计 × 形式逻辑审计）。

本表由 5 轮「对抗审计（E2E 验证）× 形式逻辑审计（证明正确性）」串行驱动更新。

## 状态词

| 状态 | 含义 |
| --- | --- |
| **已履行** | 证明 + 测试 + 审计三方齐 |
| **部分** | 缺一方 |
| **未履行** | 仅形式化陈述 |

## 轮次日志

| 轮 | 对抗审计（E2E） | 形式逻辑审计 | 新证据 | 结论 |
| --- | --- | --- | --- | --- |
| R0（基线） | 既有 25 二进制测试 | proofs.md P1-P5 数学证明 | 见下 | 初始状态 |
| R1 | ✅ 完成（adversarial_r1.rs 17 测试，5d24166）：游标撤销修复 + Replace 旧 fd 可写反例 + 12 项声称验证 | ✅ 完成（formal-convergence）：P1 有效、P2 有缺口（combine 对称未入 A3 陈述）、P3/P4/P5 部分（证据补足中/句柄反例/映射修正） | 游标恢复（A6 新维度证据）；旧 fd 句柄活性反例（D10/P4 未闭环） | 4 处未收敛点 → 派发修复（A6 批8 隔离测试 / proofs·axioms 修正 / 登记表更新） |
| R2 | ✅ 完成（adversarial_r2.rs 19 测试，2eb7312）：fd 区间压力/arbiter 争用/R1 回归/错误路径/时间面/资源计数 + RFC-06/07/08 登记 + flaky 修复（写后 flush，Write 返回⇔OS 落盘可观察性契约 D-039） | ✅ 完成（formal-convergence）：P2 收敛为「有效（附声明前提）」；A3 陈述已并入 Sym(f)+Δ-覆盖；RFC-06→D1 边界反例、RFC-08→P4/A6 部分可撤销反例；axioms 总表死信息清除 | RFC-06（fd 二次增长 u64 溢出）、RFC-07（管道半端）、RFC-08（Timeout 孤儿分支） | 3 处登记级未收敛点已修（A3 陈述/P2 同步、总表刷新、RFC-08 编号） |
| R3 | ✅ 完成（adversarial_r3{a,b,c}.rs 共 28 测试：r3a 11 Catch/Scope/撤销栈 + r3b 9 Alloc/确定性/用户责任 + r3c 8 网络/R2 回归）；发现：闭包盲区实证、SendFile flush 缺口（已修）、锁饥饿（RFC-09）、退避串行化（LOW） | ✅ 完成（formal-convergence）：P3/P5 升级「有效（附范围声明）」；盲区=系统性（链长≥2 仅首 op 可见）→ 前提入 spec 三处；A6/P4 范围限定句（trackΓ+RFC-08/09）落地；P2 闭环确认 | RFC-09（Timeout 锁饥饿）、盲区实证（2 测试）、SendFile flush（D-039 扩展，3922bf6） | 5 处登记级修正已落地（A6/P4 限定句、盲区前提、P3 文本同步、义务表收口、README 计数） |
| R4 | ✅ 完成（adversarial_r4{a,b,c}.rs 共 32 测试：组合态深挖/多 Runtime 隔离·错误透传·Open 矩阵/规模栈深）；发现：RFC-10（Windows 错误码）、RFC-11（递归栈溢出 HIGH，已修复） | ✅ 完成（formal-convergence）：P1 有效（1000 链规模证据）、P2 有效附声明前提（16 路）、P3/P5 有效附范围声明、P4 收敛中；6 处极小性修正已落地 | RFC-10（Windows errno 映射）、RFC-11（深度守卫，阈值 96，已修） | 收口完成：README 300、A4 证据引用、make_mut 残留清零、RFC-A3-3 核销 |
| R5 | ✅ 完成（r5a 8 + r5b 16 测试，945eb52/1ce734c）：五连回归/边界 95/96/97/50 轮风暴/修复点交互；Invoke 假执行器 5 面/错误恢复长链/Fork×Scope 隔离/Timeout 三层链/蓝图复用 | ✅ 完成（formal-convergence 终轮）：P1/P2/P3/P5 = 有效（附声明）终判；P4 = 部分（RFC-05 未闭环——唯一开放差距，阶段 3+ 已裁决）；极小性收口（RFC-05 补录、README 300/292/40、A1 深度注记） | 守卫边界 95/96/97；五连回归；Invoke 正向语义首证；蓝图 4 路复用；跨 Runtime 别名 undo 非恒等偏差（文档化） | **5 轮收官**：P1/P2/P3/P5 有效（附声明），P4 部分（明确差距 RFC-05）；A1-A7 终态齐备 |
| R6 | ✅ 完成（adversarial_r6 14 + r6b 18 + r6c 5 + r6_f2 5 共 42 测试，8 路并发）：错误映射三级链核验/40+ 转换点回归/README 示例真实性/线性回滚；发现：F1（CrossDevice 撞码错映射）、F2（线性标记毒化）、B2（批内部分失败残留）、macOS errno 语义 | ✅ 完成（formal-convergence）：RFC-10 修复后错误映射形式一致性（Unix 透传≡冻结面逐位一致、Windows kind 优先+码表全序一致、14 集闭包保持）；P1-P5 影响面 = 零（runtime 对变体零分支）；README 声明逐项核对 | JD-1 kind-first 跨平台映射（修复）；RFC-12 失败路径线性回滚（修复）；A8 CI 特性限定（virtual-clock 与 std 测试冲突实证）；perf 复测（echo 99.9% 无回归、append 39.1% = D-039 flush 契约成本）；E2E 累计 164 | **R6 闭环**：4 项修复落地（JD-1/2/3/5、RFC-12、B2 前缀回滚、CI 特性限定）；P1-P5 终判不变；计数终值 352/46+3；义务表终态齐备 |

## 义务明细

### 公理层（pdr.md §四）

| 义务 | 形式化陈述 | 数学论证位置 | 测试证据（文件:测试） | 对抗 E2E 补充 | 审计结论 |
| --- | --- | --- | --- | --- | --- |
| A1 结合律 | (a;b);c = a;(b;c) | spec/proofs.md | execution_axioms.rs:exec_A1_associativity；辅助压力证据：adversarial_r1.rs:conc_repeat_blueprint_100_rounds_deterministic（100 轮确定性——序列稳定维度，非直接结合律） | 轮 R1 ✅ | ✅ 有效（R1：演绎链严格，执行级+对抗双证据；深度承诺域见 P1 行——≤64 左结合/无限制右结合（迭代 1 阈值 96→64）） |
| A2 单位元 | 1;a = a;1 = a | spec/proofs.md | execution_axioms.rs:exec_A2_identity | 轮 R1 ✅ | ✅ 有效（R1：Pure 前缀/后缀/双侧 op 序列一致） |
| A3 交换律 | Δ(a)∩Δ(b)=∅ ∧ Sym(f) ∧ Cov(Δ) ⇒ Fork(a,b,f)≡Fork(b,a,f) | spec/proofs.md（R2：Sym/Cov 并入陈述；R3：静态可见性前提声明） | commutation.rs:fork_commutation_disjoint + fork_commutation_same_value + a3_can_parallel_symmetric（proptest 对称）；辅助压力：adversarial_r1.rs:conc_fork_parallel_two_files_both_handles_readable、adversarial_r2.rs fd 分配完整性、adversarial_r3b.rs:ub_fork_conflict_blindspot_*（盲区实证） | 轮 R1/R2/R3 ✅ | ✅ 有效（附声明前提）（R2 收敛 + R3：盲区前提已入 axioms/proofs——系统性不完全声明，工程应用范围限定） |
| A4 资源线性 | Write/Own 恰好消费一次 | spec/proofs.md | axioms.rs:a4_random_read_write_sequence + adversarial_r1.rs:lin_fork_conflict_double_write_then_parent_blocked（冲突 Fork 后父级拦截）+ lin_stale_fd_write_after_replace_fails（RFC-05 闭合：Replace 后旧 fd Write → NotFound，原 succeeds 反例测试反转）+ adversarial_r3a.rs:d10_replace_resets_linearity_same_resource_rewrite（D10 复位）+ 盲区线性保持（闭包内 Write 经 merge 仍并入父） | 轮 R1/R3/R7 ✅ | ✅ 有效（R1/R3 线性标记维度闭环 + R7 迭代 1：RFC-05 句柄回收闭合——registry 为 fd 活性唯一真相，旧 fd 全操作失败） |
| A5 分支隔离 | 左 Write 不影响右 Read | spec/proofs.md | concurrency_stress.rs:fork_same_fd_write（registry 副本隔离）；branch_isolation.rs:exec_P3_fork_left_write_right_read_isolated（读隔离，A6 批8 已合并） | 轮 R1 ✅ | ⚠️ 部分→语义层闭环（R1：证据-义务不匹配已识别；A6 批8 读隔离测试补足；make_mut 物理 COW 归阶段 3——§9 评估） |
| A6 撤销双态 | w;w̄=1 | spec/proofs.md（R3：trackΓ+Full 范围限定句，RFC-08/09 例外） | execution_axioms.rs:exec_A6_undo_roundtrip + adversarial_r1.rs:rev_undo_restores_file_cursor（游标）+ adversarial_r3a.rs:catch 组合 undo 保留 + r3c.rs:r2_sendfile_file_target_visibility（D-039 扩展） | 轮 R1/R3 ✅ | ⚠️ 部分（R1/R3：内容/游标/落盘/组合维度闭环；范围例外 RFC-05/08/09 已登记，recover 点 w;w̄=1 仍成立） |
| A7 无死锁 | 无循环等待链 | spec/proofs.md + tla/ | arbiter.rs:finite_retry_eventually_succeeds + arbiter_mutex.rs（R-1 强制）+ adversarial_r3c.rs:r2_arbiter_8x30_mixed_timeout_storm_no_poison（风暴 240 轮确定结果） | 轮 R1/R2/R3 ✅ | ⚠️ 部分→已收敛（R1/R3：三层+风暴证据；RFC-09 锁饥饿=有界公平性缺陷非死锁非活锁，P5 边界句已补） |

### 命题层（pdr.md §六）

| 义务 | 陈述 | 证明位置 | 测试证据 | 审计结论 |
| --- | --- | --- | --- | --- |
| P1 幺半群 | (Action,;,1) | spec/proofs.md | exec_A1 + exec_A2 + adversarial_r4c.rs（1000 节点平链规模证据） | ✅ 有效（R1/R4：演绎链严格，规模证据；**范围前提（R5 审查，迭代 1 更新）**：深度守卫阈值 64 下结合律在深度 ≤64 内成立——**左结合形式 `(a;b);c` 型 current 嵌套先触及阈值**（消耗深度 = 链长-1），右结合 `a;(b;c)` 型 next-CPS 延续恒为深度 1；边界处两种结合形式行为可不对称，超限行为不在结合律承诺内；`seq()` 左折叠 ≥65 步返回 Err(Other(105))） |
| P2 交换律 | Δ(a)∩Δ(b)=∅ ∧ Sym(f) ∧ Cov(Δ) ⇒ Fork(a,b,f)≡Fork(b,a,f) | spec/proofs.md（R2 修正 + R3 静态可见性前提） | commutation.rs + a3_can_parallel_symmetric + adversarial_r3b.rs 盲区实证 | ✅ 有效（附声明前提）（R2 收敛 + R3：盲区前提入依赖清单，工程应用范围限定） |
| P3 分支写隔离 | Choose/Fork 写隔离 | spec/proofs.md（R3：make_mut 文本同步为范围声明） | fork_same_fd_write + branch_isolation.rs（4 测试）+ adversarial_r3a.rs（Fork 内 Scope cwd 恢复/双分支 undo 合并/8 文件 LIFO 撤销栈压力） | ⚠️ 部分→**有效（附范围声明）**（R3 升级：语义层证据充分；物理层 make_mut 阶段 3 推迟有完整裁决） |
| P4 撤销双态 | w;w̄ 状态恢复 | spec/proofs.md（R3：trackΓ+Full 限定；迭代 1：RFC-08/09 主场景已修复） | exec_A6 + e2e undo + adversarial_r1.rs + r3a.rs（catch 组合/撤销栈压力）+ r1.rs:lin_stale_fd_write_after_replace_fails（RFC-05 闭合）+ r2.rs:time_timeout_parallel_fork_orphan_effects_unrecoverable 反转（孤儿响应取消）+ executor.rs:mutex_claim_released_on_timeout_cancel（锁即释放） | ⚠️ 部分→**有效（附范围声明）**（迭代 2-R7 形式审计升级：RFC-05 句柄活性反例闭环 + 取消传播（undo 回滚/线性快照/锁释放）证据齐备；范围声明 = trackΓ+Full + 宽限耗尽残余 R7-A/B + 不可逆操作 + BestEffort/Skip） |
| P5 无死锁 | 调度器无环 | spec/proofs.md + tla/scheduler.tla | arbiter + concurrent_arbiter_claims + arbiter_mutex.rs + adversarial_r3c.rs 风暴 | ⚠️ 部分→**有效（附范围声明）**（R3 升级：三层+风暴证据；边界=RFC-06/07/08/09+闭包盲区+用户隐瞒依赖（§18）） |

## 更新规则（源文件为准）

- 每轮对抗审计合并后：将新增 E2E 测试填入「对抗 E2E 补充」列。
- 每轮形式逻辑审计后：将证明有效性结论填入「审计结论」列（含证明链缺口）。
- 5 轮结束时：每条义务必须为「已履行」或列出明确差距（缺证明 / 缺测试 / 缺审计）。

> 登记表更新规则与轮次日志的权威版本见 `spec/proof-obligations.md`；本页仅作 mdBook 阅读入口。
