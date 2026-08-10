# kkagent

English | [简体中文](README.md)

[![CI](https://github.com/bianjinchen/kkagent/actions/workflows/ci.yml/badge.svg)](https://github.com/bianjinchen/kkagent/actions/workflows/ci.yml)
[![Release](https://github.com/bianjinchen/kkagent/actions/workflows/release.yml/badge.svg)](https://github.com/bianjinchen/kkagent/actions/workflows/release.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Rust: 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)](https://www.rust-lang.org/)

A production-oriented terminal Coding Agent written in Rust. Core interaction and runtime are aligned with the kimi-code CLI.
The TUI and Agent Server are decoupled and communicate over RPC (default: in-process memory transport; standalone `server` mode is also supported).

- Low memory footprint and high performance
- Supports Windows, macOS, and Linux (x86_64 / arm64)
- Permission modes: `manual`, `yolo`, `auto`, `plan`
- Built-in tools: Read, Write, Edit, Grep, Glob, Bash, TodoList, Goal, Task, AskUser, SelectTools, Cron, Web, Media, Skill, Plan
- Sessions, events, turn queues, and background Agent tasks are persisted to `~/.kkagent/transcripts.db`
- Bash sandboxing: Linux Bubblewrap, macOS Seatbelt, Windows Job Object
- MCP / Skills / Hooks (configuration-driven)

## Quick Install

### macOS / Linux (recommended)

```bash
curl -fsSLO https://raw.githubusercontent.com/bianjinchen/kkagent/main/install.sh
sh install.sh
```

The script installs to `/usr/local/bin` by default. Override it with an environment variable:

```bash
KKAGENT_INSTALL_DIR=$HOME/.local/bin sh install.sh
```

### Manual download

Download the archive for your platform from [GitHub Releases](https://github.com/bianjinchen/kkagent/releases/latest), extract it, and place `kkagent` on your `PATH`.

Current release matrix:

| Platform | Architecture | Artifact |
|---|---|---|
| macOS | x86_64 / arm64 | `kkagent-x86_64-apple-darwin.tar.gz` / `kkagent-aarch64-apple-darwin.tar.gz` |
| Linux (glibc) | x86_64 / arm64 | `kkagent-x86_64-unknown-linux-gnu.tar.gz` / `kkagent-aarch64-unknown-linux-gnu.tar.gz` |
| Linux (musl) | x86_64 / arm64 | `kkagent-x86_64-unknown-linux-musl.tar.gz` / `kkagent-aarch64-unknown-linux-musl.tar.gz` |
| Windows | x86_64 / arm64 | `kkagent-x86_64-pc-windows-msvc.zip` / `kkagent-aarch64-pc-windows-msvc.zip` |

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/bianjinchen/kkagent/main/install.ps1 | iex
```

Or download first and then execute:

```powershell
curl -fsSLO https://raw.githubusercontent.com/bianjinchen/kkagent/main/install.ps1
.\install.ps1
```

The script installs to `%LOCALAPPDATA%\Programs\kkagent` by default. Override it with an environment variable:

```powershell
$env:KKAGENT_INSTALL_DIR = "$env:USERPROFILE\bin"; .\install.ps1
```

## 30-Second Quickstart

1. Initialize the config (interactive wizard):

```bash
kkagent init
```

Or create `~/.kkagent/config.toml` manually:

```toml
default_model = "kimi-k2-0711-preview"

[providers.kimi]
api_base = "https://api.moonshot.cn/v1"
api_key = "sk-..."

[permissions]
default_mode = "manual"
```

2. Launch the TUI:

```bash
kkagent
```

The `kkagent` binary is also installed as a `kk` symlink in the same directory, so typing `kk` is equivalent to `kkagent`.

3. Run a one-off task non-interactively:

```bash
kkagent -y -p "Read ./Cargo.toml and count workspace members"
```

Or using the short alias:

```bash
kk -y -p "Read ./Cargo.toml and count workspace members"
```

See [docs/cli-and-tui.md](docs/cli-and-tui.md) for more.

## Core Features

- **Native Rust**: single binary, low memory, fast startup, native cross-platform support.
- **Agent runtime**: multi-turn conversation, tool-call loop, automatic context compression, token budget and turn budget management.
- **TUI / Server decoupling**: by default the TUI and Agent Server run in-process via memory transport; use `kkagent server` to start a standalone backend over UDS / TCP.
- **Permission model**:
  - `manual`: every write, Bash, or dangerous operation requires approval.
  - `auto`: read-only and low-risk operations pass automatically; file writes and Bash still require approval.
  - `yolo`: everything is auto-approved, suitable for trusted CI / automation environments.
  - `plan`: the Agent drafts a plan first; the user reviews it before batch execution; read-only by default.
- **Sandboxed execution**: Bash runs in a system-level sandbox (Linux Bubblewrap, macOS Seatbelt, Windows Job Object) with read-only / network-restricted policies.
- **Persistence**: sessions, events, turn queues, and background Agent tasks are stored in `~/.kkagent/transcripts.db`; supports `--resume`.
- **MCP and Skills**: connect external MCP Servers via configuration and wrap common prompts and tool combinations as Skills.
- **Observability**: structured logging, HTTP audit logs, telemetry events (configurable upload).

## Architecture Overview

The workspace contains 16 crates:

| Crate | Description |
|---|---|
| `kkagent` | Main binary entrypoint, CLI / TUI launcher. |
| `kkagent-protocol` | Protocol types, messages, and error definitions shared across crates. |
| `kkagent-rpc` | RPC transport layer (memory / UDS / TCP). |
| `kkagent-config` | TOML config loading, validation, and environment variable overrides. |
| `kkagent-llm` | LLM provider abstraction, stream parsing, token counting. |
| `kkagent-core` | Agent main loop, context projection, permission chain, plan review. |
| `kkagent-tools` | Built-in tool implementations and registry. |
| `kkagent-mcp` | MCP Client / Server support. |
| `kkagent-client` | High-level client wrapper. |
| `kkagent-tui` | Terminal user interface. |
| `kkagent-di` | Dependency injection container. |
| `kkagent-wire` | Serialization and message encoding/decoding. |
| `kkagent-telemetry` | Telemetry events and configurable upload. |
| `kkagent-acp` | Agent-Client Protocol / long-connection protocol implementation. |
| `kkagent-oauth` | OAuth / token management. |
| `kkagent-kaos` | Chaos and stress-testing helpers. |

### Built-in Tool List

| Category | Tool | Description |
|---|---|---|
| File I/O | `Read` / `Write` / `Edit` / `Glob` | Read, write, line-level edit, batch file discovery. |
| Search | `Grep` | Regex search with context and multi-file filtering. |
| Execution | `Bash` | Sandboxed command execution with permission policies and background shells. |
| Task management | `TodoList` / `Goal` / `Task` | TODO tracking, multi-turn goals, background sub-Agent tasks. |
| Interaction | `AskUserQuestion` / `SelectTools` | User confirmation and tool selection. |
| Context | `Skill` | Load and execute skill templates. |
| Planning | `Plan` | Tools related to plan mode. |
| Scheduling | `CronCreate` / `CronDelete` / `CronList` | Schedule prompts for future execution. |
| Web / Media | `Web` / `Media` | Web fetching and media file reading. |

Tool declarations and permission policies are managed centrally by `kkagent-tools`; new tools automatically enter the permission evaluation flow.

## Permission Modes

| Mode | Behavior |
|---|---|
| `manual` | Every write, Bash, or dangerous command triggers a confirmation prompt. |
| `auto` | Read-only tools and low-risk operations pass automatically; file writes and Bash still require approval. |
| `yolo` | All operations are auto-approved; suitable for automation scripts and trusted environments. |
| `plan` | The Agent generates a plan first; the user reviews it before execution; read-only by default. |

Switch at startup with `-y/--yolo`, `--auto`, or `--plan`; switch dynamically in the TUI with `/yolo`, `/auto`, or `/plan`. See [docs/tools-and-permissions.md](docs/tools-and-permissions.md) for details.

## Configuration Reference

kkagent reads a single TOML config: `--config <path>` takes precedence, otherwise `~/.kkagent/config.toml` is used. Common commands:

```bash
kkagent config show
kkagent config get sandbox.mode
kkagent config set sandbox.network false
kkagent config preset safe
```

For the full list of config options, providers, models, MCP, and hooks, see [docs/configuration.md](docs/configuration.md).

## Documentation Index

- [Getting Started](docs/getting-started.md)
- [Configuration Reference](docs/configuration.md)
- [CLI and TUI](docs/cli-and-tui.md)
- [Tools and Permissions](docs/tools-and-permissions.md)
- [Agent Server API](docs/server-api.md)
- [Troubleshooting](docs/troubleshooting.md)
- [Releases and Packages](docs/releases.md)
- [Security Design](docs/security.md)
- [Architecture Design](docs/architecture.md)
- [Extension Mechanisms](docs/extensions.md)
- [Operations and Monitoring](docs/operations.md)
- [Kimi Code Gap Analysis](docs/kimi-code-gap-analysis.md)
- [Development and Testing](docs/development.md)

## Development

```bash
git clone https://github.com/bianjinchen/kkagent.git
cd kkagent

cargo build --release --workspace
./target/release/kkagent --help

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

Please read [docs/development.md](docs/development.md) for commit conventions and cross-platform build instructions before submitting changes.

## License

MIT — see [LICENSE](LICENSE).
