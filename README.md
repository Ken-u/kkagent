# kkagent

用 Rust 实现的终端 Coding Agent（接近完整复刻 kimi-code CLI）。  
TUI 与 Agent Server 可分离，中间走 RPC（当前默认进程内 memory transport；也支持独立 `server` 模式）。

- 低内存、高性能
- 支持 Win / macOS / Linux（x86_64 / arm64）
- 权限模式：`manual` / `yolo` / `auto`
- 内置工具：Read / Write / Edit / Grep / Glob / Bash / TodoList / Goal / Task
- 会话持久化：`~/.kkagent/transcripts.db`
- MCP / Skills / Hooks（配置驱动）

---

## 1. 怎么编译

### 依赖

- Rust **1.75+**（建议 1.88+；本仓库在 1.95 验证过）
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
# 产物：target/release/kkagent  （当前约 8MB）
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

# 或一次性打到 target/release-dist/
make dist
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
| `providers.*.type` | 当前走 Anthropic Messages：`anthropic` / `openai` / `kimi` 都会走同一套流式客户端 |
| `providers.*.base_url` | 不要带尾斜杠；请求会打到 `{base_url}/v1/messages` |
| `providers.*.api_key` | `x-api-key` |
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
| `/model` | 查看/切换模型（切换能力仍在完善） |
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

TUI 模式下日志**不会**打印到屏幕（避免破坏布局），而是追加写入：

```text
~/.kkagent/kkagent.log
```

### 独立 Server 模式

```bash
./target/release/kkagent server --listen /tmp/kkagent.sock
```

**TUI ↔ Server 配对：**

| 启动方式 | 配对关系 | 退出行为 |
|---------|---------|---------|
| 默认 `kkagent`（无 subcommand） | 进程内 memory duplex，**1 个 TUI ↔ 1 个 server task** | Ctrl-C / `/exit` 退出 TUI 时会 `abort` 配对的 server task；不影响其他 `kkagent` 进程 |
| `kkagent server` | 独立进程，UDS 监听；可被多个客户端连接（多 session） | 只有你停掉这个 server 进程它才退出；**不会**随某个 TUI 一起退出 |

默认 TUI **不会**自动连到残留的 `server.sock`。若感觉「连上了旧对话」，多半是 `--resume` / 历史 transcript，或另开了一个仍在跑的 `kkagent server`，而不是默认 TUI 跨进程复用。

（TUI `--connect` 对接外部 server 的能力预留；默认 TUI 使用进程内 memory RPC。）

---

## 4. 怎么测试

### 单元 / 集成测试（不依赖外网）

```bash
cargo test
# 或
make test
```

当前主要覆盖：

- 权限链（manual / yolo / auto、敏感文件等）— `kkagent-core`
- SQLite transcript（create / append / compact / archive）— `kkagent-core`
- RPC codec — `kkagent-rpc`

期望：全部 `ok`，无 FAILED。

### Lint / 格式（可选）

```bash
make fmt
make lint   # clippy -D warnings，可能因既有 warning 未全清而失败
```

---

## 5. 怎么验证（实网烟雾测试）

前提：`config.toml` 里 `base_url` / `api_key` / `default_model` 可用。  
推荐模型：`claude-opus-4-8`（短任务稳定；`claude-opus-4-6` 当前上游可能有问题）。

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

**通过标准**：回复里出现约 **10** 个 workspace members（`kkagent`、`kkagent-protocol` …）。

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

三项都过，即可认为当前版本可用。
