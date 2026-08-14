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
| `fallback_model` | string | 无 | 全局 fallback 模型别名；主模型耗尽单步重试后使用，必须存在于 `models`。 |
| `secondary_model` | string | 无 | 可选的辅助模型别名。 |
| `default_permission_mode` | string | `manual` | `manual`、`yolo` 或 `auto`。 |
| `default_plan_mode` | bool | `false` | 新会话是否以 Plan 模式开始。 |
| `merge_all_available_skills` | bool | `false` | 把全部 Skill 正文合入初始上下文；默认只注入目录并按需加载。 |
| `extra_skill_dirs` | string[] | `[]` | 额外 Skill 根目录；相对路径基于 Server 启动目录。 |
| `telemetry` | bool | `false` | 是否启用云遥测发送。 |
| `trusted_workspaces` | string[] | `[]` | 预配置的绝对工作区；TUI 也会通过首次进入信任弹窗维护配置旁的 trust sidecar。为空时，非交互 Server 仍只隐式信任启动目录。 |

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
| `base_url` | `http://` 或 `https://` URL；兼容端点可使用 `/v1`、`/v4` 等版本前缀，也可填写完整资源 endpoint。 |
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
# 以下实验选项默认关闭，只建议为需要兼容处理的模型单独开启。
# experimental_adaptive_thinking = true
# experimental_visible_empty_retries = 1
```

`provider` 必须引用已有 Provider。`max_context_size` 和 `max_output_size` 参与上下文预算。`tool_use` 控制是否向模型发送工具定义；`image_in`、`video_in`、`audio_in` 声明多模态输入能力；`support_efforts` 和 `default_effort` 描述可用推理强度。

`experimental_adaptive_thinking` 仅影响 Anthropic 请求：开启后发送 `thinking.type = "adaptive"`，并通过 `output_config.effort` 转发当前 thinking effort。`experimental_visible_empty_retries` 指定 tool result 后遇到“无正文且无新 tool call”的成功响应时最多重试几次；thinking-only 也属于这种响应。重试只重新请求模型，不会再次执行已经完成的工具。两个选项都按模型配置，未设置时保持原有行为。

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

## TUI

```toml
[ui]
high_contrast = false
reduce_motion = false
check_updates = true
```

`check_updates` 默认启用：TUI 首屏不会等待网络，而是在后台查询 `Ken-u/kkagent` 的最新 GitHub Release；成功结果缓存 24 小时，失败结果一小时后重试。发现新版本时只显示 Release 链接和 `kkagent-update`（Windows 为 `kkagent-update.ps1`）提示，不会自动下载或替换程序。设为 `false` 可完全关闭检查。

## Agent 循环

```toml
[loop_control]
max_attempts_per_step = 10
rate_limit_retry_base_seconds = 5
reserved_context_size = 50000
max_steps_per_turn = 0
auto_compact = true
compact_keep_last = 8
# Early trigger / block ratios (kimi defaults). Optional.
# compact_trigger_ratio = 0.85
# compact_block_ratio = 0.85
# compact_max_overflow_attempts = 3
token_counting = "measured+estimated"
```

`max_steps_per_turn` 不设置或设为 `0` 时不限制单轮 Agent 步数；只有正整数才会启用上限。

`max_attempts_per_step` 是每个模型在单步推理中的尝试总数（包含首次请求）。配置顶层 `fallback_model` 后，主模型先完成这里指定的全部正常重试；仍失败时自动切换到 fallback，并重新获得同样的尝试次数。fallback 成功不会永久改变会话主模型，下一步仍从主模型开始；主模型与 fallback 相同则自动跳过 fallback。只有两个阶段都失败才向上返回错误。

TUI 使用 `/model` 切换到全局 `fallback_model` 时，会要求为本次会话选择“禁用 fallback”或指定另一个 fallback 模型。该选择写入会话记录，恢复会话时继续生效；以后切换到其他主模型会恢复继承全局 fallback。

当 LLM 返回 429 且未提供 `Retry-After` 时，`rate_limit_retry_base_seconds` 控制指数退避的基础时间，默认依次等待 5、10、20 秒。若服务端提供等待时间，则优先使用服务端值（最长 300 秒）。

`token_counting` 可取 `measured+estimated`、`measured` 或 `estimated`。上下文达到 `compact_trigger_ratio`（默认 85%）或逼近 `reserved_context_size` 预留时，`auto_compact` 会用 LLM 摘要压缩历史，并按 token 预算保留用户消息（头+尾）；assistant/tool 交换由摘要覆盖，避免 toolcall 配对 400。手动 `/compact` 在 turn 进行中会被拒绝。

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

进程资源限制与文件系统隔离彼此独立，因此 `mode = "disabled"` 仍会应用非零限制：
Linux 支持内存、CPU 和进程数，macOS 支持 CPU，Windows Job Object 支持内存和进程数。
这可避免关闭文件隔离时编译器或脚本直接耗尽主机资源；只有显式设为 `0` 的限制项才表示不限。

`auto` 在 Linux/macOS 选择 `workspace`，在 Windows 选择 `process`。Linux 的工作区模式要求 PATH 中存在 `bwrap`，用 user/mount/PID 等 namespace、只读系统目录、独立 `/tmp` 和可选 network namespace 隔离命令；macOS 使用系统 Seatbelt，默认拒绝用户主目录并重新开放当前 workspace；Windows 的 `process` 模式用 Job Object 限制进程树、内存和进程数。显式选择平台不支持的 `workspace` 会拒绝执行，不会静默降级。

`extra_read_paths`/`extra_write_paths` 必须已经存在。`disabled` 会跳过 Bash 的文件/网络隔离和 Git 环境改写，但保留上述非零资源限制，只建议用于受控容器或 VM 内排障。`kkagent --disable-sandbox` 可仅对当前进程关闭文件隔离且不写回配置；该参数不能与 `--connect` 同时使用。HTTP terminal 是单独的显式管理接口，不继承 Bash sandbox。

### 工作区与 Git 信任

TUI 首次进入一个尚未记录的工作区时，会先确认工作区信任，再独立检查两类权限：

- `.git`、gitfile、common-dir、objects/alternates 指向工作区之外的 Git 元数据；AOSP checkout 会按上层 `.repo` 根目录聚合为一次读写确认。
- `~/.gitconfig`、`$XDG_CONFIG_HOME/git/config`、递归 include、AOSP `~/.repoconfig/config` 以及全局 ignore/attributes；这些路径只按只读权限开放。

选择结果写入当前配置文件旁的 `<config-file>.trust.toml`。默认配置对应 `~/.kkagent/config.toml.trust.toml`，使用 `--config /path/team.toml` 时对应 `/path/team.toml.trust.toml`。sidecar 不保存 Git 配置值、token 或凭据，只保存规范化路径、授权结果和检测到的能力类别。

未授权全局配置时，Bash 为 Git 注入隔离配置，跳过 global/system config 和默认的全局 ignore/attributes；仓库自身的 `.git/config`、`.gitignore` 和 `.gitattributes` 不受影响。授权后，已确认的配置按 Git 原有 system/global 层级只读加载，仓库本地配置仍保持更高优先级。`.ssh`、`.git-credentials`、`.gnupg` 和系统钥匙串不会因为这一授权自动加入文件沙箱。

全局 Git 配置可以包含 `credential.helper`、shell alias、`core.hooksPath`、HTTP header 或其他 include。弹窗只显示风险类别和文件路径，不显示配置值。删除对应 trust sidecar 可重新触发完整审核。

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
[services.web_search]
provider = "searxng" # searxng | brave | custom
base_url = "http://127.0.0.1:8080/search" # 完整搜索 endpoint，不会自动再拼 /v1/search
api_key_env = "BRAVE_API_KEY" # 优先于 inline api_key
timeout_ms = 15000
default_limit = 5

# 可选：FetchURL 外部代理；未配置时走直接 HTTP GET + SSRF 校验
# [services.web_fetch]
# base_url = "https://example.invalid/fetch"
# api_key_env = "WEB_FETCH_API_KEY"
# timeout_ms = 30000
```

`web_search` 为内置 `WebSearch` 提供后端；未配置时不注册该工具。`FetchURL` 在未配置 `web_fetch` 时仍可直接抓取公网页面。旧的 `[services.moonshot_search]` / `[services.moonshot_fetch]` 仍可读一次并给出迁移提示。

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
