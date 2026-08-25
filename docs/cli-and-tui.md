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

`server` 运行独立服务；`acp` 在 stdin/stdout 上运行 ACP NDJSON 桥；`auth` 管理 Kimi 托管凭据。`init` 创建最小配置且默认不覆盖已有文件；`config show/get` 会隐藏密钥，`config set` 写入前会对完整配置重新校验；`doctor` 检查配置、凭据、工作区、持久化目录、常用工具与系统隔离，`--live` 才会访问模型服务。发现阻断问题时 `doctor` 返回非零退出码。

## 非交互模式

```bash
kkagent --config ~/.kkagent/config.toml -p "只输出当前目录名"
answer=$(kkagent -p "总结 README" 2>kkagent.err)
```

最终回答写到 stdout，诊断日志写到 stderr。有写入需求时按风险显式选择 `-y`；无人值守脚本不应默认使用 `--auto`。

## 调试系统提示词

```bash
kkagent --dump-system-prompt
kkagent --config ~/.kkagent/config.toml --dump-system-prompt > system-prompt.txt
```

按当前目录合成完整的系统提示词并打印后退出：包括基础指令、Workspace 段、`AGENTS.md` / `.kkagent/AGENTS.md` 项目指令、Skill 目录段和插件追加段。合成走真实 Session 相同的代码路径，可用于确认项目指令或 Skill 是否注入；不请求模型，不创建、不落盘会话。不能与子命令或 `--prompt` 组合。

排障时可用 `kkagent --disable-sandbox` 临时覆盖 `[sandbox].mode`。该参数不写回配置，也不能与 `--connect` 同时使用；连接独立 Server 时必须在 Server 启动参数或 Server 配置中决定沙箱模式。

Footer 第二行在 `context` 左侧显示当前有效沙箱模式：绿色 `● sandbox:workspace` 表示工作区文件系统隔离，黄色 `● sandbox:process` 表示仅有进程/资源限制，红色 `● sandbox:off` 表示沙箱及资源限制均已关闭。配置为 `auto` 时显示当前平台解析后的实际模式。

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
- 扩展：`/mcp`、`/skills`、`/plugins`（多级弹窗管理已安装插件、marketplace 与安装来源）、`/plugins marketplace [source]`、`/plugins install <id-or-source>`、`/plugins update|enable|disable|remove|info <id>`、`/plugins reload`、`/tasks`、`/agents`（子 agent 状态与活动日志；正文不再灌入子 agent 输出）、`/goal`、`/swarm`、`/web`、`/prompts`。
- 工具：`/init`、`/config`、`/auth`、`/reload`（同步热重载 TUI + server 配置，含新增模型；MCP/hooks 仍可能需重启）、`/add-dir`、`/btw`、`/fork`、`/search`、`/copy`、`/debug`。
- `/btw <question>`：打开全屏 BTW 侧问（不写入主会话），状态入口显示在 `git:<branch>` 所在的第一行，不作为 session 标签。`Ctrl+G` 隐藏或再次呼出 BTW，回答会在后台继续；再次执行带问题的 `/btw` 会删除旧 BTW，并从命令执行时的当前主 session 创建新的上下文快照。BTW 与主窗口共用 Markdown 和 thinking 折叠样式；空输入时按 `Ctrl+D` 可取消并彻底删除 BTW。
- `/fork [title]`：派生当前会话副本，仍停留在原会话；仅当存在 fork 族时，footer context 栏显示可切换会话，空输入下 `Tab` / `←` / `→` 循环切换。
- `/sessions`：仅列出当前 workspace 中有内容的会话（正在查看的空会话仍可见）；离开空会话会自动丢弃记录。
- `/sessions` 删除：`Ctrl+D` 后 ↑↓ 选 No/Yes（默认 No），Enter 确认。
- `/model`：切换会话主模型。若选中的模型等于顶层 `fallback_model`，会继续弹出选择框，可为本次会话禁用 fallback，或另选一个不同的 fallback 模型。
- 帮助与退出：`/help`、`/release-notes`、`/feedback`、`/info`、`/exit`、`/quit`、`/q`。

部分命令仅展示状态或排队执行；输入 `/help` 可查看当前版本的参数提示。

## 连接独立 Server

```bash
# 终端 A
kkagent server

# 终端 B
kkagent --connect ~/.kkagent/server.sock
kkagent --connect ~/.kkagent/server.sock -p "检查当前工程"
```

Unix endpoint 是 Unix domain socket；Windows 上同一路径保存仅指向本机 loopback 随机端口的端点信息。默认 `kkagent` 使用进程内 transport，不会自动连接残留 socket。
