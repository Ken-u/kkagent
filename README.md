# kkagent

[English](README.en.md) | 简体中文

[![CI](https://github.com/bianjinchen/kkagent/actions/workflows/ci.yml/badge.svg)](https://github.com/bianjinchen/kkagent/actions/workflows/ci.yml)
[![Release](https://github.com/bianjinchen/kkagent/actions/workflows/release.yml/badge.svg)](https://github.com/bianjinchen/kkagent/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

用 Rust 实现、面向生产使用的终端 Coding Agent，核心交互与运行时对齐 kimi-code CLI。
TUI 与 Agent Server 可分离，中间走 RPC（默认进程内 memory transport；也支持独立 `server` 模式）。

- 低内存、高性能
- 支持 Win / macOS / Linux（x86_64 / arm64）
- 权限模式：`manual` / `yolo` / `auto` / `plan`
- 内置工具：Read / Write / Edit / Grep / Glob / Bash / TodoList / Goal / Task / AskUser / SelectTools / Cron / Web / Media / Skill / Plan
- 会话、事件、turn 队列和后台 Agent 任务统一持久化到 `~/.kkagent/transcripts.db`
- Bash 系统隔离：Linux Bubblewrap、macOS Seatbelt、Windows Job Object
- MCP / Skills / Hooks（配置驱动）

## 快速安装

### macOS / Linux（推荐）

```bash
curl -fsSLO https://raw.githubusercontent.com/bianjinchen/kkagent/main/install.sh
sh install.sh
```

脚本默认安装到 `/usr/local/bin`，可通过环境变量覆盖：

```bash
KKAGENT_INSTALL_DIR=$HOME/.local/bin sh install.sh
```

### 手动下载

从 [GitHub Releases](https://github.com/bianjinchen/kkagent/releases/latest) 下载对应平台的 tar.gz / zip，解压后将 `kkagent` 放到 `PATH` 中即可。

当前发布矩阵：

| 平台 | 架构 | 产物 |
|---|---|---|
| macOS | x86_64 / arm64 | `kkagent-x86_64-apple-darwin.tar.gz` / `kkagent-aarch64-apple-darwin.tar.gz` |
| Linux (glibc) | x86_64 / arm64 | `kkagent-x86_64-unknown-linux-gnu.tar.gz` / `kkagent-aarch64-unknown-linux-gnu.tar.gz` |
| Linux (musl) | x86_64 / arm64 | `kkagent-x86_64-unknown-linux-musl.tar.gz` / `kkagent-aarch64-unknown-linux-musl.tar.gz` |
| Windows | x86_64 / arm64 | `kkagent-x86_64-pc-windows-msvc.zip` / `kkagent-aarch64-pc-windows-msvc.zip` |

### Windows（PowerShell）

```powershell
irm https://raw.githubusercontent.com/bianjinchen/kkagent/main/install.ps1 | iex
```

或先下载再执行：

```powershell
curl -fsSLO https://raw.githubusercontent.com/bianjinchen/kkagent/main/install.ps1
.\install.ps1
```

默认安装到 `%LOCALAPPDATA%\Programs\kkagent`，可通过环境变量覆盖：

```powershell
$env:KKAGENT_INSTALL_DIR = "$env:USERPROFILE\bin"; .\install.ps1
```

## 30 秒上手

1. 初始化配置（交互式向导）：

```bash
kkagent init
```

或手动创建 `~/.kkagent/config.toml`：

```toml
default_model = "kimi-k2-0711-preview"

[providers.kimi]
api_base = "https://api.moonshot.cn/v1"
api_key = "sk-..."

[permissions]
default_mode = "manual"
```

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

- **原生 Rust**：单二进制、低内存、秒级启动，跨平台原生支持。
- **Agent 运行时**：多轮对话、工具调用循环、自动上下文压缩、token 预算与 turn 预算管理。
- **TUI / Server 分离**：默认 TUI 与 Agent Server 在同一进程内通过 memory transport 通信；可用 `kkagent server` 启动独立后台，通过 UDS / TCP 连接。
- **权限模型**：
  - `manual`：每次写操作、Bash 等需要人工确认。
  - `auto`：仅写文件与危险命令需确认，只读操作自动通过。
  - `yolo`：完全自动通过，适合受信任的 CI / 自动化场景。
  - `plan`：先审阅计划再执行，默认只读倾向。
- **沙箱隔离**：Bash 默认使用系统级沙箱（Linux Bubblewrap、macOS Seatbelt、Windows Job Object），支持只读 / 限制网络等策略。
- **持久化**：会话、事件、turn 队列和后台 Agent 任务统一写入 `~/.kkagent/transcripts.db`，支持 `--resume` 恢复。
- **MCP 与 Skills**：通过配置接入外部 MCP Server，用 Skill 封装常用提示词和工具组合。
- **可观测性**：结构化日志、HTTP 审计日志、telemetry 事件（可配置上报）。

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
| 任务管理 | `TodoList` / `Goal` / `Task` | TODO 追踪、多轮目标、后台子 Agent 任务。 |
| 交互 | `AskUserQuestion` / `SelectTools` | 用户确认、工具选择。 |
| 上下文 | `Skill` | 加载并执行 skill 模板。 |
| 计划 | `Plan` | 计划模式相关工具。 |
| 定时 | `CronCreate` / `CronDelete` / `CronList` | 调度提示词到未来执行。 |
| Web / 媒体 | `Web` / `Media` | 网页抓取、媒体文件读取。 |

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

完整配置项、Provider、Model、MCP、Hooks 说明见 [docs/configuration.md](docs/configuration.md)。

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
- [Kimi Code 差异分析](docs/kimi-code-gap-analysis.md)
- [开发与测试](docs/development.md)

## 开发

```bash
git clone https://github.com/bianjinchen/kkagent.git
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
