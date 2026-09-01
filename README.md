# kkagent

[English](README.en.md) | 简体中文

[![CI](https://github.com/Ken-u/kkagent/actions/workflows/ci.yml/badge.svg)](https://github.com/Ken-u/kkagent/actions/workflows/ci.yml)
[![Release](https://github.com/Ken-u/kkagent/actions/workflows/release.yml/badge.svg)](https://github.com/Ken-u/kkagent/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

可后台持续运行、可恢复、原生跨平台的 Rust 终端 Coding Agent。

kkagent 是一个用 Rust 实现的终端 Coding Agent，重点增强长任务运行、多会话协作、上下文恢复和本地安全控制。

- **任务不中断**：按 `Ctrl+B` 离开 TUI，独立 Agent Server 与进行中的任务继续运行；再次执行 `kk` 自动恢复现场。
- **主线不被打断**：通过会话标签、`/fork` 和 BTW 全屏侧问工作区并行探索不同问题。
- **安全可控**：提供 `manual` / `auto` / `yolo` / `plan` 四种权限模式，以及系统级 Bash 沙箱。
- **原生跨平台**：Rust 单二进制分发，无 Node.js 运行时依赖；支持 Windows / macOS / Linux（x86_64 / arm64）。
- **可扩展**：内置 MCP、Skills、Hooks、Web UI、ACP 和插件市场，可用于交互开发、远程接入与 CI 自动化。

<p align="center">
  <img src="docs/output.gif" alt="kkagent 终端 Coding Agent 演示" width="800">
</p>

[快速安装](#快速安装) · [30 秒上手](#30-秒上手) · [功能亮点](#功能亮点) · [完整文档](#文档索引)

## 为什么选择 kkagent

| 你关心的问题 | kkagent 的处理方式 |
|---|---|
| 关闭 TUI 后任务会不会中断？ | 独立 Server 持续运行，重新执行 `kk` 自动恢复会话和进行中的任务。 |
| 同时探索多个方向会不会弄乱上下文？ | 会话标签、`/fork` 与 BTW 侧问工作区相互隔离，主对话无需停下来。 |
| Agent 执行命令是否安全？ | 四种权限模式、系统级 Bash 沙箱、隐私路径策略与审计记录共同控制风险。 |
| 长会话或意外断线后能否继续？ | 会话、事件、turn 队列和后台任务写入 SQLite，支持重启、断线和压缩后的恢复。 |
| 能否接入现有开发流程？ | 支持非交互模式、结构化输入输出、MCP、ACP、Web UI、插件与远程执行环境。 |

### 功能亮点

差异化能力主要集中在后台运行、会话导航和终端体验：

| 功能 | 快捷键 / 命令 | 带来的体验 |
|---|---|---|
| 会话后台 detach | `Ctrl+B` | 退出 TUI 而不中断整个会话；后台 turn 继续运行，再次执行 `kk` 自动恢复。 |
| 会话标签页 | 空输入时 `Tab` / `←` / `→` · `Ctrl+Shift+Tab` | 在 `/new`、`/fork` 派生的会话族之间快速切换，底部常驻会话条。 |
| BTW 全屏侧问 | `/btw` · `Ctrl+G` | 从当前会话快照独立提问，不污染或阻塞主对话。 |
| Goal 目标模式 | `/goal <objective>` · `/goal status/pause/resume/cancel` | 给 Agent 一个可跨多轮持续推进的目标：footer 常驻 goal 指示器，运行中的 turn 也能即时注入新目标；可选开启独立裁判 agent 审查"完成"申报，未达标时带着缺口继续推进，防止草率收工。 |
| Todo 贴底面板 | 自动 | 任务状态常驻输入框上方，并按终端宽度自动折叠。 |
| 会话记录搜索 | `Ctrl+F` | 对整个 transcript 全文搜索，快速定位历史结论。 |
| Emacs 风格行编辑 | `Ctrl+K` / `Ctrl+W` / `Ctrl+Y` / `Ctrl+Z` / `Ctrl+Shift+Z` | kill line / kill word / yank / undo / redo。 |
| 原生 scrollback | `--no-alt-screen` | 保留终端原生滚动缓冲，方便翻页、选择和复制。 |
| 强制全量重绘 | `F5` | 修复 SSH / 老终端下的撕裂帧与幽灵字符。 |
| 非交互编排 | `--max-turns` · `--input-format` | 限制任务轮数，并用 stream-json 驱动 Agent。 |
| Shell 补全 | `kkagent completions` | 生成 bash / zsh / fish / PowerShell 补全脚本。 |

此外还支持独立 Server 生命周期管理、运行中输入排队、大段粘贴自动折叠，以及鼠标滚动、拖拽选择复制（含双击/三击按词/按行选取）。

### 适合这些场景

- 运行耗时较长的 Coding Agent 任务，希望离开终端后任务仍能继续。
- 同时维护多个会话、分支或探索方向，又不希望上下文互相干扰。
- 需要明确的权限审批、系统沙箱、凭据保护和审计轨迹。
- 希望以单二进制方式部署到 Windows、macOS、Linux 或远程开发机。
- 需要通过脚本、CI、Web UI、ACP、MCP 或插件扩展 Agent 工作流。

## 快速安装

### macOS / Linux（推荐）

```bash
curl --proto '=https' --tlsv1.2 -fsSL https://raw.githubusercontent.com/Ken-u/kkagent/main/install.sh | sh
```

安装器会自动识别 macOS/Linux 与 x86_64/arm64，下载最新 Release 并校验 SHA-256。Linux 默认使用 musl 静态包（`unknown-linux-musl`），无需系统 OpenSSL。`/usr/local/bin` 可写时安装到该目录，否则自动使用 `~/.local/bin`。也可指定目录、版本或 glibc 包：

```bash
curl --proto '=https' --tlsv1.2 -fsSLO https://raw.githubusercontent.com/Ken-u/kkagent/main/install.sh
KKAGENT_INSTALL_DIR="$HOME/bin" KKAGENT_VERSION=<version> sh install.sh
KKAGENT_TARGET=x86_64-unknown-linux-gnu sh install.sh
```

安装完成后会同时安装 `kkagent-update`，以后直接执行该命令即可升级，且会沿用原安装目录。TUI 默认每 24 小时在后台检查一次 GitHub Release；只显示提示，不会自动下载。可在配置中设置 `[ui] check_updates = false` 关闭。

### 手动下载

从 [GitHub Releases](https://github.com/Ken-u/kkagent/releases/latest) 下载对应平台的 tar.gz / zip，按 `SHA256SUMS` 校验后将 `kkagent` 放到 `PATH` 中即可。

当前发布矩阵：

| 平台 | 架构 | 产物 |
|---|---|---|
| macOS | x86_64 / arm64 | `kkagent-x86_64-apple-darwin.tar.gz` / `kkagent-aarch64-apple-darwin.tar.gz` |
| Linux (musl, 静态) | x86_64 / arm64 | `kkagent-x86_64-unknown-linux-musl.tar.gz` / `kkagent-aarch64-unknown-linux-musl.tar.gz` |
| Linux (glibc) | x86_64 / arm64 | `kkagent-x86_64-unknown-linux-gnu.tar.gz` / `kkagent-aarch64-unknown-linux-gnu.tar.gz` |
| Windows | x86_64 / arm64 | `kkagent-x86_64-pc-windows-msvc.zip` / `kkagent-aarch64-pc-windows-msvc.zip` |

### Windows（PowerShell）

```powershell
irm https://raw.githubusercontent.com/Ken-u/kkagent/main/install.ps1 | iex
```

或先下载再执行：

```powershell
curl -fsSLO https://raw.githubusercontent.com/Ken-u/kkagent/main/install.ps1
.\install.ps1
```

默认安装到 `%LOCALAPPDATA%\Programs\kkagent`，可通过环境变量覆盖：

```powershell
$env:KKAGENT_INSTALL_DIR = "$env:USERPROFILE\bin"; .\install.ps1 -Version <version>
```

安装完成后可执行 `kkagent-update.ps1` 升级，并沿用原安装目录。

## 30 秒上手

kkagent 支持 Anthropic、Kimi、OpenAI / OpenAI Responses、Google Gemini 以及 OpenAI 兼容端点，可同时配置多个模型并按会话切换。

1. 初始化配置（交互式向导，也可在这里完成 Kimi 托管账号登录）：

```bash
kkagent init
```

或手动创建 `~/.kkagent/config.toml`：

```toml
default_model = "kimi-k2-0711-preview"

[providers.kimi]
type = "kimi"
base_url = "https://api.moonshot.cn/v1"
# 推荐：用环境变量引用密钥，避免明文提交
api_key_env = "KIMI_API_KEY"

[models."kimi-k2-0711-preview"]
provider = "kimi"

# 可选：主模型耗尽正常单步重试后切换到此模型
# fallback_model = "backup-model"

[permissions]
default_permission_mode = "manual"
```

> 配置密钥更推荐通过环境变量（`api_key_env`）或 `kkagent auth login` 注入，避免在配置文件中明文保存 API Key。完整字段说明见[配置参考](#配置参考)与 `docs/configuration.md`。

2. 启动 TUI：

```bash
kkagent
```

`kkagent` 命令同时会以 `kk` 软链接的形式安装到同一目录，直接敲 `kk` 等价于 `kkagent`。

3. 非交互执行单次任务：

```bash
kkagent -y -p "Read ./Cargo.toml and count workspace members"
```

也可以简写为：

```bash
kk -y -p "Read ./Cargo.toml and count workspace members"
```

更多用法见 [docs/cli-and-tui.md](docs/cli-and-tui.md)。

## 核心特性

- **原生 Rust**：单二进制分发，无 Node.js 运行时依赖，跨平台原生支持。
- **Agent 运行时**：多轮对话、工具调用循环、自动上下文压缩、token 预算与 turn 预算管理。
- **TUI / Server 分离**：默认使用进程内 memory transport，也可通过 UDS / TCP 连接独立 Server，并用 `Ctrl+B` 将整个会话留在后台运行。
- **安全执行**：四种权限模式配合 Linux Bubblewrap、macOS Seatbelt、Windows Job Object，支持只读、限制网络与细粒度授权策略。
- **可靠恢复**：会话、事件、turn 队列、后台任务与检查点持久化到 `~/.kkagent/transcripts.db`，支持 `--resume`、断线重连与跨重启恢复。
- **多会话协作**：会话标签、`/new`、`/fork`、BTW 侧问、Todo 面板和 transcript 搜索共同服务长任务工作流。
- **自动化与接入**：支持 headless / CI 结构化输入输出、Web UI、ACP，以及本地和 SSH 远程执行环境。
- **可扩展工具系统**：内置文件、搜索、Shell、任务、计划、Web 和媒体工具，并支持 MCP、Skills、Hooks 与插件市场；插件还可声明自定义子 Agent 类型与模型绑定。
- **可观测性**：结构化日志、HTTP 审计日志和可配置 telemetry 事件。

<details>
<summary><strong>运行时与工程实现细节</strong></summary>

- **后台与多会话**：workspace session registry 与跨目录会话恢复；子 Agent 会话页签；AgentSwarm 并行子代理（超时 / 限流恢复、可后台化）；断线重连后恢复 BTW、prompt 队列、审批与 live stream。
- **Goal 目标运行时**：目标状态机（active/paused/blocked/complete）与 turns/tokens/wall-clock 三维预算；turn 边界自动续跑；可选完成判定裁判 agent（`[goal] judge_enabled`）——独立模型通过 `GoalJudge` toolcall 标记 approve/reject，拒绝达到上限转 blocked，裁判故障自动 fail-open；裁决记录可在 TUI 点击 footer goal 指示器查看。
- **插件与子 Agent**：插件市场多源安装（GitHub / GitBucket 兼容 forge、`tree/<ref>/<dir>` 子目录源）；插件可声明自定义子 Agent 类型（ACP 外部 + 内部，带缓存 profile registry）与专属模型绑定；`kkagent doctor` 提供 fail-fast / 卫生检查。
- **Web UI**：深色主题、Markdown 渲染、移动端侧栏、model picker、Timeline 逐轮 diff、plan review 与插件面板；支持通过 `--http` 热挂到已运行的 Server。
- **工具系统**：渐进式工具披露、MCP schema 延迟通告、未知工具 BM25 模糊建议；针对超大 workspace 的流事件合并和 transcript 布局缓存；统一后台任务面板（`/tasks` + `/ps`）。
- **LLM 工程**：Anthropic / DeepSeek prompt caching 与缓存命中率统计；`compaction_model`、per-model thinking effort、`api_key_env`、可配置重试退避、流式首 token 超时门控与跨 chunk UTF-8 重组。
- **安全与沙箱**：S0–S2 隐私路径策略；声明式 toolchain sandbox profile；Once / Turn / Session / Workspace 授权范围；`shell -c` 绕过检测、always-approvals 持久化、凭据目录 deny 与安全审计轨迹。
- **运行时可靠性**：磁盘持久化 turn 检查点；undo 跨重启与压缩存活；逐消息 transcript 持久化与孤儿 tool-use 修复；超大工具结果落盘和 trash 归档；malformed tool call 自动重试。
- **上下文与隔离**：预算安全的 payload projector、`/compact` 自动压缩、`/context` 分项 token 透视；无原生依赖的 bash AST tokenizer/parser；子 Agent 独立 git worktree、跨会话写冲突告警与测试命令隔离。
- **终端体验细节**：剪贴板图片粘贴折叠为 `[Pasted Image #n]` 标记；完成通知（OSC 9）附带净化后的 prompt；实验性 `mouse_mode = "off"` 兼容不支持鼠标上报的 SSH 客户端。

</details>

## 架构概览

Workspace 包含 16 个 crate：

| Crate | 说明 |
|---|---|
| `kkagent` | 主入口二进制，CLI / TUI 启动器。 |
| `kkagent-protocol` | 跨 crate 的协议类型、消息与错误定义。 |
| `kkagent-rpc` | RPC 传输层（memory / UDS / TCP）。 |
| `kkagent-config` | TOML 配置加载、验证、环境变量覆盖。 |
| `kkagent-llm` | LLM Provider 抽象、流解析、token 计数。 |
| `kkagent-core` | Agent 主循环、上下文投影、权限链、计划审阅。 |
| `kkagent-tools` | 内置工具实现与注册表。 |
| `kkagent-mcp` | MCP Client / Server 支持。 |
| `kkagent-client` | 高层客户端封装。 |
| `kkagent-tui` | 终端交互界面。 |
| `kkagent-di` | 依赖注入容器。 |
| `kkagent-wire` | 序列化与消息编解码。 |
| `kkagent-telemetry` | 遥测事件与可配置上报。 |
| `kkagent-acp` | Agent-Client Protocol / 长连接协议实现。 |
| `kkagent-oauth` | OAuth / 令牌管理。 |
| `kkagent-kaos` | 混沌与压力测试辅助。 |

### 内置工具清单

| 类别 | 工具 | 说明 |
|---|---|---|
| 文件读写 | `Read` / `Write` / `Edit` / `Glob` | 读取、写入、行级编辑、批量查找。 |
| 搜索 | `Grep` | 正则搜索，支持上下文与多文件过滤。 |
| 执行 | `Bash` | 带沙箱与权限策略的命令执行，支持后台 shell。 |
| 任务管理 | `TodoList` / `Goal` / `Agent` / `TaskOutput` | TODO 追踪、多轮目标（可选完成判定裁判）、子代理委派与后台任务管理。 |
| 交互 | `AskUserQuestion` / `SelectTools` | 用户确认、工具选择。 |
| 上下文 | `Skill` | 加载并执行 skill 模板。 |
| 计划 | `Plan` | 计划模式相关工具。 |
| 定时 | `Cron`（action=create/list/delete） | 调度提示词到未来执行。 |
| Web / 媒体 | `Web`（action=search/fetch） / `ReadMediaFile` | 网页搜索与抓取、媒体文件读取。 |

工具声明与权限策略由 `kkagent-tools` 统一管理，新增工具会自动进入权限评估流程。

## 权限模式

| 模式 | 行为 |
|---|---|
| `manual` | 每次写操作、Bash、危险命令都弹窗确认。 |
| `auto` | 只读工具与低风险操作自动通过；写文件、Bash 仍需确认。 |
| `yolo` | 全部操作自动批准，适合自动化脚本与受信环境。 |
| `plan` | 先让 Agent 生成计划，用户审阅后再批量执行；默认只读倾向。 |

启动时通过 `-y/--yolo`、`--auto`、`--plan` 切换，TUI 内可用 `/yolo`、`/auto`、`/plan` 动态切换。详见 [docs/tools-and-permissions.md](docs/tools-and-permissions.md)。

## 配置参考

kkagent 读取一份 TOML 配置：优先使用 `--config <path>`，否则读取 `~/.kkagent/config.toml`。常用命令：

```bash
kkagent config show
kkagent config get sandbox.mode
kkagent config set sandbox.network false
kkagent config preset safe
```

排障时可用 `kkagent --disable-sandbox` 仅对当前进程关闭 Bash OS 沙箱和资源限制；它不修改配置，只建议在受控容器或 VM 中使用。

完整配置项、Provider、Model、MCP、Hooks、插件市场说明见 [docs/configuration.md](docs/configuration.md)。

### 插件市场

`/plugins` 打开插件管理（已安装列表、市场浏览、安装/更新/启用/禁用）。默认市场用 `plugin_marketplace`，额外市场用 `plugin_marketplaces`：

```toml
plugin_marketplace = "https://plugins.example.com/marketplace.json"
plugin_marketplaces = [
  "http://git.example.com/org/kk-plugins",
  { name = "team", source = "/data/kk-plugins/marketplace.json" },
]
```

`KKAGENT_PLUGIN_MARKETPLACE_URL` 只覆盖默认那一项。也支持 GitHub / GitBucket 等兼容 forge 的仓库首页和 `tree/<ref>/<plugin-dir>` 源（安装时下 zip 并只取该子目录）。详情见 [docs/extensions.md](docs/extensions.md)。

## 文档索引

- [安装与快速开始](docs/getting-started.md)
- [配置参考](docs/configuration.md)
- [CLI 与 TUI](docs/cli-and-tui.md)
- [工具与权限](docs/tools-and-permissions.md)
- [Agent Server API](docs/server-api.md)
- [故障排查](docs/troubleshooting.md)
- [发布与安装包](docs/releases.md)
- [安全设计](docs/security.md)
- [架构设计](docs/architecture.md)
- [扩展机制](docs/extensions.md)
- [运维与监控](docs/operations.md)
- [开发与测试](docs/development.md)

## 致谢

本项目的交互设计与运行时模型参考了 [kimi-code](https://github.com/MoonshotAI/kimi-code) CLI，在此表示感谢。kkagent 使用 Rust 独立实现，并未复用其代码。

## 开发

```bash
git clone https://github.com/Ken-u/kkagent.git
cd kkagent

cargo build --release --workspace
./target/release/kkagent --help

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

提交前请阅读 [docs/development.md](docs/development.md) 中的提交约定与跨平台构建说明。

## License

MIT — 详见 [LICENSE](LICENSE)。
