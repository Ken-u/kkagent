# kkagent 与 ref/kimi-code 差距分析

> 分析日期：2026-08-09  
> 对比对象：当前仓库 `kkagent`（Rust） vs `ref/kimi-code`（Kimi Code CLI 原版，TypeScript）

## 摘要

当前 `kkagent` 是 Kimi Code CLI 的一个**小范围 Rust 复刻骨架**，核心逻辑可用但远未完整。原版 `ref/kimi-code` 是一个经过多年迭代、包含 **3564 个 TS/TSX 文件、约 84.6 万行代码** 的生产级 monorepo；当前 `kkagent` 仅有 **162 个 Rust 文件、约 4.1 万行代码**，规模相差约 **约 10 倍**。差距不仅在于功能数量，更在于 Agent 引擎完整度、CLI/IDE 集成、测试体系、文档与发布流程等工程基础设施。

---

## 1. 基础定位与技术栈

| 维度 | ref/kimi-code | 当前 kkagent |
|---|---|---|
| 语言/运行时 | TypeScript / Node.js + pnpm monorepo | Rust / Cargo workspace |
| 代码规模 | 3564 个 TS/TSX 文件，约 169 万行 | 162 个 `.rs` 文件，约 8.2 万行 |
| 提交历史 | 3061 个 commit | 79 个 commit |
| 版本 | CLI v0.34.0，多包 0.x–0.34 | 单一 workspace v0.1.1 |
| 目标 | 生产级多平台 Coding Agent | 实验性复刻骨架 |

---

## 2. 包/模块结构差距

ref 包含 **20 个 package + 1 个 CLI app + VSCode 扩展**；当前只覆盖了其中一小部分，且大量为简化实现。

| ref 包 | 当前对应 crate/目录 | 完成度 |
|---|---|---|
| `apps/kimi-code`（CLI 入口） | `crates/kkagent` | 命令少很多 |
| `packages/agent-core-v2` | `crates/kkagent-core` | 严重缩水 |
| `packages/pi-tui` | `crates/kkagent-tui` | 功能少很多 |
| `packages/protocol` / `klient` / `kosong` | `kkagent-protocol/rpc/wire/client` | 协议层较薄 |
| `packages/transcript` | `kkagent-core/src/transcript` | 仅 2 个文件 |
| `packages/kaos`（沙盒/文件系统） | `kkagent-tools` + `kkagent-core` | 缺少完整 kaos |
| `packages/acp-adapter` / `acp-server` | `crates/kkagent-acp` | 基本实现 |
| `packages/oauth` | `crates/kkagent-oauth` | 基本实现 |
| `packages/telemetry` | `crates/kkagent-telemetry` | 基本实现 |
| `packages/node-sdk` | `sdk/node` | 仅单个极简 `index.ts` |
| `apps/vscode` | `apps/vscode` | 3 个文件的占位扩展 |

当前完全缺失的 ref 包：

- `packages/agent`（高层 Agent 抽象）
- `packages/kap-server`
- `packages/minidb`

---

## 3. CLI 命令差距

ref CLI 注册的命令包括：`acp`、`doctor`、`export`、`login`、`provider`、`migrate`、`rotate-token`、`vis`、`web`、`auth`、`deprecated server`、`legacy-kill`、`native-acp` 等。

当前 `kkagent` 仅支持：`server`、`acp`、`auth`、`init`、`config`、`sessions`、`history`、`migrate`、`plan` 模式，以及默认 TUI/print 模式。

明显缺失：`doctor`、`export`、`login`、`provider`、`rotate-token`、`vis`、`web`。

---

## 4. Agent Core 差距最大

| 项目 | 文件数 | 代码量 |
|---|---|---|
| ref `packages/agent-core` | 371 个 TS 文件 | 约 6.9 万行 |
| ref `packages/agent-core-v2` | 782 个 TS 文件 | 更大 |
| 当前 `crates/kkagent-core` | 58 个 RS 文件 | 约 1.96 万行 |

ref `agent-core-v2` 拥有完整子系统：

> 注：`packages/agent-core` 为 v1 旧版引擎（legacy），CLI 默认已切换到 v2，当前 kkagent 无 v1 包袱，复刻目标应只聚焦 `agent-core-v2`。

- `agent/background`（后台任务）
- `agent/compaction`（上下文压缩）
- `agent/config`、`agent/context`
- `agent/cron`
- `agent/goal`
- `agent/injection`
- `agent/permission`
- `agent/replay`
- `agent/turn`
- `agent/tool`
- `session/store`、`session/export`
- `rpc`、`telemetry`、`plugin`
- 大量 builtin tools 和 provider tools

当前 `kkagent-core` 仅有：`agent_loop`、`context_memory`、`context_projector`、`event_bus`、`full_compaction`、`media_pipeline`、`permission`、`plugin`、`session`、`subagent_runtime`、`swarm`、`tool_scheduler`、`transcript` 等，很多还是简化实现。

---

## 5. 测试覆盖

| 项目 | 测试情况 |
|---|---|
| ref | 几乎每个 package 都有 `test/` 目录，包含大量 e2e/unit 测试 |
| 当前 | 无独立 `tests/` 目录；72 个文件含 `#[cfg(test)]`，但只是少量内联单元测试，整体覆盖率很低 |

---

## 6. 文档与配套

当前文档：

- `README.md`
- 10 篇 `docs/*.md`
- `AGENTS.md`、`TODO.md`
- 都比较精简

ref 文档：

- VitePress 完整站点（`docs/.vitepress/`）
- 中英文 README
- `CONTRIBUTING.md`、`SECURITY.md`
- `.changeset/` 变更日志管理
- issue 模板、PR 模板

---

## 7. CI/CD 与发布

当前：

- `.github/workflows/ci.yml`
- `.github/workflows/release.yml`
- `Makefile`（Rust cross compile）
- `install.sh` / `install.ps1`

ref：

- 9+ 个 workflow（native build、nix build、docs deploy、changesets、pkg-pr-new 等）
- GitHub Actions 复用 action（macOS notarize、keychain）
- 基于 changesets 的自动版本发布

---

## 8. 工程化与元数据

当前 crate 的 `Cargo.toml` 缺少：

- `authors`
- `license`（仅 workspace root 有 `license = "MIT"`）
- `description`
- `repository`
- `homepage`

ref 每个 `package.json` 都有完整元数据。

当前还缺少：

- `.changeset/` 变更管理
- Nix/ `flake.lock` 构建
- 多语言/国际化

（ref 在 `packages/klient` 下有一个 Dockerfile，但非核心差异。）

---

## 9. 代码质量标记

- 当前代码中 `TODO/FIXME/XXX/HACK/BUG` 仅出现在 2 个文件，表面干净，但主要是因为功能尚未覆盖到需要大量维护债务的阶段。
- ref 里有 55 个文件带这些标记，说明工程已迭代到需要持续处理技术债务的阶段。
- 当前仍有少量 `panic!` 是测试桩或强制断言，需逐步替换为错误处理。

---

## 10. 关键功能对照

| 功能 | 当前状态 | ref 状态 |
|---|---|---|
| Read/Write/Edit/Grep/Glob/Bash | 已实现 | 已实现且更完善 |
| TodoList/Plan/Goal/Task | 已实现 | 已实现 |
| MCP/Skills/Hooks | 基本实现 | 成熟（最近 commit 仍在修 MCP auth probe） |
| 多模态图片输入 | 已实现 | 已实现 |
| WebSearch/FetchURL | 已实现 | 已实现 |
| 沙盒隔离 | Linux Bubblewrap、macOS Seatbelt、Windows Job Object | 更完整的 kaos 沙盒 |
| 权限模式 manual/yolo/auto | 已实现 | 已实现 |
| 会话持久化/transcript | SQLite 基本实现 | 完整 transcript 包 |
| ACP / VSCode 扩展 | 极简 3 文件扩展 | 完整 VSCode 插件 |
| 后台 Agent / Cron | 基本实现 | 成熟 |
| 云端 telemetry | 基本实现 | 完整 telemetry 包 |

---

## 11. 结论

当前 `kkagent` 距离完整复刻 `ref/kimi-code` 还有**一个完整产品周期**的差距。主要问题不是某几个 bug，而是：

1. **规模差距约 20 倍**，大量子系统尚未拆分。
2. **核心 Agent 引擎严重简化**，尤其是 `agent-core-v2` 的 DI/scope/生命周期、完整 replay、compaction、turn 管理、后台任务等。
3. **CLI 命令和 IDE 集成**只是占位级别。
4. **测试、文档、发布流程**等工程基础设施尚未建立。
5. **crate 元数据、LICENSE、贡献指南**等开源必备项不完整。

### 建议优先补齐顺序

1. **重构/扩展 `crates/kkagent-core`**：对照 `agent-core` + `agent-core-v2` 拆分模块并完整实现。
2. **补齐 CLI 命令**：`doctor`、`export`、`login`、`provider`、`rotate-token`、`vis`、`web` 等。
3. **完善 VSCode 扩展**：从 3 个文件扩成完整插件。
4. **建立测试体系**：独立 `tests/` 目录 + 集成测试 + e2e。
5. **补充开源基础设施**：crate 元数据、LICENSE、CONTRIBUTING、changeset、多语言文档。
