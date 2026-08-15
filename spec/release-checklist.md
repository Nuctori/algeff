# Algeff 0.1.0 发布清单（Release Checklist）

> 面向：发布工程师。配套脚本：`scripts/release.sh`（预览默认，`--publish` 真实发布）。
> 本清单与脚本互为镜像：脚本负责机械步骤，本清单负责**人工核对点、验证点、回滚与已知风险**。
> 状态约定：每步执行后勾选；任何一步失败即中止，修复后从失败步骤重跑（发布面只允许文档/元数据修复）。

## 0. 前置事实

- 三个 crate：`algeff-core`（契约冻结核心，永久冻结）、`algeff-std`（tokio 物理后端）、`algeff-macro`（可选语法糖宏）。
- 依赖方向：**core → std → macro**（std、macro 均依赖 core；macro 发布清单中 dev-dep 的 path 依赖会被 cargo 剔除，消费者无感）。
- 版本 0.1.0；工作区统一 `version/edition/license`（`license = "MIT OR Apache-2.0"`），三 crate `*.workspace = true` 继承。
- 发布凭证：`cargo login`（crates.io API token），发布者需有 crate 所有权（首次发布需注册 crate 名）。
- 本仓库当前无 git remote / 无公开仓库 URL → 三 crate **未填 `repository`/`homepage` 字段**（缺省不阻塞发布，见 §5 风险 R2）。

## 1. 发布前检查（脚本步骤 1–3）

- [ ] 分支/基线确认：`git branch --show-current`（应为 `iter1/it1-rel` 或等价发布分支），基线可追溯。
- [ ] tag 检查：`git rev-parse -q --verify refs/tags/v0.1.0` 无输出（脚本自动做）。
- [ ] 工作区干净：`git status --porcelain` 为空（脚本自动做）。
- [ ] 名称占用预检（crates.io 名称不可改名、被占即无法发布）：
  ```bash
  cargo search algeff-core --limit 3   # 期望无同名 crate；若有同名异主，需 cargo owner 流程或改名
  cargo search algeff-std --limit 3
  cargo search algeff-macro --limit 3
  ```
  （首次发布时 crates.io 自动注册名称；同名前缀不冲突，需**精确同名**才冲突。）

## 2. 本地验证（提交前/发布前）

- [ ] `cargo test --workspace` 全绿（300+ 测试，46 个测试二进制）。
- [ ] `cargo fmt --check` 通过。
- [ ] `cargo clippy --workspace -- -D warnings` 通过（可选，CI 已覆盖）。
- [ ] 三个 Cargo.toml 元数据核对：
  - `version = 0.1.0`（workspace 继承）、`edition = 2021`、`description` 非空、`license = "MIT OR Apache-2.0"`（workspace 继承，三 crate 一致）。
  - `repository`/`homepage`：当前未填（无公开 URL），发布时会有 manifest 警告，非阻塞；见 R2。
- [ ] README 呈现核对（crates.io 直接渲染仓库 `README.md`，无独立 crates.io 版）：
  - 顶部 ⚠️ 实验性声明醒目（研究级项目、不提供稳定性承诺、API 可能变化）——已确认在标题下首段。
  - 快速上手依赖示例为 path 依赖（仓库内自洽），发布后用户应以 crates.io 版本号为准。

## 3. dry-run 预检（脚本步骤 4）

- [ ] `cargo publish -p algeff-core --dry-run --registry crates-io` → PASS（期望仅 manifest 元数据警告）。
- [ ] `cargo publish -p algeff-std --dry-run --registry crates-io` → **预期失败**（`no matching package named algeff-core`，cargo 固有行为：core 未发布时 registry 无法解析带 version 的 path 依赖）。
- [ ] std 打包面预检（不阻塞发布流程，仅预览）：
  ```bash
  scripts/release.sh --allow-unpublished-deps   # 对 std 附加 patch.crates-io.algeff-core.path 代偿，仅验证打包+编译面
  ```
- [ ] `cargo publish -p algeff-macro --dry-run --registry crates-io` → PASS。
- [ ] 已发布版本的**版本号与 tag 不可复用**：发布后若发现缺陷，只能 bump 版本（0.1.1…），不能删除/覆盖 0.1.0。

## 4. 真实发布（脚本 `--publish`）

> 需要网络 + `cargo login` 凭据。顺序由依赖方向强制：**core 必须先于 std**。

- [ ] `git tag v0.1.0 && git push origin v0.1.0`（先打 tag 再发布，保证可复现基线）。
- [ ] 发布 core：`cargo publish -p algeff-core --registry crates-io`（**契约冻结面，发布前必须经 A1 冻结确认**）。
- [ ] 等待镜像同步：core 发布与索引可见是异步的（rsproxy 等镜像通常数分钟）。脚本自动轮询 `cargo search algeff-core` 直到 `algeff-core = "0.1.0"` 可见（`ALGEFF_WAIT_SECS` 可调，默认 300s）。
  - 若超时：镜像同步慢或网络问题；可临时移除 `~/.cargo/config.toml` 的 `[source.crates-io] replace-with` 直连 crates.io 索引后重试。
- [ ] 自测 core：独立临时工程 `cargo add algeff-core@0.1.0 && cargo check`（脚本自动）。
- [ ] 发布 std：`cargo publish -p algeff-std --registry crates-io`；自测同 core。
- [ ] 发布 macro：`cargo publish -p algeff-macro --registry crates-io`；自测同 core。

## 5. 发布后验证点

- [ ] docs.rs 构建：`https://docs.rs/algeff-core`（std / macro 同理）——首次触发后需等待构建（通常数分钟），失败会以红色徽章展示且**重发同版本无法重触发**，只能 bump 版本。
- [ ] crates.io 页面核对：description、license（MIT OR Apache-2.0）、README 实验性声明渲染正常。
- [ ] 消费侧冒烟：任意新工程 `cargo add algeff-std@0.1.0`（连带 core）+ 最小 `Runtime::new` 用例编译运行。

## 6. 回滚（crates.io 版本不可变）

- 已发布版本**不可删除、不可覆盖**。唯一手段是 **yank**（标记不可用于新依赖；已依赖该版本的项目不受影响）：
  ```bash
  cargo yank --version 0.1.0 algeff-core
  cargo yank --version 0.1.0 algeff-std
  cargo yank --version 0.1.0 algeff-macro
  ```
- 修复路径：yank 后 bump 到 0.1.1（或按 semver 规则），重新走本清单；**yank 不可撤销恢复**（`--undo` 只能取消 yank 标记本身，不能把版本变回"从未发布"）。
- 部分发布失败（如 core 成功、std 失败）：core 已发布即生效，修复 std 问题后**仅重跑 std → macro**，不要重复发布 core 同版本。

## 7. 已知风险

| # | 风险 | 影响 | 缓解 |
| --- | --- | --- | --- |
| R1 | crates.io 名称占用 | `algeff-*` 已被他人注册则无法发布（名称随首次发布锁定，不可改名） | §1 名称占用预检；最坏情况需改名并同步更新依赖/文档 |
| R2 | `repository`/`homepage` 缺失 | 发布仅产生 manifest 警告（无 `license` 警告，license 已继承）；crates.io 页面无仓库链接 | 非阻塞；公开仓库建立后补 `repository.workspace = true`（补 `[workspace.package] repository`）+ 三 crate 继承，随下次版本发布 |
| R3 | docs.rs 构建失败 | 徽章红色；同版本无法重触发构建 | 发布前本地 `cargo doc` 预检 + 依赖均已在 crates.io（tokio/proc-macro2/quote/syn 长期稳定） |
| R4 | 版本不可变 / 发布即不可撤销 | 0.1.0 一旦发布，任何缺陷只能靠 0.1.1 修复 | §3「版本号不可复用」+ §6 yank 流程；核心 crate 发布前强制 A1 冻结确认 |
| R5 | 镜像同步延迟 | std 发布时 core 索引不可见 → `no matching package named algeff-core` | §4 等待镜像步骤（脚本自动轮询，默认 300s） |
| R6 | 无 LICENSE 文本文件 | 仓库内无 `LICENSE-MIT`/`LICENSE-APACHE`，`license` 字段声明与文本缺失不一致（法律合规风险，不阻塞 crates.io 发布） | 发布前补齐许可证文本文件（需仓库所有者决策，超出本清单执行范围） |
| R7 | README 引用 `scripts/release.sh` 路径 | crates.io 渲染 README 时 `spec/`、`scripts/` 相对链接指向仓库文件，在 crates.io 页面可能 404 | 发布后核对 crates.io 页面链接；README 文档入口表为仓库视角，接受现状 |

## 8. 完成定义

- [ ] 三 crate 全部真实发布成功且 `cargo add` 自测通过。
- [ ] docs.rs 三 crate 构建成功（绿色）。
- [ ] 本清单 §1–§5 全部勾选。
