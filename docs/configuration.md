# 配置参考

## 加载顺序

kkagent 读取一份 TOML 配置：优先使用 `--config <path>`，否则读取 `~/.kkagent/config.toml`。配置不存在时，交互终端会启动首次运行向导；非交互运行会提示先执行 `kkagent init`。环境变量会覆盖部分字段。启动时会校验默认模型、Provider 引用、URL、权限模式和数值范围。

常用维护命令：

```bash
kkagent config show
kkagent config get sandbox.mode
kkagent config set sandbox.network false
kkagent config preset safe
```

`config show/get` 输出的是应用环境变量覆盖后的有效配置，并递归隐藏 API key、token、secret、header 和 MCP env。`config set` 的值使用 TOML 语法，例如字符串需要写成 `\"manual\"`；它通过临时文件原子替换并在 Unix 上使用 `0600` 权限。

## 顶层字段

| 字段 | 类型 | 默认值 | 说明 |
|---|---:|---:|---|
| `default_model` | string | 必填 | 默认模型别名，必须存在于 `models`。 |
| `secondary_model` | string | 无 | 可选的辅助模型别名。 |
| `default_permission_mode` | string | `manual` | `manual`、`yolo` 或 `auto`。 |
| `default_plan_mode` | bool | `false` | 新会话是否以 Plan 模式开始。 |
| `merge_all_available_skills` | bool | `false` | 把全部 Skill 正文合入初始上下文；默认只注入目录并按需加载。 |
| `extra_skill_dirs` | string[] | `[]` | 额外 Skill 根目录；相对路径基于 Server 启动目录。 |
| `telemetry` | bool | `false` | 是否启用云遥测发送。 |
| `trusted_workspaces` | string[] | `[]` | HTTP 文件和终端操作允许访问的绝对工作区；为空时只信任 Server 启动目录。 |

## Provider

```toml
[providers.openai]
type = "openai-responses"
api_key = "sk-..."
base_url = "https://api.openai.com/v1"
custom_headers = { "X-Organization" = "example" }
```

| 字段 | 说明 |
|---|---|
| `type` | `anthropic`、`kimi`、`openai`/`openai-chat`、`openai-responses`、`google`/`google-genai`/`gemini`。下划线别名也被接受。 |
| `api_key` | Provider 密钥。不要提交到 Git。 |
| `base_url` | `http://` 或 `https://` URL；兼容端点可带或不带 `/v1`。 |
| `custom_headers` | 发送给上游的附加 HTTP Header。 |
| `oauth` | 托管 OAuth 配置，通常由 `kkagent auth login` 管理。 |

OAuth 子项：`storage` 默认 `file`，`key` 默认 `kimi-code`，`oauth_host` 可覆盖登录服务地址。

## Model

```toml
[models."openai/coding"]
provider = "openai"
model = "upstream-model-id"
max_context_size = 200000
max_output_size = 16384
capabilities = ["tool_use", "thinking", "image_in"]
display_name = "Coding model"
support_efforts = ["low", "medium", "high"]
default_effort = "medium"
```

`provider` 必须引用已有 Provider。`max_context_size` 和 `max_output_size` 参与上下文预算。`tool_use` 控制是否向模型发送工具定义；`image_in`、`video_in`、`audio_in` 声明多模态输入能力；`support_efforts` 和 `default_effort` 描述可用推理强度。

## 图片

```toml
[image]
max_edge_px = 2000
read_byte_budget = 262144
```

`max_edge_px` 是所有普通图片入口（路径附件、粘贴、工具和 MCP 结果）的最长边限制，范围为 1–16384。`read_byte_budget` 是 Agent 通过 `ReadMediaFile` 或 MCP 自行获得单张图片时的编码预算，默认 256 KiB、最大 20 MiB；`region` 与 `full_resolution` 使用 5 MiB Provider 安全上限。环境变量 `KKAGENT_IMAGE_MAX_EDGE_PX` / `KIMI_IMAGE_MAX_EDGE_PX` 和 `KKAGENT_IMAGE_READ_BYTE_BUDGET` / `KIMI_IMAGE_READ_BYTE_BUDGET` 可覆盖配置。

## Thinking

```toml
[thinking]
enabled = true
effort = "high"
keep = "all"
```

`keep` 是 Provider 兼容字符串。并非所有端点都支持 thinking 与 tools 同时使用；遇到 400 响应时可先删除 `keep` 并关闭 thinking 验证。

## Agent 循环

```toml
[loop_control]
max_attempts_per_step = 10
reserved_context_size = 50000
max_steps_per_turn = 64
auto_compact = true
compact_keep_last = 8
token_counting = "measured+estimated"
```

`token_counting` 可取 `measured+estimated`、`measured` 或 `estimated`。上下文逼近上限时，`auto_compact` 会压缩旧历史。

## 后台任务

```toml
[background]
max_running_tasks = 4
keep_alive_on_exit = false
bash_auto_background_on_timeout = true
bash_task_timeout_s = 120
approval_timeout_s = 900
```

`max_running_tasks` 控制 Agent 后台任务并发；`approval_timeout_s` 到期后按拒绝处理，范围在运行时限制为 1 秒到 24 小时。

## 系统隔离

```toml
[sandbox]
mode = "auto"       # auto | workspace | process | disabled
network = true
memory_mb = 4096
cpu_seconds = 600
max_processes = 128
extra_read_paths = []
extra_write_paths = []
```

`auto` 在 Linux/macOS 选择 `workspace`，在 Windows 选择 `process`。Linux 的工作区模式要求 PATH 中存在 `bwrap`，用 user/mount/PID 等 namespace、只读系统目录、独立 `/tmp` 和可选 network namespace 隔离命令；macOS 使用系统 Seatbelt，默认拒绝用户主目录并重新开放当前 workspace；Windows 的 `process` 模式用 Job Object 限制进程树、内存和进程数。显式选择平台不支持的 `workspace` 会拒绝执行，不会静默降级。

`extra_read_paths`/`extra_write_paths` 必须已经存在。`disabled` 只建议用于受控容器内排障；HTTP terminal 是单独的显式管理接口，不继承 Bash sandbox。

## 权限规则

```toml
[[permission.rules]]
decision = "allow"
pattern = "Read"
scope = "user"

[[permission.rules]]
decision = "deny"
pattern = "Bash(rm *)"
scope = "workspace"
```

`decision` 为 `allow`、`deny` 或 `ask`。`pattern` 支持工具名、`*`、MCP 工具通配和 `Tool(argument-pattern)` 形式。危险命令的硬阻断不会被允许规则绕过。

## Hooks

```toml
[[hooks]]
event = "pre_tool_call"
matcher = "Bash"
command = "/absolute/path/to/check.sh"
timeout = 5
```

事件支持 `pre_tool_call`、`post_tool_call`、`session_start`、`session_end`、`turn_start`、`turn_end`、`notification`。`matcher` 支持精确工具名和 `*` 通配；带 matcher 的非工具事件不会触发。JSON Hook 格式见[扩展机制](extensions.md)。

## 服务

```toml
[services.moonshot_search]
base_url = "https://example.invalid/search"
api_key = "..."

[services.moonshot_fetch]
base_url = "https://example.invalid/fetch"
api_key = "..."
```

`moonshot_search` 为内置 `WebSearch` 提供后端。`FetchURL` 也有受 SSRF 防护约束的直接抓取路径。

## MCP Server

stdio：

```toml
[mcp_servers.filesystem]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/absolute/path"]
env = { "LOG_LEVEL" = "warn" }
timeout_ms = 30000
```

远程 HTTP：

```toml
[mcp_servers.remote]
type = "streamable-http"
url = "https://mcp.example.com/mcp"
headers = { "Authorization" = "Bearer ..." }
timeout_ms = 30000

[mcp_servers.remote.oauth]
enabled = true
scopes = ["tools.read", "tools.call"]
client_label = "kkagent"
```

远程类型支持 `sse`、`http` 和 `streamable-http`。OAuth 还可配置 `client_id`、`client_secret`、`redirect_uri`。

## 环境变量覆盖

启动时会读取当前工作区的 `.env`，但只导入 `KKAGENT_*` 和下表中的模型 Provider 密钥，并且不会覆盖父进程已经设置的变量。标准格式为 `KEY=value`。旧版本把 TOML 配置命名为 `.env` 的工作区不会被当成 dotenv 解析，仍可使用 `kkagent --config .env ...`。

| 环境变量 | 用途 |
|---|---|
| `KKAGENT_DEFAULT_MODEL` | 默认模型。 |
| `KKAGENT_SECONDARY_MODEL` | 辅助模型。 |
| `KKAGENT_PERMISSION_MODE` | 默认权限模式。 |
| `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`KIMI_API_KEY`、`GOOGLE_API_KEY` | 对应 Provider 密钥。 |
| `KKAGENT_MOONSHOT_SEARCH_URL` | 搜索服务地址。 |
| `KKAGENT_MOONSHOT_SEARCH_KEY` 或 `MOONSHOT_API_KEY` | 搜索服务密钥。 |
| `KKAGENT_HTTP_TOKEN` | Agent Server HTTP/WS Bearer token。 |
| `KKAGENT_HTTP_READ_TOKEN` | 只读 API token。 |
| `KKAGENT_HTTP_WRITE_TOKEN` | read + 非 terminal 写操作 token。 |
| `KKAGENT_HTTP_TERMINAL_TOKEN` | read + terminal token。 |
| `KKAGENT_ALLOW_IN_MEMORY_TRANSCRIPTS` | 显式允许 transcript 非持久化降级；readiness 会保持失败。 |
| `KKAGENT_TELEMETRY_ENDPOINT`、`KKAGENT_TELEMETRY_CLOUD` | 云遥测地址和开关。 |
| `RUST_LOG` | 日志过滤器。 |

完整可复制模板见 [`examples/config.example.toml`](../examples/config.example.toml)。
