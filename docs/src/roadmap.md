# 路线图

> 本页是 `pdr.md`（v3.2）§19「8 Agent 并行 Loop 闭环开发实现路线」的摘要。权威源以 `pdr.md` 为准。

## 8 Agent 角色（§19.1）

| Agent ID | 名称 | 职责 | 交付物 |
| --- | --- | --- | --- |
| A1 | Spec Guardian | 维护形式化规范，审计一致性，更新公理证明 | `spec/`、`proofs/` |
| A2 | Core Runtime | 实现 Action 解释器、Context、UndoStack | `algeff-core/runtime.rs` |
| A3 | Resource & Coeffects | 实现 ResourceSet、冲突检测、依赖表 notify（可选） | `algeff-core/resource.rs` |
| A4 | DSL & Macro | 实现可选语法糖宏 | `algeff-macro/` |
| A5 | Std Adapters | 实现数据平面原语（文件、网络、内存等） | `algeff-std/` |
| A6 | Verification | 属性测试、模型检测、形式化证明检查 | `tests/`、`tla/` |
| A7 | Integration & Perf | 跨平台后端、基准测试、性能回归 | `benches/`、`ci/perf` |
| A8 | DevOps & CI | 管理并行分支、自动合并、文档生成、发布 | `.github/workflows/` |

## 并行 Loop 闭环机制（§19.2）

1. **接口冻结**：A1 定义核心类型签名和公理，发布 `contracts.md`。
2. **并行开发**：A2–A5 在独立分支实现各自模块，通过 trait 契约交互。
3. **持续集成**：A8 配置 CI，每次推送自动运行 A6 测试套件和 A7 基准测试。
4. **反馈循环**：测试失败或性能不达标 → 自动创建 Issue → 对应 Agent 修复。
5. **每周同步**：A1 审查所有模块是否符合形式化公理；接口变更走 RFC 流程。
6. **闭环收敛**：公理全部被证明/测试覆盖、性能达标后合并主分支，冻结 API。

## 阶段划分（§19.3）

| 阶段 | 时长 | 主要任务 |
| --- | --- | --- |
| 阶段 0：契约冻结 | 1 周 | A1 输出核心类型与公理；A8 搭建 monorepo 和 CI |
| 阶段 1：核心实现 | 2 周 | A2–A5 并行实现各自模块；A6 编写第一批属性测试 |
| 阶段 2：集成与验证 | 2 周 | 合并代码；A6 运行 proptest/loom/TLA+；A1 修订公理 |
| 阶段 3：优化与跨平台 | 2 周 | A7 优化 tokio 后端；A3 优化锁竞争；A6 并发压力测试 |
| 阶段 4：发布与冻结 | 1 周 | A8 生成文档、发布 algeff-core 0.1.0；A1 冻结 spec |

## 工具链（§19.4）

| 用途 | 工具 |
| --- | --- |
| 属性测试 | proptest |
| 并发测试 | loom |
| 模型检测 | TLA+ / Apalache |
| 形式化证明 | Coq（可选） |
| 基准测试 | criterion |
| CI/CD | GitHub Actions |
| 代码审查 | reviewdog |
| 文档 | mdBook |

## 关键风险与应对（§19.5）

| 风险 | 应对 |
| --- | --- |
| 接口频繁变更导致返工 | 阶段 0 冻结契约，RFC 流程变更 |
| 性能不达标 | A7 提前介入，基准测试驱动优化 |
| 形式化证明过于耗时 | A6 以属性测试为主，TLA+ 只覆盖调度器 |
| Agent 之间沟通不畅 | A8 每周同步会议 + 共享设计文档 |

## 当前进展（阶段 1，契约已冻结）

契约（`contracts.md`）与基线测试已冻结，A2–A7 在各自分支并行实现中；本仓库（s1/a8 分支）负责 CI 工作流、mdBook 文档与发布脚本。
