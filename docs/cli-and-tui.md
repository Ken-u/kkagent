# CLI 与 TUI

不带子命令时启动 TUI；配合 `--prompt` 时执行一轮非交互任务。

## 全局参数

| 参数 | 说明 |
|---|---|
| `--config <path>` | 指定 TOML 配置。 |
| `-y, --yolo` | 以 `yolo` 权限模式开始，与 `--auto` 冲突。 |
| `--auto` | 以 `auto` 权限模式开始。 |
| `--plan` | 以 Plan 模式开始。 |
| `-p, --prompt <text>` | 执行任务并把最终文本写到 stdout。 |
| `--resume <id-or-prefix>` | 恢复已有会话。 |
| `--connect <endpoint>` | 连接独立 Server，仅用于 TUI 或 prompt 模式。 |
| `--dump-system-prompt` | 按当前工作区合成系统提示词，并在末尾追加已注册的工具清单（名称 + 描述 + 是否 deferred），打印到 stdout 后退出；不请求模型，不落盘会话。不能与子命令或 `--prompt` 组合。 |
| `--disable-sandbox` | 仅在当前进程完全关闭 Bash OS 沙箱和资源限制，不修改配置；仅建议用于受控容器或 VM。 |

## 子命令

```bash
kkagent server [--listen <path>] [--http <addr>] [--http-token <token>] \
  [--allow-terminal-api] [--allow-fs-write-api] \
  [--http-rate-limit <requests-per-minute>] [--http-audit-log <path>]
kkagent acp
kkagent mcp serve [--http [<addr>]] [--http-token <token>] \
  [--tunnel <tunnel_id>] [--tunnel-client <path>]
kkagent auth login [--oauth-host <url>] [--base-url <url>]
kkagent auth logout
kkagent auth status
kkagent init [--preset safe|default|full-auto] [--provider <name>] [--model <id>]
kkagent config show
kkagent config get <dotted-key>
kkagent config set <dotted-key> <toml-value>
kkagent config preset <safe|default|full-auto>
kkagent doctor [--json] [--live]
```

`server` 运行独立服务；`acp` 在 stdin/stdout 上运行 ACP NDJSON 桥；`mcp serve` 把 kkagent 暴露为 MCP Server（默认 stdio，`--http` 走 Streamable HTTP，`--daemon` 后台运行并配合 `mcp stop` / `mcp status` 管理；`--tunnel <id>` 隐含 HTTP 并拉起 OpenAI `tunnel-client`，详见[扩展文档 · Secure MCP Tunnel](extensions.md#openai-secure-mcp-tunnel接入-chatgpt)）；`auth` 管理 Kimi 托管凭据。`init` 创建最小配置且默认不覆盖已有文件；`config show/get` 会隐藏密钥，`config set` 写入前会对完整配置重新校验；`doctor` 检查配置、凭据、工作区、持久化目录、常用工具与系统隔离，`--live` 才会访问模型服务。发现阻断问题时 `doctor` 返回非零退出码。

## 非交互模式

```bash
kkagent --config ~/.kkagent/config.toml -p "只输出当前目录名"
answer=$(kkagent -p "总结 README" 2>kkagent.err)
```

最终回答写到 stdout，诊断日志写到 stderr。有写入需求时按风险显式选择 `-y`；无人值守脚本不应默认使用 `--auto`。

当模型未显式设置 `experimental_retry_partial_stream_errors` 时，`-p` 会在流式响应已有部分内容后遇到断连时继续按 `max_attempts_per_step` 重试；可在模型配置中设为 `false` 关闭。

## 调试系统提示词

```bash
kkagent --dump-system-prompt
kkagent --config ~/.kkagent/config.toml --dump-system-prompt > system-prompt.txt
```

按当前目录合成完整的系统提示词并打印后退出：包括基础指令、Workspace 段、`AGENTS.md` / `.kkagent/AGENTS.md` 项目指令、Skill 目录段和插件追加段。合成走真实 Session 相同的代码路径，可用于确认项目指令或 Skill 是否注入；不请求模型，不创建、不落盘会话。不能与子命令或 `--prompt` 组合。

排障时可用 `kkagent --disable-sandbox` 临时覆盖 `[sandbox].mode`。该参数不写回配置，也不能与 `--connect` 同时使用；连接独立 Server 时必须在 Server 启动参数或 Server 配置中决定沙箱模式。

Footer 第二行在 `context` 左侧显示当前有效沙箱模式：绿色 `● sandbox:workspace` 表示工作区文件系统隔离，黄色 `● sandbox:process` 表示仅有进程/资源限制，红色 `● sandbox:off` 表示沙箱及资源限制均已关闭。配置为 `auto` 时显示当前平台解析后的实际模式。

Footer 第一行的模型槽位跟随当前视图：切进子 agent 视图（`/agents` 选择或 Tab）时显示该子 agent 实际运行的模型（内建/internal 型为解析后的别名，ACP 插件型为外部 agent 名字），Esc 返回主会话后恢复主会话模型。

## TUI 快捷键

| 按键 | 作用 |
|---|---|
| `Enter` | 发送输入；当前 turn 运行中时排队到下一 turn。 |
| 运行中 `Ctrl+S` | 注入 steer，引导模型在下一个模型步骤响应；已有 next-turn 队列时会按队列顺序与当前草稿一并注入。 |
| `Shift+Enter` | 插入换行；不再用于 steer，避免不同终端无法区分它和普通 Enter。 |
| `Ctrl+J` | 插入换行。 |
| `Esc` | 先关闭当前菜单/浮层；无浮层且 turn 运行中时中断生成（留在 TUI）。空闲下连按两次可选择历史提示分叉编辑。 |
| `Ctrl+B` | 后台化：退出 TUI，standalone server 与进行中的 turn 继续跑；再次 `kk` 自动 resume。有菜单/浮层时只关闭浮层；Plan 模式或 in-process 模式下忽略。 |
| `Ctrl+C` | 输入非空时清空输入。空输入时第一次请求确认退出；第二次：无 turn 则退出并保留 server；有 turn 时弹窗可选 Terminate / Background / Cancel。不会在第一次按键时中断 turn。 |
| 空输入时 `Ctrl+D` | 关闭当前会话标签，或在无可关标签时确认退出。 |
| `Shift+Tab` | 切换 Plan 模式。写入计划后滚动锁定在完整计划内；`ExitPlanMode` 后底部可选「执行 / 修改意见 / 拒绝」。 |
| 空输入时输入 `!` | Shell 输入模式。 |
| `Ctrl+O` | 全局切换最近 5 个 turn 的工具输出展开/收起模式；点击工具提示行可单独切换，且该项不再跟随全局模式。 |
| `PgUp` / `PgDn` | 滚动对话记录（Plan 聚焦时仅滚动计划全文）。 |
| 鼠标滚轮 | 滚动对话；贴底时快速连滑一次会跳到本轮提问起点。Plan 模式下有计划时，滚动只限于全篇计划。 |
| 鼠标点选拖拽 | 点击后暂时放开鼠标捕获以便选中复制；任意按键后恢复滚轮滚动。 |
| `↑` / `↓` | 输入历史（多行编辑时先在输入框内移动）。 |
| `Ctrl+P` / `Ctrl+N` | 输入历史。 |
| 大段粘贴 | 超过约 1000 字符或 15 行时折叠为 `[Pasted text #n]` 概览，发送时自动展开。 |

## 斜杠命令

- 会话：`/new`、`/clear`、`/sessions`、`/resume`、`/compact`、`/undo`、`/title`、`/status`、`/usage`。
- 模式：`/permission`、`/yolo`、`/auto`、`/plan`、`/model`、`/effort`、`/thinking`、`/provider`。
- `/effort [off|on|none|minimal|low|medium|high|xhigh|max]`：查看或设置思考强度。带参数直接生效（`on` 等价 `high`）；无参数打开选择器，优先展示当前模型 `support_efforts` 声明的等级。写入 `[thinking].effort`，优先级高于模型级 `default_effort`。会话运行在外部 ACP agent 上时，`/effort` 改的是外部 agent 的思考等级，见[ACP 外部 agent 会话](#acp-外部-agent-会话)。
- 扩展：`/mcp`、`/skills`、`/plugins`（多级弹窗管理已安装插件、marketplace 与安装来源）、`/plugins marketplace [source]`、`/plugins install <id-or-source>`、`/plugins update|enable|disable|remove|info <id>`、`/plugins reload`、`/tasks`、`/cron`（浏览/删除定时任务；也支持 `/cron list`、`/cron delete <id>`）、`/agents`（子 agent 状态与活动日志；正文不再灌入子 agent 输出）、`/goal`、`/swarm`、`/web`、`/prompts`。
- `/goal <objective>`：创建目标；`/goal status|pause|resume|cancel` 管理状态；`/goal budget <turns|tokens|milliseconds|seconds|minutes|hours> <正整数|off>` 调整或清除单项硬预算。
- 工具：`/init`、`/config`、`/auth`、`/reload`（同步热重载 TUI + server 配置，含新增模型；MCP/hooks 仍可能需重启）、`/add-dir`、`/btw`、`/fork`、`/search`、`/copy`、`/debug`。
- `/btw <question>`：打开全屏 BTW 侧问（不写入主会话），状态入口显示在 `git:<branch>` 所在的第一行，不作为 session 标签。`Ctrl+G` 隐藏或再次呼出 BTW，回答会在后台继续；再次执行带问题的 `/btw` 会删除旧 BTW，并从命令执行时的当前主 session 创建新的上下文快照。BTW 与主窗口共用 Markdown 和 thinking 折叠样式；空输入时按 `Ctrl+D` 可取消并彻底删除 BTW。
- `/fork [title]`：派生当前会话副本，仍停留在原会话；仅当存在 fork 族时，footer context 栏显示可切换会话，空输入下 `Tab` / `←` / `→` 循环切换。
- `/sessions`：仅列出当前 workspace 中有内容的会话（正在查看的空会话仍可见）；离开空会话会自动丢弃记录。
- Footer session 条按 TUI 启动时的 workspace 稳定分组，该 workspace 的 session 始终排在其他 workspace 前面；子 agent 只占一个 `agents 当前运行数/总数` 聚合入口，点击后在 `/agents` 中选择具体 transcript。
- `/sessions` 删除：`Ctrl+D` 后 ↑↓ 选 No/Yes（默认 No），Enter 确认。
- `/model [名称]`：切换会话主模型；无参数打开选择器。若选中的模型等于顶层 `fallback_model`，会继续弹出选择框，可为本次会话禁用 fallback，或另选一个不同的 fallback 模型。已启用插件里声明了 `transport: "acp"` 的外部 agent（如官方 `kk-cursor-agent`）会自动作为 `acp:<插件>.<名称>` 出现在列表里，详见下文[ACP 外部 agent 会话](#acp-外部-agent-会话)。参数按「配置别名 → 完整 `acp:` 别名 → 插件 id → 唯一匹配的 agent 短名（如 `/model cursor`）」解析，短名有歧义时会列出完整别名；agent 后面可以接 `#等级`（如 `/model cursor#max`）同时指定外部 agent 的思考等级。
- 帮助与退出：`/help`、`/release-notes`、`/feedback`、`/info`、`/exit`、`/quit`、`/q`。

部分命令仅展示状态或排队执行；输入 `/help` 可查看当前版本的参数提示。

## ACP 外部 agent 会话

在 `/model` 里选中 `acp:` 项（例如 `/model cursor`）后，这个会话的每一轮都由外部 ACP agent（如 Cursor CLI 的 `agent acp`）执行，TUI 只作为前端：

- 文本、思考、工具卡片（工具名形如 `cursor/execute`）和 Todo 面板照常显示；外部 agent 发来的权限请求、`ask_question` 提问和 `create_plan` 计划评审会弹成与普通会话相同的卡片，答案回传给它。权限按当前权限模式处理（`manual` 弹审批，`yolo` 只询问敏感命令/文件，`auto` 自动放行）；外部 agent 自己不发请求的操作（如 Cursor CLI 配置为 `approvalMode: unrestricted` 时）kkagent 无从拦截。
- 外部 agent 自己管理工具、沙箱、MCP 和上下文，所以 footer 显示 `cursor · ACP`、`● sandbox:external` 和 `context: external`，不再显示上下文百分比；kkagent 的内建工具、skills、`/goal` 与自动压缩对它不生效。
- **模型与思考等级由外部 agent 决定**：kkagent 不能替它换模型，也不会读写 `config.toml` 里的 `[models]`、`[thinking]`。第一条消息发出后，footer 会在 `cursor · ACP` 后显示 agent 实际在用的模型和思考等级（如 `claude-sonnet-5-5  thinking: high`，窄屏为 `claude-sonnet-5-5 · think:high`），数据来自 agent 自己的汇报。要换模型请在 agent 一侧改（Cursor CLI 里的 `/model`，或 `~/.cursor/cli-config.json`；Codex 里的 `~/.codex/config.toml`）。
- **`/effort` 控制外部 agent 的思考等级**：`/effort`（选择器，列出 agent 提供的等级）或 `/effort max`；等价于 `/model cursor#max`。等级写进会话的模型别名（`acp:kk-cursor-agent.cursor#max`），随会话保存，下一条消息发送前由 kkagent 通过 ACP 的 `session/set_config_option` 设给 agent。agent 提供的等级不含你选的那一档时，已汇报过等级的会话会直接拒绝并列出可选项；尚未汇报时会就近取最接近的一档并说明（如 `minimal` → `low`）。`/effort off` 等于 `none`，多数 agent 没有这一档。
- 并非所有外部 agent 都允许客户端选择思考等级：Cursor CLI 需要它提供的 `thought_level` 选项（kkagent 在 `initialize` 里声明 `parameterizedModelPicker` 后即会提供），Codex 的 `codex-acp`（实测 1.10.0）对 `~/.codex/config.toml` 里自定义的模型不提供，此时 `/effort` 只会提示「该 agent 不允许客户端选择思考等级」，等级请在 agent 自己的配置里改。
- 副作用：Cursor CLI 把「模型 + 思考等级」保存在 `~/.cursor/cli-config.json`（全局），所以在 kkagent 里改等级也会改变你之后直接使用 Cursor CLI 时该模型的默认等级。
- Plan 模式（`Shift+Tab`）与外部 agent 的 plan 模式同步；计划被接受后退出 Plan 模式，随后的消息按普通模式执行。
- 外部 agent 没有「运行中插话」：运行中 `Enter` 与 `Ctrl+S` 都只是排队，本轮结束后作为下一条消息发送（TUI 提示「已排队」）；`Esc` 会先发 `session/cancel`，8 秒内没有响应则回收该进程，保留已输出的部分文本，下一轮自动重启。
- 会话转录保持普通结构（用户消息、助手文本，外加一条对 UI 隐藏的活动摘要），可随时 `/model` 切回普通模型接管同一会话，也可再切回外部 agent；`/export-md`、`/copy`、`/fork` 照常可用。
- `/undo`、`/edit` 与历史恢复只回退对话：外部 agent 改动的文件不会恢复，TUI 会在回退后追加一条警告，请自行用 git 处理。
- 图片附件不会转发给外部 agent；`/btw` 在这类会话里回退到默认模型。

## 连接独立 Server

```bash
# 终端 A
kkagent server

# 终端 B
kkagent --connect ~/.kkagent/server.sock
kkagent --connect ~/.kkagent/server.sock -p "检查当前工程"
```

Unix endpoint 是 Unix domain socket；Windows 上同一路径保存仅指向本机 loopback 随机端口的端点信息。默认 `kkagent` 使用进程内 transport，不会自动连接残留 socket。
