# 语义撤销缺口清单（Undo Semantics Gaps）

> 状态：**已修复（P0+P1+A4 use/move 拆分）**。剩余：P3 确定性维度的完整落地（is_deterministic 已加）、chmod/chown 快照逆。P2 宏级 compile warning 因 stable Rust proc macro 无 Diagnostic API **不可行**——已由运行时 Replace 闸门（NonInvertible 标记）覆盖 + dx::irreversible 文档标记。
> 关联决策：D-098（语义真回归原则）、D-099（修复分层）、D-100（测试先行）、D-101（线性语义分层）。
> 关联测试：`crates/algeff-std/tests/undo_semantics_contract.rs`（4 个锁定测试，当前 4/4 绿 = 问题行为基线）。
> 记录日期：2026-08-17。修复时逐条勾销并反转对应测试断言。

## 一、数学模型（物理现实的代数化）

每个 IO 操作 w 是状态变换幺半群 M 的元素，按三个正交属性分类：

1. **可逆性（逆元存在性）**：单位元（w = id_M，无副作用）| 可逆（∃w̄, w;w̄ = 1，可为部分逆）| 不可逆（无逆元）
2. **交换性（换序安全）**：非阿贝尔 → 撤销必须 LIFO（(w₁;w₂)⁻¹ = w₂⁻¹;w₁⁻¹）；D14 冲突矩阵即交换性判定
3. **确定性（可重放）**：唯一结果可重放 | 不确定（墙钟时间、网络投递、外部进程）不可重放

A6 只要求单边逆（w;w̄ = 1，不要求 w̄;w = 1）→ 撤销是一次性（Replace 后清空栈），这是数学事实。

## 二、36 个 DataOp 的物理现实分类

### 文件

| 操作 | 数学角色 | 逆 | 现状 | 缺口 |
| --- | --- | --- | --- | --- |
| Open(无 create) | 部分逆 | 逆 = Close（生命周期） | None（Arc Drop 兜底） | 可接受，应显式标记 |
| Open(create) | 部分逆 | 逆 = Close + Unlink（文件原不存在时） | None | **假回归：Replace 后文件残留（测试 4）** |
| Open(truncate) | 可逆（需成本） | 逆 = 写回原内容 | None | **假回归：旧内容永不恢复** |
| Read(文件) | **完全可逆** | 逆 = Seek 回原位（内容不变，仅游标） | None | **游标无逆** |
| Write | 完全可逆 | 逆 = 写回原区域 + set_len + seek | Full / 降级 None | **降级无声（测试 1）** |
| Seek | **完全可逆** | 逆 = 反向 Seek | None | **游标无逆** |
| Close | 不可逆（终结） | 无 | None | 不可逆无声 |
| Stat / ReadDir | 单位元 ✓ | 无 | None | 正确 |
| Chmod | **可逆（成本≈1 次 fstat）** | 逆 = 恢复权限快照 | None | **可逆却未实现** |
| Chown | 可逆（成本低） | 逆 = 恢复属主快照 | None | 同 Chmod |
| Truncate | 可逆（<1MB） | 逆 = 写回原内容 | Full / None(≥1MB) | 降级无声 |
| Unlink | 不可逆（内容不缓存） | 无 | None | 不可逆无声 |
| Rename | 完全可逆 ✓ | 逆 = 反向 Rename | Full ✓ | 正确 |
| SendFile | 部分可逆 | 逆 = 恢复目标区域 | None（目标侧 BestEffort 注释） | **目标侧残留（同族假回归）** |

### 目录

| 操作 | 数学角色 | 逆 | 现状 | 缺口 |
| --- | --- | --- | --- | --- |
| Mkdir | 部分逆 | 逆 = RemoveDir（仅空目录有效） | 尽力 undo（吞错） | **部分逆执行失败吞错（测试 2）** |
| Rmdir | 不可逆 | 无 | None | 不可逆无声 |

### 网络 / 进程 / 内存 / 时间 / 同步

| 操作 | 数学角色 | 确定性 | 现状 + 缺口 |
| --- | --- | --- | --- |
| TcpBind / UdpBind / PipeOpen | 部分逆（逆 = Close） | 确定 | None 可接受 |
| TcpAccept / TcpConnect | 不可逆 | 确定 | 不可逆无声 |
| TcpRead | **不可逆（消费数据）** | 确定 | **有副作用却无声** |
| TcpWrite / TcpShutdown | 不可逆 | 确定 | 不可逆无声 |
| UdpRecvFrom / UdpSendTo | 不可逆 | **不确定**（网络） | 不可逆 + 不可重放，无声（pdr A6 补偿挂钩） |
| Spawn | 不可逆 | **不确定**（外部进程） | 不可逆 + 不可重放，无声 |
| Kill / SendSignal | 不可逆 | 确定 | 不可逆无声（pdr A6 补偿挂钩） |
| Wait | 不可逆（状态已消耗） | 确定 | 不可逆无声 |
| Mmap / Munmap | 可逆（COW 用户态）/ no-op | 确定 | None，语义可接受 |
| GetTime | 单位元 | **不确定**（墙钟） | 不可重放（virtual-clock feature 已修） |
| MutexLock / MutexUnlock | **完全可逆** | 确定 | **有 undo ✓ 正确** |
| Dup / Dup2 | 部分逆（逆 = Close 新 fd） | 确定 | 应显式标记 |

## 三、类型系统与数学解释的同构缺口

`Option<UndoOp>` 把 4 类数学角色混进一个 None：

| 数学角色 | 落入 None 的操作 | 类型层表现 |
| --- | --- | --- |
| 单位元（w = id） | read/stat/readdir/get_time | ✅ 正确 |
| 部分逆定义域外（写前读失败） | write-only 写 | ❌ 吞掉 = 把"逆不存在"伪装成"无事发生" |
| 完全可逆但未实现/成本高 | chmod/chown/游标(seek/read)/truncate-open/sendfile-target/大文件 | ❌ 把"可逆"伪装成"不可逆" |
| 本质不可逆 | unlink/rmdir/close/tcp/udp/kill/spawn/wait/signal | ❌ 把"不可逆"伪装成"正常" |

三个具体证据：

1. **游标双标**：write 的 undo 恢复游标（executor.rs:594，A6 测试锁定），但 seek/read 自己的游标移动返回 None——同一物理量两套待遇
2. **可逆未实现**：chmod 的逆 = 恢复权限快照（成本≈0），却标注"补偿挂钩由用户提供"（executor.rs:703）
3. **确定性维度缺失**：类型层无法区分可重放（文件 IO）与不可重放（udp/time/spawn），重放性承诺无类型支撑

## 四、修复优先级（与 D-099 分层对齐）

- **P0（一期·撤销链路真回归）**：`UndoCapability` 类型三分（Identity/Invertible/NonInvertible）+ 部分逆定义域 Err（写前读失败） + recover 检查 undo 结果 + Replace 闸门（含 NonInvertible 标记 → Err）。验收 = 测试 1/2/4 断言反转。
- **P1（补物理可逆）**：游标(Seek/Read) + Chmod/Chown 快照 + Open(truncate/create) + SendFile 目标侧。
- **P2（不可逆显式化）**：DataOp 静态 role 标注 + do_! 宏编译期 warning + `dx::irreversible` 显式包装。
- **P3（确定性维度）**：DataOp `deterministic` 静态位 + 重放性类型化（与二期 A4 use/move 拆分同批）。

## 五、与测试的对应

| 锁定测试 | 缺口 |
| --- | --- |
| `write_only_fd_write_silently_drops_undo_then_replace_fake_rolls_back` | Write 降级无声（部分逆定义域） |
| `mkdir_undo_failure_swallowed_replace_reports_success` | Mkdir 部分逆执行失败吞错 |
| `sequential_multi_write_same_fd_rejected_by_a4` | A4 过度拒绝（二期，不列入一期验收） |
| `create_open_undo_missing_file_left_after_replace` | Open(create) 无逆（Replace 后残留） |
