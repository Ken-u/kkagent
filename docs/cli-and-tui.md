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

## TUI 快捷键

| 按键 | 作用 |
|---|---|
| `Enter` | 发送输入。 |
| `Shift+Enter` / `Ctrl+J` | 插入换行。 |
| `Esc` / `Ctrl+C` | 中断当前生成或关闭浮层。 |
| 空输入时连续两次 `Ctrl+C` | 退出。 |
| 空输入时 `Ctrl+D` | 退出。 |
| `Shift+Tab` | 切换 Plan 模式。写入计划后滚动锁定在完整计划内；`ExitPlanMode` 后底部可选「执行 / 修改意见 / 拒绝」。 |
| 空输入时输入 `!` | Shell 输入模式。 |
| `Ctrl+O` | 展开/收起本轮工具历史概览（或单条工具输出）。 |
| `PgUp` / `PgDn` | 滚动对话记录（Plan 聚焦时仅滚动计划全文）。 |
| 鼠标滚轮 | 滚动对话；贴底时快速连滑一次会跳到本轮提问起点。Plan 模式下有计划时，滚动只限于全篇计划。 |
| 鼠标点选拖拽 | 点击后暂时放开鼠标捕获以便选中复制；任意按键后恢复滚轮滚动。 |
| `↑` / `↓` | 输入历史（多行编辑时先在输入框内移动）。 |
| `Ctrl+P` / `Ctrl+N` | 输入历史。 |
| 大段粘贴 | 超过约 1000 字符或 15 行时折叠为 `[Pasted text #n]` 概览，发送时自动展开。 |

## 斜杠命令

- 会话：`/new`、`/clear`、`/sessions`、`/resume`、`/compact`、`/undo`、`/title`、`/status`、`/usage`。
- 模式：`/permission`、`/yolo`、`/auto`、`/plan`、`/model`、`/effort`、`/thinking`、`/provider`。
- 扩展：`/mcp`、`/skills`、`/plugins`、`/tasks`、`/goal`、`/swarm`、`/web`、`/prompts`。
- 工具：`/init`、`/config`、`/auth`、`/reload`、`/add-dir`、`/btw`、`/fork`、`/search`、`/copy`、`/debug`。
- `/btw <question>`：侧问（不写入主会话）；`Ctrl+G` 打开/关闭侧栏，流式中再按可取消。
- `/fork [title]`：派生当前会话副本，仍停留在原会话；footer context 栏显示同目录会话，输入框为空时按 `Tab` 循环切换。
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
