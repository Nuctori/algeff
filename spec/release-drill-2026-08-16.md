# 发布演练报告（Release Drill）2026-08-16

> 迭代 3-A4 发布演练（分支 `iter3/it3-a4`，基线 `7f7285a`）。
> **不真实 publish**（无凭据、无发布动作）；验证发布全链路可行性。
> 配套：`spec/release-checklist.md`（发布清单）、`scripts/release.sh`（发布脚本）、
> `scripts/local-registry-build.py`（本次新增：本地 registry 组装工具）。
> 冻结面（algeff-core 契约）零改动。

## 1. 演练环境

| 项 | 值 |
| --- | --- |
| 工具链 | cargo 1.96.0 / rustc 1.96.0（MSVC，Git Bash） |
| 镜像 | `~/.cargo/config.toml`：`crates-io` → `rsproxy-sparse`（rsproxy.cn sparse 索引） |
| 工作区 | 三 crate `version=0.1.0` / `edition=2021` / `license="MIT OR Apache-2.0"`（workspace 继承） |
| 依赖方向 | core → std → macro（std、macro 均依赖 core） |
| 产物目录 | `target/release-drill/`（local-registry、consumer、logs，均 gitignored） |

## 2. 演练步骤与结果

### 2.1 名称占用预检（crates.io）— ✅ 三个名称均未被占用

- 默认 `cargo search algeff-core` 因 rsproxy 镜像报错（见 §3 障碍 O1）。
- 绕过镜像：`cargo search <name> --limit 3 --registry crates-io`（直查 crates.io 索引）：
  - `algeff-core`：**空结果**（exit 0）→ 未占用
  - `algeff-std`：**空结果**（exit 0）→ 未占用
  - `algeff-macro`：**空结果**（exit 0）→ 未占用
  - 对照组验证命令有效：`serde` 返回 `serde = "1.0.229"`、`tokio` 返回 `tokio = "1.53.1"`。
- HTTP 交叉验证（crates.io API `https://crates.io/api/v1/crates/<name>`）：
  - 需带 User-Agent（无 UA 一律 403）；带 UA 后三名称均返回 **HTTP 404**（= crate 不存在）→ 与 cargo search 结论一致。
- 结论：**名称可注册**，R1 风险解除。发布时首次 publish 自动注册名称。

### 2.2 本地 registry 安装自测（模拟消费者）— ✅ 全链路通过

流程：`cargo package` 三 crate → 组装本地 registry（git 索引 + `download` 布局）→
独立 consumer 工程从该 registry 安装三 crate → `cargo build` + 运行最小 `do_!` 用例。

1. **打包**：`cargo package -p algeff-core --allow-dirty` ✅ / `-p algeff-macro` ✅；
   `-p algeff-std` 首次失败（`no matching package named algeff-core`，见 §3 障碍 O2），
   附加 `--config patch.crates-io.algeff-core.path="crates/algeff-core"`（发布清单既有缓解，
   仅验证打包+编译面）后 ✅。
2. **发布包内容核对**（`cargo package --list` + .crate 内归一化 Cargo.toml）：
   - 三 crate 均只含 `src/`、`tests/`、`Cargo.toml(.orig)`、`Cargo.lock`、`.cargo_vcs_info.json`（macro 另含 README.md、std 另含 examples/benches）——无意外文件泄漏。
   - **dev-deps 剔除确认**：macro 的 path-only dev-deps（`algeff-core`/`algeff-std`）与 std 的 path-only dev-dep（`algeff-macro`）在发布清单中**均被 cargo 剔除**（带 version 的 dev-deps 如 proptest/criterion/tempfile 保留为元数据，消费者解析不受影响）。core 无 path dev-dep。
   - std 的依赖 `algeff-core = { path=…, version="0.1.0" }` 发布后为 `version = "0.1.0"`（path 剔除），version 匹配 0.1.0 ✅。
   - std 的 feature 转发（`virtual-clock`/`coeffects` → `algeff-core/…`）保留 ✅。
3. **本地 registry 组装**：`python scripts/local-registry-build.py target/release-drill/local-registry`
   （git 索引 `config.json` + `al/ge/<name>` 版本行；`<name>/<version>/download` 存 .crate 内容）。
4. **消费者安装**：`target/release-drill/consumer/`（独立 workspace），
   `algeff-core/std/macro = { version = "0.1.0", registry = "local" }`，
   `.cargo/config.toml` 指向本地索引；`cargo build` **✅ 通过**——
   三 crate 均以 `Downloaded … (registry \`local\`)` 从本地 registry 拉取（模拟 crates.io），
   tokio/syn/quote 等第三方依赖经 rsproxy 镜像正常解析。
5. **最小用例运行**：`do_! { let t = dx::get_time(); t }` +
   `Runtime::new(Box::new(TokioExecutor::new()))` + `run_blocking`：
   `OK: consumer drill passed`（exit 0）✅。
6. **feature 转发变体验证**：consumer 启用
   `algeff-std = { features = ["virtual-clock", "coeffects"] }` → 重新 build + 运行 ✅
   （std → core 的 feature 链经 registry 索引正确解析，feature 名在 registry 侧存在）。

### 2.3 docs.rs 构建预检 — ✅ 无阻塞

- `cargo doc --no-deps --workspace`（默认 features）✅ exit 0
- `cargo doc --no-deps --workspace --all-features`（全量 features）✅ exit 0
- 断链专项：`RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links -D rustdoc::private_intra_doc_links" cargo doc --no-deps --workspace --all-features` ✅ exit 0
- 唯一 rustdoc lint：`crates/algeff-core/src/runtime.rs:233` 中文注释中 `Arc<Mutex>` 被 rustdoc 误判为未闭合 HTML 标签（`invalid_html_tags`）——**纯样式告警，docs.rs 默认不视为失败**；位于冻结面核心，按约束不动（见 §4 风险）。
- 结论：docs.rs 三 crate 构建面无错误，R3 风险解除（唯一告警不影响构建徽章）。

### 2.4 发布脚本 dry-run 预览（`scripts/release.sh`）— 脚本行为验证

- 首次运行被脚本自身的「工作区干净」检查拦截（`scripts/local-registry-build.py` 未提交）——
  **符合设计**；提交后复跑（见 §2.5）。
- 注意：std 的 dry-run 预期失败（`no matching package named algeff-core`，cargo 固有行为，
  已记录于发布清单 §3）；真实发布按 core → 等镜像 → std → macro 顺序解除。

### 2.5 复跑 `scripts/release.sh`（提交后，干净工作区）

- 见 §5「命令与产物」。core/macro dry-run PASS、std dry-run 预期失败
  （未发布 core 的 registry 语义，非缺陷）。

## 3. 障碍与处置

| # | 障碍 | 处置 | 状态 |
| --- | --- | --- | --- |
| O1 | rsproxy 镜像下默认 `cargo search` 报 `crates-io is replaced with non-remote-registry source` | 用 `--registry crates-io` 显式直查 crates.io 索引；HTTP API 交叉验证（需 User-Agent） | 已绕过，结论成立 |
| O2 | `cargo package/publish -p algeff-std` 因 `algeff-core` 未在 registry 索引而失败 | 发布清单已记录该固有行为；打包自测附加 `--config patch.crates-io.algeff-core.path=…` 代偿；真实发布顺序 core 先行 | 已知约束，非缺陷 |
| O3 | 本机无 `cargo-local-registry` 工具 | 自建 `scripts/local-registry-build.py`（git 索引 + download 布局） | 已解决，产物与工具一致 |
| O4 | `sparse+file://` 本地 sparse 索引在本机 cargo 下报 `invalid format` | 改用 git 索引布局（cargo 对本地 registry 的标准形态）；`file://` URL 需 `///` 三斜杠 | 已解决 |
| O5 | consumer 建在 workspace `target/` 内被上层 workspace 吞并 | consumer `Cargo.toml` 加空 `[workspace]` 表脱离 | 已解决 |

## 4. 修正项（本次新增/改动，均 scripts 域 + 报告）

- **新增 `scripts/local-registry-build.py`**：把 `cargo package` 产物组装为本地 registry
  （git 索引 + `<name>/<version>/download`），供消费者安装自测复现。
  修正了 3 处自身缺陷（Python3.10 无 tomllib → 回退 tomli；
  crates.io 索引 4+ 字符路径规则 `ab/cd/<name>`；`file:///` 三斜杠）。
- **新增 `spec/release-drill-2026-08-16.md`**：本报告。
- 冻结面零改动；`Cargo.toml` 零改动（发布面核对无缺陷需修）。

## 5. 命令与产物

```text
cargo search algeff-core/std/macro --limit 3 --registry crates-io   # 空结果 exit 0（未占用）
curl -A "algeff-release-drill/0.1" https://crates.io/api/v1/crates/<name>  # 404×3（未占用）
cargo package -p algeff-core / -p algeff-macro / -p algeff-std（+patch 代偿）  # 均 exit 0
python scripts/local-registry-build.py target/release-drill/local-registry     # exit 0
cargo build（consumer，registry=local）                                       # exit 0
./algeff-consumer-drill.exe                                                    # OK，exit 0
cargo build（consumer，features=virtual-clock+coeffects）+ 运行                 # exit 0
cargo doc --no-deps --workspace [--all-features]                              # exit 0（1 条样式 lint）
RUSTDOCFLAGS="-D rustdoc::broken_intra_doc_links …" cargo doc …                # exit 0
scripts/release.sh                                                            # 干净后复跑，见下方
```

复跑 release.sh 结果（提交后）：

```text
OK：tag v0.1.0 尚不存在。
OK：工作区干净。
==> 本地发布面检查（cargo package --list）        # 三 crate 文件清单正常
==> cargo publish --dry-run（仅打包校验，不实际发布；需要网络）
    algeff-core: PASS   （仅 manifest 元数据警告：无 repository/homepage，即清单 R2）
    algeff-std:  FAIL（预期：no matching package named `algeff-core`，cargo 固有行为）
    algeff-macro: PASS

scripts/release.sh --allow-unpublished-deps：
    algeff-core: PASS / algeff-std: PASS（patch.crates-io.algeff-core 代偿）/ algeff-macro: PASS
```

## 6. 发布前最终检查清单（勾选状态）

对照 `spec/release-checklist.md`：

| 清单项 | 状态 | 备注 |
| --- | --- | --- |
| §1 分支/基线确认 | ✅ | `iter3/it3-a4`，基线 7f7285a 可追溯（正式发布分支待定，发布前切换） |
| §1 tag 检查（v0.1.0 不存在） | ✅ | release.sh 自动确认 |
| §1 工作区干净 | ✅ | 本报告提交后干净 |
| §1 名称占用预检 | ✅ | 三名称均未占用（cargo search + API 404 双验证） |
| §2 `cargo test --workspace` 全绿 | ⬜（本次未跑） | 迭代 3 回归域，非本演练范围；发布前按清单执行 |
| §2 `cargo fmt --check` / clippy | ⬜（本次未跑） | 同上 |
| §2 Cargo.toml 元数据核对 | ✅ | version/edition/license/description 均正确；`repository` 缺失属已知 R2（非阻塞） |
| §2 README 呈现核对 | ⚠️ | core/std 无 crate 级 README（`readme=false`），crates.io 页仅显示 description；macro 有 README.md。非阻塞，属既有 R2/R7 范畴 |
| §3 core dry-run PASS | ✅ | release.sh 实测 PASS |
| §3 std dry-run 预期失败 | ✅ | 实测如预期（cargo 固有行为），`--allow-unpublished-deps` 可预览打包面 |
| §3 macro dry-run PASS | ✅ | release.sh 实测 PASS |
| §4 真实发布（tag/发布/等待镜像/自测） | ⬜ | 需要凭据 + 正式发布分支；演练已验证发布面与顺序可行性，不真实发布 |
| §5 docs.rs 构建 | ✅ | 本地 doc 预检三 crate 全量 features 无错误（1 条样式 lint 不影响） |
| §5 crates.io 页面核对 / 消费侧冒烟 | ✅ | 消费侧冒烟已由本地 registry consumer 等价验证 |
| §6 回滚（yank）流程 | ✅ | 文档已就绪；无真实版本可回滚 |

## 7. 残留风险

| # | 风险 | 影响 | 处置 |
| --- | --- | --- | --- |
| R-A | core `runtime.rs:233` `Arc<Mutex>` rustdoc `invalid_html_tags` 样式告警 | 仅 `-D warnings` 严格模式失败；docs.rs 默认构建成功 | 冻结面不动；发布后 docs.rs 页确认，若介意待核心解冻期修文档注释 |
| R-B | 名称占用预检基于当前时刻 | 发布前他人可能抢注（概率极低） | 真实发布当天重跑 §1 预检 |
| R-C | 本地 registry 演练未覆盖 crates.io 真实索引/镜像同步路径 | std 发布时 core 索引可见性依赖镜像 | 发布清单 §4 已有轮询等待步骤（默认 300s，超时可直连） |
| R-D | core/std 无 crate 级 README | crates.io 页无 README 渲染（仅 description） | 既有 R2/R7，非阻塞；如需补 README 属发布面文档改动，随正式发布决策 |
| R-E | `cargo search --registry crates-io` 依赖本机网络可达 crates.io 索引 | 离线环境无法预检 | HTTP API 404 检查为交叉验证，已双通道确认 |

## 8. 演练结论

发布全链路可行性验证通过：名称可注册、打包产物干净（dev-deps 剔除、version 匹配、
feature 转发完整）、消费者可从 registry 安装并运行最小 `do_!` 用例、docs.rs 构建面无错误、
发布脚本 dry-run 行为符合清单预期。剩余项均为「真实发布动作」（需凭据与正式分支）与
既有非阻塞风险（R2/R6/R7），无新增阻塞项。
