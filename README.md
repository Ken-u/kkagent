# kkagent

用 Rust 实现、面向生产使用的终端 Coding Agent，核心交互与运行时对齐 kimi-code CLI。
TUI 与 Agent Server 可分离，中间走 RPC（当前默认进程内 memory transport；也支持独立 `server` 模式）。

- 低内存、高性能
- 支持 Win / macOS / Linux（x86_64 / arm64）
- 权限模式：`manual` / `yolo` / `auto`
- 内置工具：Read / Write / Edit / Grep / Glob / Bash / TodoList / Goal / Task
- 会话、事件、turn 队列和后台 Agent 任务统一持久化到 `~/.kkagent/transcripts.db`
- Bash 系统隔离：Linux Bubblewrap、macOS Seatbelt、Windows Job Object
- MCP / Skills / Hooks（配置驱动）

## 文档导航

完整手册见 [docs/README.md](docs/README.md)：

- [安装与快速开始](docs/getting-started.md)
- [完整配置参考](docs/configuration.md)
- [CLI 与 TUI](docs/cli-and-tui.md)
- [工具与权限](docs/tools-and-permissions.md)
- [Agent Server API](docs/server-api.md)
- [扩展机制](docs/extensions.md)
- [安全与运维](docs/security.md)
- [架构与开发](docs/architecture.md)
- [故障排查](docs/troubleshooting.md)

---

## 1. 怎么编译

### 依赖

- Rust **1.88+**（仓库通过 `rust-toolchain.toml` 固定最低工具链）
- 系统：macOS / Linux / Windows

```bash
rustc --version
cargo --version
```

### Debug 构建

```bash
cd /path/to/kkagent
cargo build
# 产物：target/debug/kkagent
```

或：

```bash
make build
```

### Release 构建（推荐日常使用）

```bash
cargo build --release
# 产物：target/release/kkagent
```

或：

```bash
make release
```

### 安装到 PATH（可选）

```bash
make install
# 拷贝到 /usr/local/bin/kkagent
```

### 跨平台交叉编译

已装好 target 时：

```bash
# 单目标示例
rustup target add aarch64-apple-darwin
rustup target add x86_64-unknown-linux-musl
rustup target add x86_64-pc-windows-gnu

cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-unknown-linux-musl
cargo build --release --target x86_64-pc-windows-gnu

# CI 会在 Linux/macOS/Windows 的 x86_64/arm64 组合上执行原生或交叉检查
```

> Windows / Linux musl 交叉编译可能还需要对应 linker；本机原生 `cargo build --release` 最稳。

---

## 2. 配置怎么写

### 配置路径

优先级：

1. 命令行 `--config /path/to/config.toml`
2. 默认：`~/.kkagent/config.toml`

首次使用建议：

```bash
mkdir -p ~/.kkagent
cp examples/config.example.toml ~/.kkagent/config.toml
# 再编辑 api_key / base_url / default_model
```

仓库根目录的 `.env` 是本机调试用 TOML 样例（名字叫 `.env`，内容实际是 TOML）。正式用法请放进 `~/.kkagent/config.toml`，不要把密钥提交进 git。

### 最小可用配置

```toml
default_model = "local/claude-opus-4-8"
default_permission_mode = "yolo"   # manual | yolo | auto
default_plan_mode = false

[providers.local]
type = "anthropic"
api_key = "sk-xxxx"
base_url = "http://127.0.0.1:3000"   # Anthropic Messages 兼容端点

[models."local/claude-opus-4-8"]
provider = "local"
model = "claude-opus-4-8"
max_context_size = 262144
max_output_size = 16384              # 建议 <= 16384，过大易被上游拒绝
capabilities = ["tool_use"]
display_name = "claude-opus-4-8"
```

说明：

| 字段 | 含义 |
|------|------|
| `default_model` | 模型别名，必须对应 `models."别名"` |
| `providers.*.type` | `anthropic`、`openai`、`openai_responses`、`kimi`、`google-genai` 使用各自的流式协议 |
| `providers.*.base_url` | 可带或不带末尾 `/v1`，客户端会避免重复路径 |
| `providers.*.api_key` | Anthropic 使用 `x-api-key`，OpenAI/Kimi 使用 Bearer token |
| `models.*.model` | 发给上游的真实 model id |
| `models.*.capabilities` | 含 `tool_use` 才会带工具定义；`thinking` 可按需开 |
| `default_permission_mode` | `manual` 每次确认；`yolo` 自动批准常规工具；`auto` 更激进 |

### MCP 服务器（可选）

```toml
[mcp_servers.filesystem]
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
```

### 权限规则（可选）

```toml
[[permission.rules]]
decision = "allow"
scope = "user"
pattern = "Read"
```

### Skills / Hooks 目录

| 路径 | 用途 |
|------|------|
| `~/.kkagent/skills/<name>/SKILL.md` | 全局 skill |
| `.kkagent/skills/<name>/SKILL.md` | 项目 skill |
| `AGENTS.md` | 项目约定（会被发现） |
| `~/.kkagent/hooks.json` 或 `.kkagent/hooks.json` | Hook 脚本 |
| `~/.kkagent/transcripts.db` | 会话 SQLite |

---

## 3. 怎么用

### 交互 TUI（默认）

```bash
# 用默认配置
./target/release/kkagent

# 指定配置
./target/release/kkagent --config ~/.kkagent/config.toml

# 启动即 yolo / auto / plan
./target/release/kkagent -y
./target/release/kkagent --auto
./target/release/kkagent --plan
```

### 常用快捷键

| 按键 | 作用 |
|------|------|
| Enter | 提交 |
| Shift-Enter | 换行 |
| Esc / Ctrl-C | 中断流式输出 |
| Ctrl-C×2（空输入） | 退出 |
| Ctrl-D（空输入） | 退出 |
| Shift-Tab | 切换 Plan 模式 |
| `!`（空输入） | Shell 模式 |
| Ctrl-O | 折叠/展开工具输出 |

### 斜杠命令

| 命令 | 作用 |
|------|------|
| `/yolo` | 切换 YOLO |
| `/auto` | 切换 Auto |
| `/plan` | 切换 Plan（只读倾向） |
| `/new` | 新会话 |
| `/sessions` | 列出会话 |
| `/resume <id>` | 恢复会话 |
| `/compact` | 压缩历史 |
| `/undo` | 撤销上一轮 |
| `/model` | 查看/切换模型 |
| `/help` | 帮助 |
| `/exit` | 退出 |

### 非交互打印模式（脚本 / CI / 烟雾测试）

```bash
./target/release/kkagent --config ~/.kkagent/config.toml \
  -p "Read ./Cargo.toml and count workspace members"
```

输出写到 **stdout**，日志写到 **stderr**。

调试日志：

```bash
RUST_LOG=kkagent_core=info,kkagent_llm=debug \
  ./target/release/kkagent --config ~/.kkagent/config.toml -p "Say hello"
```

### Kimi 托管账号登录

```bash
kkagent auth login
kkagent auth status
kkagent auth logout
```

登录使用 Kimi device-code OAuth。凭据以 `0600` 权限原子写入
`~/.kkagent/credentials/kimi-code.json`，访问令牌过期前会自动刷新；登录成功后会从
Kimi managed `/models` 拉取模型并更新 `~/.kkagent/config.toml`（使用 `--config` 时更新指定文件）。

TUI 模式下日志**不会**打印到屏幕（避免破坏布局），而是追加写入：

```text
~/.kkagent/kkagent.log
```

### 独立 Server 模式

```bash
./target/release/kkagent server --listen /tmp/kkagent.sock
./target/release/kkagent server --http 127.0.0.1:8787 --http-token "$KKAGENT_HTTP_TOKEN"
```

直接 HTTP 文件写和 terminal API 默认关闭；只有确实需要时才增加
`--allow-fs-write-api` / `--allow-terminal-api`。服务提供 `/api/v1/health`、
`/api/v1/ready`、`/api/v1/metrics` 和带序号的事件回放。

HTTP/WS API 始终启用认证：优先使用 `Authorization: Bearer <token>`，也兼容
`?token=<token>`。如果未传 `--http-token` 且环境变量中没有
`KKAGENT_HTTP_TOKEN`，服务会为本次进程生成高熵 token 并打印到 stderr。

**TUI ↔ Server 配对：**

| 启动方式 | 配对关系 | 退出行为 |
|---------|---------|---------|
| 默认 `kkagent`（无 subcommand） | 进程内 memory duplex，**1 个 TUI ↔ 1 个 server task** | Ctrl-C / `/exit` 退出 TUI 时会 `abort` 配对的 server task；不影响其他 `kkagent` 进程 |
| `kkagent server` | 独立进程，UDS 监听；可被多个客户端连接（多 session） | 只有你停掉这个 server 进程它才退出；**不会**随某个 TUI 一起退出 |

默认 TUI **不会**自动连到残留的 `server.sock`。若感觉「连上了旧对话」，多半是 `--resume` / 历史 transcript，或另开了一个仍在跑的 `kkagent server`，而不是默认 TUI 跨进程复用。

需要复用显式启动的服务时，TUI 和非交互模式均可使用
`--connect ~/.kkagent/server.sock`。Unix 使用 domain socket；Windows 的同一路径是仅指向
loopback 随机端口的本地端点文件。默认 TUI 仍使用进程内 memory RPC。

### Node.js SDK

`sdk/node` 用于让 Node.js 后台、自动化脚本或编辑器扩展控制已经启动的
`kkagent server`。它通过 HTTP 创建 Session、发送任务，并通过 WebSocket 接收 Agent
回复与工具执行事件；它本身不在 Node.js 中运行模型或工具。

```js
import { KkagentClient } from "./sdk/node/src/index.js";

const client = new KkagentClient({
  baseUrl: "http://127.0.0.1:8787",
  token: process.env.KKAGENT_HTTP_TOKEN,
});

const session = await client.createSession("/absolute/path/to/project", "代码检查");
await client.postMessage(session.session_id, "运行测试并总结问题");
client.connectEvents((event) => console.log(event));
```

完整的启动步骤、API、事件订阅、安全说明和当前能力边界见
[Node.js SDK 文档](sdk/node/README.md)。

---

## 4. 怎么测试

### 单元 / 集成测试（不依赖外网）

```bash
cargo test --workspace --all-targets
# 或
make test
```

当前主要覆盖：

- LLM 方言与流终止校验（Anthropic/OpenAI Responses/Kimi/Google）
- 工具安全边界、取消、进程树、输出上限与敏感文件保护
- 权限链、并行调度、panic 隔离、自动压缩与 SQLite transcript 原子持久化
- RPC/HTTP/WS/ACP、认证、会话并发、文件边界和本地 IPC
- Kimi OAuth 刷新、私有凭据存储、视频上传和模型配置

期望：全部 `ok`，无 FAILED。

### Lint / 格式

```bash
make fmt
make lint   # clippy -D warnings
```

---

## 5. 怎么验证（实网烟雾测试）

前提：`config.toml` 里 `base_url` / `api_key` / `default_model` 可用。  
### A. 纯对话

```bash
./target/release/kkagent --config ~/.kkagent/config.toml -p "Say hello in one sentence"
```

**通过标准**：stdout 有一句正常回复；进程退出码 0。

### B. 读文件工具

```bash
./target/release/kkagent --config ~/.kkagent/config.toml -y \
  -p "Read ./Cargo.toml and tell me how many workspace members there are"
```

**通过标准**：回复给出的数量与根目录 `Cargo.toml` 的 `workspace.members` 一致。

### C. Shell 工具

```bash
./target/release/kkagent --config ~/.kkagent/config.toml -y \
  -p "Run 'ls crates/' and list the crate names"
```

**通过标准**：列出 `crates/` 下各 crate 名。

### D. 写文件工具

```bash
./target/release/kkagent --config ~/.kkagent/config.toml -y \
  -p "Create /tmp/kkagent_smoke.txt with content Hello kkagent"

cat /tmp/kkagent_smoke.txt
# 期望：Hello kkagent
```

### E. 带日志确认工具链路

```bash
RUST_LOG=kkagent_core=info,kkagent_llm=debug \
  ./target/release/kkagent --config ~/.kkagent/config.toml -y \
  -p "Read ./Cargo.toml" 2> /tmp/kkagent_smoke.log

grep -E "Tool use|Permission|Turn completed|LLM response status" /tmp/kkagent_smoke.log
```

**通过标准**日志中大致出现：

1. `LLM response status: 200 OK`
2. `Tool use collected: Read`
3. `Permission for Read: Approve`（`-y` / yolo）
4. `Recursing into next turn` → `Turn completed`

### 常见失败

| 现象 | 原因 | 处理 |
|------|------|------|
| HTTP 卡住无响应 | 上游代理 / HTTP2 | 客户端已强制 HTTP/1.1；检查网络与代理 |
| `400` max tokens 超限 | `max_output_size` 过大 | 设为 `16384` 或更小 |
| 无工具调用 | 模型未开 `tool_use` / 权限卡住 | `capabilities` 加 `tool_use`；用 `-y` |
| 配置未生效 | 写了 `.env` 却没传 `--config` | 拷到 `~/.kkagent/config.toml` 或显式 `--config` |
| `Model '...' not found` | `default_model` 别名在 `[models."..."]` 里不存在 | 补上对应 `[models."别名"]`，或把 `default_model` 改成已有别名 |

---

## 6. 仓库结构（简表）

```
crates/
  kkagent/           # CLI 入口
  kkagent-protocol/  # RPC 帧、权限、Goal、Subagent
  kkagent-rpc/       # NDJSON RPC + memory/UDS transport
  kkagent-config/    # TOML 配置
  kkagent-llm/       # Anthropic 兼容流式
  kkagent-core/      # Agent loop / 权限 / session / transcript
  kkagent-tools/     # 内置工具
  kkagent-mcp/       # MCP / Skills / Hooks
  kkagent-client/    # 客户端 facade
  kkagent-tui/       # ratatui TUI
examples/
  config.example.toml
```

---

## 7. 一键自检清单

```bash
# 1) 编译
cargo build --release

# 2) 单测
cargo test

# 3) 实网（改成你的 config）
./target/release/kkagent --config ~/.kkagent/config.toml -y \
  -p "Read ./Cargo.toml and count workspace members"
```

三项都过，才能进入目标平台的发布候选验证；正式发布还应以 CI 的全平台矩阵通过为准。
