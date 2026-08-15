# 参与开发（Contributing）

本文档面向所有在 Algeff 仓库工作的贡献者（含 8 Agent 并行开发），说明契约边界、工作流、提交规范与验证要求。权威源：`pdr.md`（设计规范 v3.2）、`contracts.md`（阶段 0 冻结契约）。

## 1. 契约冻结规则

`contracts.md` 是 8 Agent 并行开发的**唯一接口事实来源**，已进入冻结状态（阶段 0 完成）。规则：

- **文件所有权表（contracts.md §1）**：每个路径都有唯一拥有者。只允许增删改自己目录下的文件；越界修改视为违规。
- **冻结文件**：`contracts.md`、`pdr.md`、`Cargo.toml`、`crates/algeff-core/src/{action,error,syscall,lib}.rs`、`crates/algeff-std/src/lib.rs` 等标注「冻结」的路径，**一律不许改**。契约变更必须走 RFC → CTO 裁决。
- **Cargo.toml 一律不许改**：缺依赖 → 报告 CTO，禁止自行添加。
- **基线测试**：`cargo test --workspace` 必须保持绿色（`todo!()` 允许，但不得让别的 crate 编译失败）。
- **跨 crate 需求**：先自行绕开或本地实现，报告中写明「RFC 建议」，CTO 批量裁决，禁止等待。

## 2. worktree / 分支约定

每个 Agent 一个独立的 git worktree：

```bash
# 在仓库根目录创建 worktree（.wt/aN，分支 sN/aN，N 为 Agent 编号，sN 为阶段）
git worktree add .wt/a8 -b s2/a8
cd .wt/a8
```

- worktree 目录：`.wt/aN`（如 `.wt/a1`、`.wt/a8`）。
- 分支命名：`sN/aN`（阶段 N / Agent N），如 `s1/a2`、`s2/a8`。
- 每个 Agent 只提交自己目录下的文件（见 contracts.md §1 所有权表），`git add` 时按文件白名单操作。
- 完成后：`cargo test --workspace` 全绿 → `git add`（仅自己文件）→ `git commit` → 报告 CTO。
- CTO 合并 → 全量验证 → 审计 → 下一批；冲突由 CTO 裁决，**不跨分支互相等待**。
- 不在自己 worktree 内执行 `git push` 或切换分支（除非 CTO 明确指示）。

## 3. 提交信息规范

- 格式：`sN/aN: 摘要`（阶段号/Agent 号 + 冒号 + 一句话摘要）。
- 示例：`s2/a8: 反馈循环模板 + CONTRIBUTING + getting-started 文档`、`s1/a2: 解释器 trampoline 实现`。
- 摘要用中文，简洁描述**做了什么**（动词开头），不写流水账。
- 合并提交由 CTO 负责（如 `merge: s1/a8 DevOps (CI+docs+release)`），普通 Agent 不推送合并。

## 4. 验证要求（提交前必须通过）

| 检查项 | 命令 | 通过标准 |
| --- | --- | --- |
| 工作区测试 | `cargo test --workspace` | 全绿（exit 0） |
| CI YAML 合法 | `python scripts/check-yaml.py .github/workflows/ci.yml` | 输出 `yaml OK` |
| 文档构建 | `mdbook build docs`（或 CI 中 Build mdBook docs 步骤） | 无错误 |
| 格式（可选建议） | `cargo fmt --check` | 无 diff（仅 Rust 改动时） |
| 静态检查（可选建议） | `cargo clippy --workspace -- -D warnings` | 无 error（仅 Rust 改动时） |

> 修改 Rust 代码（crates/**）后必须跑 `cargo test --workspace`；修改 `.github/workflows/*.yml` 后必须跑 `python scripts/check-yaml.py`；修改 `docs/**` 后建议 `mdbook build docs` 验证。

## 5. 反馈循环（pdr.md §19.2）

测试失败或性能不达标 → 自动创建 Issue（`.github/ISSUE_TEMPLATE/bug_report.md`）→ 对应 Agent 修复：

- **core**（crates/algeff-core）→ A2 / A3
- **std**（crates/algeff-std）→ A5
- **macro**（crates/algeff-macro）→ A4
- **ci**（.github/、scripts/、docs/）→ A8

修复后更新 Issue 状态并说明验证方式；契约级变更走 RFC（feature_request 模板 + CTO 裁决）。

## 6. 文档约定

- 文档统一用 mdBook（`docs/book.toml`），新增页面后记得更新 `docs/src/SUMMARY.md`。
- 文档是对 `pdr.md` / `contracts.md` 的**转述与导航**，权威源始终以 `pdr.md` / `contracts.md` 为准，涉及规范表述时注明章节出处。
