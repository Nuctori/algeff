# FAQ

> 常见问题速查。细节与权威来源见文末链接。

## 1. 深度守卫 64 是什么？我的蓝图会触发吗？

解释器递归处理嵌套蓝图，为防栈溢出设置**深度上限 64**（2MB 线程栈下的安全阈值，迭代 1 由 96 下调）。触发时返回 `Err(SysError::Other(105))`（「嵌套资源耗尽」语义）。

- **会触发**：左结合链（如 `adapters::seq` 用 `then` 逐层嵌套）**≥ 65 步**；
- **不会触发**：右结合 CPS（`and_then` 风格）恒为深度 1，无限制——**`do_!` 正是这种**，写多少条语句都安全；
- 错误可被 `Catch` 捕获。

## 2. undo 语义是什么？undo 栈怎么工作？

运行时对每个已执行操作维护一张**跟踪表 Γ**（`rt.undo_stack()` 可查看）：

- 写操作执行前记录「写前状态」（小文件 = 完整内容快照；大文件 = 长度/元数据策略）；
- `Replace` / `Scope` 退出触发 **recoverΓ**：按 LIFO 顺序把资源恢复到执行前状态；
- 恢复**幂等且可嵌套**（作用域嵌套时内层先恢复，外层接着恢复）；
- **职责边界**：`dx::catch`（`Action::Catch`）只处理错误值、不触碰撤销栈；需要整体回滚时用 `Action::Replace` 包住。

## 3. 锁为什么可以重入（lock → unlock → 再 lock）？

`MutexLock` / `MutexUnlock` 推导为**空声明**：锁 id 的互斥语义由 executor 层 **arbiter** 动态仲裁（占坑 ⟺ 持锁），与 A4 线性域（每资源至多消费一次）正交。若声明 `Write(Fd(id))`，lock→unlock→再 lock 第二次必被 A4 拒（R7 发现的回归）——空声明后同 id 并行争用仍由 arbiter 序列化（败者 `WouldBlock`），顺序重入成功。

## 4. TcpShutdown 为什么声明空集而不是 Own？

`TcpShutdown` 是**半关闭**（`Shutdown::Both` 也只是关闭发送/接收方向），不终结 fd，无 A4 消费语义：

- 声明 `Write(fd)` 会与 `tcp_write` 消费冲突 → 误拒 write→shutdown 合法链；
- 声明 `Own(fd)` 是终结语义 → 拒绝后续 close（shutdown→close 标准链被误拒，R7 审计）。

空声明 + 物理层执行，`shutdown` 后仍可 `close`。需要显式声明时用 `dx::syscall_with` 覆盖。

## 5. 1MB 主线程栈会怎样？怎么处理？

深嵌套蓝图（左结合链，见 FAQ 1）在 1MB 主线程栈下可能栈溢出。处理方式：

- 抬栈：`/STACK` 链接参数（Windows）或 `RUST_MIN_STACK` 环境变量；
- 或把执行放到 `spawn` 线程（受 `RUST_MIN_STACK` 控制）。

`do_!` 右结合链不受此影响（恒为深度 1）。

## 参考

- 深度守卫 / undo / Replace：README「深入：确定性、重放与撤销机制」与 [架构](architecture.md)
- 锁重入 / TcpShutdown 空声明：`docs/src/dx-design.md` §3 推导表（含 R7 修复论证）
- 栈问题完整分析：`spec/resource-notes.md` §10（RFC-11）
