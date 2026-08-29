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
| `secondary_model` | string | 无 | 全局辅助模型别名。子 Agent 在未配置 `[subagent.default_models]` 对应 profile、且工具调用未显式传 `model` 时回退到此模型；也用作未设置 `compaction_model` 时的压缩摘要候选。必须存在于 `models`。 |
| `fast_model` | string | 无 | 快速/廉价模型别名。符号 `fast`（工具 `model` 参数与 `[subagent.default_models]` 值）优先解析到此模型；未配置时回退到 `secondary_model`，再回退到 `default_model`。必须存在于 `models`。 |
| `compaction_model` | string | 无 | 专用于 `/compact` 历史压缩摘要的模型别名，必须存在于 `models`。设置后优先级最高，高于 `secondary_model`、session 当前模型和 `default_model`。也可用环境变量 `KKAGENT_COMPACTION_MODEL` 覆盖。 |
| `default_permission_mode` | string | `manual` | `manual`、`yolo` 或 `auto`。 |
| `default_plan_mode` | bool | `false` | 新会话是否以 Plan 模式开始。 |
| `merge_all_available_skills` | bool | `false` | 把全部 Skill 正文合入初始上下文；默认只注入目录并按需加载。 |
| `extra_skill_dirs` | string[] | `[]` | 额外 Skill 根目录；相对路径基于 Server 启动目录。 |
| `plugin_marketplace` | string | 无 | 默认 KK plugin marketplace JSON 的本地路径、`file://` URL 或 HTTP(S) URL。环境变量 `KKAGENT_PLUGIN_MARKETPLACE_URL` 优先。 |
| `plugin_marketplaces` | array | `[]` | 额外 marketplace 列表。元素可以是 URL/路径字符串，或 `{ name = "...", source = "..." }`。与 `plugin_marketplace` 合并去重后全部出现在 `/plugins` 市场列表中（配置项不可移除）。 |
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
| `first_token_timeout_ms` | Provider 级流式首字超时默认值（毫秒）；模型级可覆盖；`0` 禁用。 |

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
# first_token_timeout_ms = 60000  # 等首字；0 禁用；未设置继承 provider / 默认 60s
# 以下实验选项默认关闭，只建议为需要兼容处理的模型单独开启。
# experimental_adaptive_thinking = true
# experimental_visible_empty_retries = 1
# experimental_bad_toolcall_auto_retries = 2
# experimental_vision_proxy = true  # 让该模型为非 vision 主模型充当多模态读图代理
```

`provider` 必须引用已有 Provider。`max_context_size` 和 `max_output_size` 参与上下文预算。`tool_use` 控制是否向模型发送工具定义；`image_in`、`video_in`、`audio_in` 声明多模态输入能力；`support_efforts` 和 `default_effort` 描述可用推理强度（`none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`，如 GPT-5.6 支持 `none`/`low`/`medium`/`high`/`xhigh`/`max`）。`default_effort` 必须列在 `support_efforts` 中（若后者非空）。

`first_token_timeout_ms` 控制流式请求等待第一个有效内容 chunk（文本 / thinking / tool_use）的超时。优先级为：模型级 → Provider 级 → 默认 `60000`（60 秒）。设为 `0` 表示禁用（退化为仅受 HTTP 总超时 300s 约束）。≥ 300s 的配置会被 clamp 到 290s。超时后请求中断；若配置了 `fallback_model`，Agent loop 会按既有重试策略切换。

`experimental_adaptive_thinking` 仅影响 Anthropic 请求：开启后发送 `thinking.type = "adaptive"`，并通过 `output_config.effort` 转发当前 thinking effort。`experimental_visible_empty_retries` 指定 tool result 后遇到“无正文且无新 tool call”的成功响应时最多重试几次；thinking-only 也属于这种响应。重试只重新请求模型，不会再次执行已经完成的工具。`experimental_bad_toolcall_auto_retries` 指定模型返回的 tool call 参数不是合法 JSON 对象、被服务端以 HTTP 400 拒绝时，自动回滚该条 assistant 小步骤并重新请求模型的次数；回滚只丢弃这一步及其后的 tool result，不会反向恢复已发生的工具副作用。重试次数耗尽后仍按原有行为停下来等待 `continue`。三个选项都按模型配置，未设置时保持原有行为。

`experimental_vision_proxy` 让一个**具备图像输入能力**（`image_in` 等）的模型为非 vision 主模型充当多模态读图代理。配置时在某个 vision 模型上设置该 flag，整个配置最多一个。当主模型声明无图像输入能力时，Agent loop 在每轮请求前把消息中的图像块替换为该代理模型生成的文字描述（逐字转写文字、描述布局与 UI、报告图表数据），**替换是永久性的**——session 历史中的 base64 图片块被丢弃，只保留描述文本，节省内存和上下文预算。原始文件路径仍保留在消息文本中（用户 `<image-attached>` 标记或 ReadMediaFile 工具结果），切回 vision 模型后模型可重新调用 ReadMediaFile 读取原图。描述按图像 SHA-256 缓存，重复发送同一张图只调用一次代理。非 vision 主模型此前被隐藏的 `ReadMediaFile` 工具也会重新可见。

该模型本身也可以作为主模型使用（例如设为 `default_model` 或通过 `/model` 切换过去）。此时它有 vision 能力，`engaged()` 返回 false，代理替换不触发，主模型直接读原图，proxy flag 无副作用。

官方 OpenAI endpoint 会自动使用稳定的 `prompt_cache_key`。自定义 OpenAI-compatible endpoint 为避免未知字段导致 HTTP 400，默认不发送；确认兼容后可在模型的 `capabilities` 中加入 `prompt_cache_key` 开启。

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

`effort` 可取 `none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`（按模型支持情况）。优先级：运行时 `/effort` 命令（写入本字段）> 模型级 `default_effort` > 协议默认值。对 OpenAI Responses API 转发为 `reasoning.effort`，对 Chat Completions 转发为 `reasoning_effort`，对 Anthropic（需 `experimental_adaptive_thinking`）转发为 `output_config.effort`。未显式指定 effort 时，Responses API 按 `budget_tokens` 推导：≥16000 → `high`，≥4000 → `medium`，否则 `low`。

`keep` 是 Provider 兼容字符串。并非所有端点都支持 thinking 与 tools 同时使用；遇到 400 响应时可先删除 `keep` 并关闭 thinking 验证。

## TUI

```toml
[ui]
high_contrast = false
reduce_motion = false
check_updates = true
# experimental_smart_at_complete = true  # 递归模糊 `@` 补全；默认关闭，按级目录补全
mouse_mode = "capture"                   # 实验性："capture"（默认）或 "off"
```

`check_updates` 默认启用：TUI 首屏不会等待网络，而是在后台查询 `Ken-u/kkagent` 的最新 GitHub Release；成功结果缓存 24 小时，失败结果一小时后重试。发现新版本时只显示 Release 链接和 `kkagent-update`（Windows 为 `kkagent-update.ps1`）提示，不会自动下载或替换程序。设为 `false` 可完全关闭检查。

`@` 路径补全默认只列出**当前一级**目录/文件（输入前缀过滤，选中目录后以 `/` 结尾并继续下一级）。需要原来的递归模糊搜索时，设置 `experimental_smart_at_complete = true`。

`mouse_mode`（实验性）控制 TUI 是否接管鼠标滚轮：`capture`（默认）在应用内滚动聊天记录并支持拖选；`off` 完全关闭鼠标上报，滚轮交给终端原生处理。适用于部分 Windows SSH 客户端（老版 Xshell / PuTTY / SecureCRT 等）对 SGR 鼠标协议支持不全的场景——它们会把滚轮误解为方向键序列，导致在输入框里上下翻历史而不是滚动内容。环境变量 `KKAGENT_MOUSE_MODE=off` 可临时覆盖此配置（`off`/`none`/`alternate-scroll` 均视为关闭）。

## Agent 循环

```toml
[loop_control]
max_attempts_per_step = 10
rate_limit_retry_base_seconds = 5
retry_base_seconds = 1
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

其余可重试失败（流式断连、空响应、5xx，以及自建推理服务常见的 KV-cache 容量准入拒绝——HTTP 400 报 `max_completion_tokens is too large`，其上限值随池子空闲动态变化，并非配置错误）统一走 `retry_base_seconds` 的指数退避，默认依次等待 1、2、4 秒（封顶 32 秒）。服务端提供的等待时间同样优先。遇到 KV 池拥塞报错时，优先调大 `max_attempts_per_step` 拉长总重试窗口，而不是调小 `max_output_size`。

`token_counting` 可取 `measured+estimated`、`measured` 或 `estimated`。上下文达到 `compact_trigger_ratio`（默认 85%）或逼近 `reserved_context_size` 预留时，`auto_compact` 会用 LLM 摘要压缩历史，并按 token 预算保留用户消息（头+尾）；assistant/tool 交换由摘要覆盖，避免 toolcall 配对 400。手动 `/compact` 在 turn 进行中会被拒绝。

## 子 Agent（`[subagent]`）

```toml
[subagent]
max_depth = 2
max_concurrent = 4

[subagent.default_models]
explore = "current"
coder = "default"
general = "fast"
# fallback = "default"  # 未单独列出的 profile 回退
```

| 字段 | 类型 | 默认值 | 说明 |
|---|---:|---:|---|
| `max_depth` | u32 | `2` | 子 Agent 嵌套深度上限（运行时夹到 `1..=4`）。`1` 表示子 Agent 不能再委派。 |
| `max_concurrent` | usize | `4` | 每个 manager 同时运行的子 Agent 上限（至少为 1）。 |
| `default_models` | map | `{}` | 按 profile 配置默认模型。内置 profile：`explore` / `coder` / `general`（`agent` 视为 `general`）。键名大小写不敏感；可选键 `fallback` 作为未单独配置 profile 的回退。 |

`default_models` 的值可以是 `[models]` 中的真实别名，或以下符号别名（大小写不敏感）：

| 符号 | 含义 |
|---|---|
| `current` | 父会话当前正在使用的模型（主模型失败切到 fallback 时为 fallback 模型）；不可用时回退到 `default_model`。 |
| `default` | 顶层 `default_model`。 |
| `fast` | 顶层 `fast_model`；未配置时回退 `secondary_model`，再回退 `default_model`。 |
| `secondary` | 顶层 `secondary_model`；未配置时回退到 `default_model`。 |

子 Agent 选用模型的优先级：工具调用显式 `model`（也可写上述符号）→ `[subagent.default_models]` 对应 profile → 顶层 `secondary_model` → `default_model`。

工具 schema 中的 `model` 参数是静态枚举 `default` / `fast` / `current`，LLM 无需感知真实模型别名即可选择档位；`secondary` 与真实别名仅在配置文件中支持。枚举是静态的，不会随 `[models]` 变化导致模型缓存失效。

校验规则：

- `current` / `default` / `fast` / `secondary` 是保留符号，`[models]` 中的别名不能使用这些名字（含大小写变体），否则校验报错。
- `[subagent.default_models]` 的键在归一化后（大小写折叠、`agent` → `general`）不得重复，否则校验报错。

## 后台任务

```toml
[background]
max_running_tasks = 4
keep_alive_on_exit = false
bash_auto_background_on_timeout = true
bash_task_timeout_s = 120
approval_timeout_s = 900
```

`max_running_tasks` 控制 Agent 后台任务并发；`approval_timeout_s` 到期后按拒绝处理，范围在运行时限制为 1 秒到 24 小时。`bash_auto_background_on_timeout` 控制前台 Bash 命令超时后是否自动转入后台（默认 `true`）。`bash_task_timeout_s` 控制前台 Bash 命令的默认超时秒数（默认 `120`，即 2 分钟），LLM 可通过 `timeout` 参数覆盖。

## 工具与超大工程

```toml
[tools]
path_guard_mode = "warn"          # warn | strict
sensitive_path_check = true
additional_dirs = []
dynamically_loaded_tools = true
# 覆盖默认重型目录跳过列表（Glob / Grep / `@` 补全共用）。
# 默认：["node_modules", "target", ".git", "out", ".repo"]
# heavy_dirs = ["out", "target", "node_modules", ".git", ".repo"]
# 在默认（或 heavy_dirs）之上追加：
# extra_heavy_dirs = ["bazel-out", "dist"]
```

`heavy_dirs` 一旦写出（含空数组）就会替换内置默认值；只想追加时用 `extra_heavy_dirs`。项目级 `.kkagent/config.toml` 也可写同样的 `[tools]` 字段，启动时与全局配置合并。显式定向到重型目录的搜索（例如 Glob `out/soong/**` 或 Grep `path = "out"`）仍会进入该目录。

## Standalone Server

```toml
[server]
# 无客户端且无 active turn 时，独立 server 自动退出的空闲秒数。
# 0 = 永不自动退出，需手动 `kkagent server stop`。
idle_timeout_secs = 1800
# true：`kk` 默认拉起/连接独立 server（Ctrl+B 可用）。
# false：退回进程内 server（旧行为，Ctrl+B 不可用）。
standalone = true
```

默认 `kk`（无参数）会连接 `~/.kkagent/server.sock` 上的独立 server；不存在时自动后台启动。TUI 退出不会杀死该 server。`~/.kkagent/active-session` 记录最近后台化的 session，下次启动自动 resume。

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

在 `disabled` 之外的模式中，进程资源限制与文件系统隔离彼此独立：即便文件隔离能力有限，非零限制仍然生效。
Linux 支持内存和 CPU，macOS 支持 CPU，Windows Job Object 支持内存和进程数。
`mode = "disabled"` 会完整跳过文件/网络隔离、Git 环境改写以及全部 OS 资源与 Job Object 限制（包括 `NO_NEW_PRIVS`），只建议在已有外层容器或 VM 隔离时使用；非 `disabled` 模式下只有显式设为 `0` 的限制项才表示不限。`max_processes` 仅在 Windows Job Object 上生效：Linux/macOS 的 `RLIMIT_NPROC` 按真实 UID 全局计数而非进程树，设置它会让桌面用户的所有 fork 失败。

`auto` 在 Linux/macOS 选择 `workspace`，在 Windows 选择 `process`。Linux 的工作区模式要求 PATH 中存在 `bwrap`，用 user/mount/PID 等 namespace、只读系统目录、独立 `/tmp` 和可选 network namespace 隔离命令；macOS 使用系统 Seatbelt，默认拒绝用户主目录并重新开放当前 workspace；Windows 的 `process` 模式用 Job Object 限制进程树、内存和进程数。显式选择平台不支持的 `workspace` 会拒绝执行，不会静默降级。

`extra_read_paths`/`extra_write_paths` 必须已经存在。`disabled` 会跳过 Bash 的文件/网络隔离、Git 环境改写和全部资源限制，只建议用于受控容器或 VM 内排障。`kkagent --disable-sandbox` 可仅对当前进程关闭上述全部隔离且不写回配置；该参数不能与 `--connect` 同时使用。HTTP terminal 是单独的显式管理接口，不继承 Bash sandbox。

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

## 插件（`[plugins]`）

```toml
[plugins]
extra_overridable_tools = ["Bash"]  # 允许插件 toolOverride 覆盖高危内置工具
```

`extra_overridable_tools`：在默认可覆盖清单（`Web`/`TaskOutput`/`Skill`/`Cron`/`ReadMediaFile`）之外，允许插件覆盖的内置工具名。`Bash`/`Edit`/`Write` 等高危工具必须在此显式列出才可被 `toolOverrides` 替换；`AskUserQuestion`/`EnterPlanMode`/`ExitPlanMode`/`Goal` 为守卫工具，无论如何配置都不可覆盖。完整 19 个内置工具的分级表见[扩展机制](extensions.md#插件-override)。注意该开关只管 `toolOverrides`（MCP 替换工具本体）；插件通过 `services` 段替换 `[services.web_search]`/`[services.web_fetch]` 后端不受此限制，也不需要任何 MCP server。

## 服务

`[services]` 为内置 `Web` 工具的两个动作提供可选后端：`action = "search"`（搜索）与 `action = "fetch"`（抓取网页正文）。`fetch` 不配置后端也能用（直接 HTTP GET + SSRF 校验）；`search` 必须配置 `[services.web_search]`，未配置时工具会返回错误并提示补配置。

### Web 搜索

```toml
[services.web_search]
provider = "searxng" # searxng | brave | custom，默认 searxng（大小写不敏感）
base_url = "http://127.0.0.1:8080/search" # 完整搜索 endpoint，不会自动再拼路径
api_key = "..."          # 二选一；api_key_env 优先
api_key_env = "BRAVE_API_KEY"
timeout_ms = 15000       # 默认 15000，运行时下限 1000
default_limit = 5        # 默认 5，实际生效范围 1..20
proxy = "auto"           # auto | none | system，默认 auto（见下）
```

#### 代理策略（`proxy`）

`Web` 工具的 HTTP 客户端默认遵循系统代理环境变量（`http_proxy` / `https_proxy` / `all_proxy`）。但当你用本地 provider（如 SearXNG 跑在 `127.0.0.1`）时，系统代理通常会劫持 loopback 请求——远端代理服务器无法回连你本机，导致连接被拒或超时。`proxy` 字段控制该 endpoint 的出站代理行为：

- `auto`（默认）：`base_url` 的 host 是 loopback（`localhost` / `127.0.0.0/8` / `::1`）、link-local、或私网地址（RFC1918 / IPv6 ULA）时**自动绕过代理**，其余情况跟随系统代理。仅解析字面量地址，不做 DNS 解析；本地域名（如 `mybox.local`、`*.localhost`）也按本地处理。私网 hostname 需要强制直连时用 `none`。
- `none`：该 endpoint 永远不走代理。
- `system`：永远跟随系统代理环境变量（等价于旧行为）。

注意：仅 `[services.web_search]` / `[services.web_fetch]` 的 endpoint 请求受此配置影响；`fetch` 未配置后端时对公网页面的直接 GET 始终跟随系统代理。

三种 `provider` 的线上协议（对接外部搜索服务时按此实现）：

**`searxng`** — 直连 SearXNG 实例的 JSON API：

- 请求：`GET {base_url}?q={query}&format=json`（`base_url` 通常是 `.../search` 路径）。
- 鉴权：配置了 key 时发送 `Authorization: Bearer <key>`，未配置则不带。
- 响应：`{"results": [{"title", "url", "content"|"snippet", "publishedDate"|"published_at", "engine"|"source"}]}`，字段均可缺省，`url` 非法或重复的条目会被丢弃。

**`brave`** — 直连 Brave Search API：

- 请求：`GET {base_url}?q={query}&count={limit}`。
- 鉴权：`X-Subscription-Token: <key>`。
- 响应：`{"web": {"results": [{"title", "url", "description"|"snippet", "age"}]}}`；`age` 映射为发布时间，`source` 固定为 `brave`。

**`custom`** — 通用 JSON 端点，用于对接任意外部搜索服务（或自建适配层）：

- 请求：`GET {base_url}?q={query}&limit={limit}`，配置了 key 时带 `Authorization: Bearer <key>`。
- 响应：`{"results": [...]}`，每个条目取 `title` / `url` / `snippet`|`content`|`description` / `published_at` / `source`，全部字段宽松解析。

一个特例：`base_url` 包含 `/v1/search` 时，`custom` 自动切换为 Moonshot 兼容协议——`POST {base_url}`，body 为 `{"text_query": "<query>"}`，Bearer 鉴权，响应解析 `{"search_results": [...]}`（字段同上）。因此可以直接把 `base_url` 指向 `https://api.kimi.com/coding/v1/search` 之类的 Moonshot 端点。

三种 provider 共同的后处理：`url` 规范化（仅 http/https）→ 按去重 → 截断到 `limit`。`limit` 由模型调用参数或 `default_limit` 决定，clamp 到 1..20。

### 网页抓取

```toml
[services.web_fetch]
base_url = "https://example.invalid/fetch" # 可选代理 endpoint；未配置时走直接 GET
api_key_env = "WEB_FETCH_API_KEY"
timeout_ms = 30000                          # 默认 30000，运行时下限 1000
proxy = "auto"                              # auto | none | system，默认 auto（同 web_search）
```

配置了 `base_url` 后，`Web(action = "fetch")` 的抓取协议为：

- 请求：`POST {base_url}`，`Content-Type: application/json`，body `{"url": "<目标 URL>"}`。
- 鉴权：配置了 key 时发送 `Authorization: Bearer <key>`。
- 响应：`2xx` 且 body 不超过 4 MiB 时，body 整体当作**已抽取的正文**（纯文本或 markdown）——服务端负责把 HTML 抽取成正文，kkagent 只做一次可读文本清洗并按 `max_chars`（默认 20000，上限 200000）截断。
- 回落：代理返回非 2xx、网络错误或 body 超限时，自动回退为直接 GET 目标 URL（带 SSRF 校验、最多 5 次重定向逐一校验、Content-Type 白名单：text/json/xml/html/javascript，HTML 会做本地正文抽取）。

#### SSRF 校验策略

`fetch` 在两处做安全校验，策略不同：

- **入口预校验（转发到代理前）**：只校验 scheme（`http`/`https`）。host / 私网 IP / DNS 一律不查——出站安全由 `[services.web_fetch]` provider 负责。这样内网目标（如 `http://10.10.10.205/...`）和 fake-IP DNS 环境都能正常转发给代理。
- **直连回落校验（未配代理或代理失败时）**：校验 scheme、拒绝 `localhost`/`*.localhost` 与非公网 IP 字面量，并**额外做 DNS 解析**，确保解析到的 IP 是公网地址。直接 GET 本身仍跟随系统代理。

> 简言之：配了 fetch 代理时，kkagent 只拦非 http(s) URL，其余防护交给 provider；走直连时才做完整 SSRF 校验。

对接外部 fetch 服务时只需实现上面这一个 POST 端点：入参 `url`，返回 2xx + 抽取后的正文文本。Moonshot 的 `.../v1/fetch` coding fetch 端点与此协议兼容（服务端已做正文抽取）。

### 兼容旧配置

旧的 `[services.moonshot_search]` / `[services.moonshot_fetch]` 仍会被读取一次（用于一次性迁移）：`moonshot_search` 自动补 `/v1/search` 后缀并按 Moonshot 兼容协议处理，api_key 缺失时从 `moonshot_fetch` 兜底复用；`moonshot_fetch` 自动补 `/v1/fetch` 后缀。迁移期间工具输出会附带迁移提示，建议尽快改用 `[services.web_search]` / `[services.web_fetch]`。

## MCP Server

stdio：

```toml
[mcp_servers.filesystem]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-filesystem", "/absolute/path"]
env = { "LOG_LEVEL" = "warn" }
cwd = "/optional/working/directory"
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
| `KKAGENT_PLUGIN_MARKETPLACE_URL` | 覆盖顶层 `plugin_marketplace`（默认市场），指定本地路径、file URL 或 HTTP(S) marketplace JSON。不影响 `plugin_marketplaces`。 |
| `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`KIMI_API_KEY`、`GOOGLE_API_KEY` | 对应 Provider 密钥。 |
| `KKAGENT_WEB_SEARCH_URL` | 覆盖 `[services.web_search]` 的 `base_url`，并同时读取下述 key / provider 变量。 |
| `KKAGENT_WEB_SEARCH_KEY` | 搜索服务密钥；未设置时回退 `KKAGENT_MOONSHOT_SEARCH_KEY`、`MOONSHOT_API_KEY`。 |
| `KKAGENT_WEB_SEARCH_PROVIDER` | 搜索 provider（`searxng` / `brave` / `custom`）。 |
| `KKAGENT_MOONSHOT_SEARCH_URL` | 旧搜索服务地址，仍映射到已废弃的 `moonshot_search` 做一次性兼容。 |
| `KKAGENT_MOONSHOT_SEARCH_KEY` 或 `MOONSHOT_API_KEY` | 旧搜索服务密钥。 |
| `KKAGENT_HTTP_TOKEN` | Agent Server HTTP/WS Bearer token。 |
| `KKAGENT_HTTP_READ_TOKEN` | 只读 API token。 |
| `KKAGENT_HTTP_WRITE_TOKEN` | read + 非 terminal 写操作 token。 |
| `KKAGENT_HTTP_TERMINAL_TOKEN` | read + terminal token。 |
| `KKAGENT_ALLOW_IN_MEMORY_TRANSCRIPTS` | 显式允许 transcript 非持久化降级；readiness 会保持失败。 |
| `KKAGENT_TELEMETRY_ENDPOINT`、`KKAGENT_TELEMETRY_CLOUD` | 云遥测地址和开关。 |
| `KKAGENT_NOTIFY` | TUI 任务完成提醒：响铃 + `OSC 9` 文本通知（iTerm2 / WezTerm / kitty / Windows Terminal 等会弹出系统通知，内容为 `完成: <用户上一条输入的首行，30 字符内>`；无已发送输入时为 `turn completed`）。设为 `0` 或 `off` 关闭，默认开启。不支持该序列的终端会静默忽略。 |
| `RUST_LOG` | 日志过滤器。 |

完整可复制模板见 [`examples/config.example.toml`](../examples/config.example.toml)。
