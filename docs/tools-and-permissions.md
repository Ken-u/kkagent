# 工具与权限

## 内置工具

| 类别 | 工具 | 用途 |
|---|---|---|
| 文件 | `Read`、`Write`、`Edit` | 读取、创建和精确修改文本文件。 |
| 搜索 | `Grep`、`Glob` | 搜索内容和路径。 |
| 命令 | `Bash` | 运行 Shell 命令，支持超时、取消和后台任务。 |
| 媒体 | `ReadMediaFile` | 将图片作为多模态内容发送给模型，支持原图坐标裁剪和全分辨率读取。 |
| 规划 | `TodoList`、`EnterPlanMode`、`ExitPlanMode` | 管理步骤和 Plan 模式。 |
| 交互 | `AskUserQuestion`、`SelectTools` | 请求用户输入或选择工具集。 |
| 扩展 | `Skill`、动态 MCP 工具 | 加载 Skill 或调用 MCP Server。 |
| Web | `Web` | 搜索（action=search）和抓取（action=fetch）网页；搜索使用与具体厂商无关的 provider-agnostic 接入。 |
| 任务 | `Agent`、`TaskOutput` | 委派子代理（单个 / agents[] 并发 / prompt_template+items 模板扇出 / resume）与后台任务管理（action=status/list/stop）。 |
| 目标 | `Goal` | 管理跨轮目标（action=create/get/update/budget）。 |
| 定时 | `Cron` | 管理会话内定时任务（action=create/list/delete）。 |

实际可用集合受模型 capability、配置、当前模式和 MCP 连接状态影响。`GET /api/v1/tools` 或 TUI 状态可查看当前工具。

## 渐进式工具披露（Progressive Tool Disclosure）

为节省上下文，内置工具按使用频率分三层（基于 196 个会话、14,447 次调用的统计）：

- **Inline 常驻**：`Read`、`Write`、`Edit`、`Grep`、`Glob`、`Bash`、`TaskOutput`、`TodoList`、`Skill`、`AskUserQuestion`、`SelectTools` 及 `WritePlan`/`ExitPlanMode`（仅 Plan 模式激活时）。完整 schema 始终随请求发送。
- **Deferred 按需加载**：`Agent`、`EnterPlanMode`、`Web`、`ReadMediaFile`、`Goal`、`Cron`、`RequestToolchainAccess`、`ToolchainDoctor` 及全部 MCP 工具。仅在请求中以名字公告，模型调用 `SelectTools` 按名加载 schema 后使用。
- **条件可见**：`WritePlan`/`ExitPlanMode` 仅在 Plan 模式激活时出现在工具列表；执行层（permission guard）独立兜底，隐藏不影响安全性。

基线约 4.5k token 的工具 schema 降至常规请求约 2.3k token（约 -49%）。随后通过合并语义重叠的冷门工具（Task/Agent/AgentSwarm → Agent；TaskList/TaskStop → TaskOutput action；Goal 四件套 → Goal；Cron 三件套 → Cron；WebSearch/FetchURL → Web），并把零调用的沙箱元工具（RequestToolchainAccess、ToolchainDoctor）转为 Deferred，工具总数从 29 降至 19，公告名从 15 降至 8，inline schema 降至 12 个。

## 图片输入

模型配置含 `image_in`（也兼容 `vision`、`image`、`multimodal`）时启用完整图片输入：

- 在提示词中写 `@./screenshot.png`，或在 TUI 粘贴剪贴板图片（macOS/Linux 使用 `Ctrl-V`，Windows 使用 `Alt-V`；Windows 也接受 `Ctrl-V`）。
- `ReadMediaFile` 会返回真正的图片内容，不会把 base64 当作文本塞给模型。
- `region = { x, y, width, height }` 使用原图像素坐标读取局部细节；`full_resolution = true` 跳过常规缩放，超过 Provider 安全限制时明确报错。
- MCP 返回的 image block 和 image 类型 blob resource 会统一压缩后传给模型。
- 历史图片会在上下文投影或 HTTP 413 恢复时替换成文本标记，避免重复携带大体积媒体。

示例工具参数：

```json
{"path":"screenshots/app.png","region":{"x":120,"y":80,"width":640,"height":480}}
```

支持 PNG、JPEG、GIF、WebP 和 BMP 输入；SVG 必须先光栅化。普通图片会统一编码为有界 JPEG。当前模型未声明 `image_in` 时，最新图片输入会被拒绝并给出可操作提示。

## 权限模式

| 模式 | 行为 | 建议场景 |
|---|---|---|
| `manual` | 只读工具通常直接执行，写入和命令等操作请求批准。 | 首次使用、不熟悉的仓库、高风险环境。 |
| `yolo` | 常规操作自动批准；敏感路径、Git 控制等仍可询问。 | 本地受版本控制的日常开发。 |
| `auto` | 除必须交互的情况外尽量自动推进；`AskUserQuestion` 不自动回答。 | 隔离环境中的明确无人值守任务。 |

启动参数 `-y`、`--auto` 可覆盖新会话模式，TUI 可通过 `/permission`、`/yolo`、`/auto` 切换。配置规则会参与最终决策。

## Plan 模式

Plan 模式用于先分析和制定计划。此时写操作仅允许更新当前 session 目录内的 `agents/main/plans/<plan-id>.md`，对源代码的 Write/Edit 会被拒绝。计划 Markdown 第一行必须是一级标题 `# <plan name>`；调用 `ExitPlanMode` 时，文件会按该标题最终命名为 `YYYY-MM-DD_<plan-name>.md`，重名时自动追加 `_2`、`_3`。旧版 `<workspace>/.kkagent/plans/<session-id>.md` 会在恢复时复制迁移，原文件保留。退出 Plan 模式后才进入实施阶段。

计划模式、plan id 和计划正文均随 session 持久化。计划文件写入后，TUI 会完整展示计划全文；在 Plan 模式保持开启时，上下滚动仅限于该计划文档，直到用户退出 Plan 模式。退出 kkagent 后再 resume，同一份计划会从 session 目录恢复。

当 agent 调用 `ExitPlanMode` 后（manual / yolo），底部会出现计划评审选项：
- **执行** — 批准计划并退出 Plan 模式，开始实施
- **修改意见** — 输入反馈，留在 Plan 模式，由 agent 修订计划后再提交
- **拒绝** — 拒绝本轮计划，留在 Plan 模式并结束本轮

若计划含多种方案，agent 可通过 `ExitPlanMode.options` 列出 2–3 个方案供选择（再加「修改意见 / 拒绝」）。auto 权限模式下会自动退出 Plan 模式而不弹评审。

Plan 模式不是完整沙箱：只读工具和经过策略允许的其他能力仍可能运行，因此仍应关注工具请求。

## Shell 安全

`Bash` 会解析命令并结合权限策略执行：

- 明显破坏系统的命令（例如针对根目录的递归删除、格式化磁盘、直接写块设备、下载后直接 pipe 到 Shell）会硬阻断。
- `sudo`、危险删除和 Git hard reset 等会被标记为高风险并进入更严格决策。
- 命令支持超时和取消，取消时会尝试结束进程树；取消状态会在 TUI 中正确展示。
- 后台进程有数量、存活时间和历史记录上限。
- `!shell` 类命令在本地直接执行，不进入 Agent loop，用于快速 shell 操作。

`Bash` 还会按 `[sandbox]` 应用操作系统隔离和资源上限。Linux/macOS 的 `workspace` 模式限制文件边界，并可关闭网络；Windows 默认通过 Job Object 约束整个进程树。沙箱能降低工具进程越界风险，但不能替代 VM/容器级多租户隔离；`yolo` 和 `auto` 下仍不应把生产凭据放进 workspace。

需要排障时，可使用 `kkagent --disable-sandbox` 仅对当前进程临时关闭沙箱与资源限制；该参数不写回配置，且不能与 `--connect` 同时使用（连接独立 Server 时由 Server 配置决定沙箱模式）。

## 文件边界

文件工具会规范化路径，防止通过 `..`、符号链接等绕过工作区和敏感文件策略。`.env`、私钥、常见云凭据和认证目录会受到额外限制。Grep/Glob 也会排除敏感路径。

HTTP 文件 API 受 `trusted_workspaces` 限制；这与 Agent 工具权限是两层独立控制。

## 批准响应

TUI 会展示工具名、参数摘要和风险说明。HTTP 客户端监听到 approval 事件后，可调用：

```http
POST /api/v1/approvals/<approval-id>
Content-Type: application/json

{"decision":"approve"}
```

也可使用拒绝决定并提供 `feedback`。客户端不应根据工具名盲目自动批准，应检查参数、工作目录和会话 ID。

## Tool call 参数异常恢复

部分兼容模型服务会在历史 assistant tool call 的 `arguments` 不是 JSON object 时返回 HTTP 400。kkagent 识别 `Assistant tool call <id>.arguments must be a JSON object` 后不会猜测修复参数，而是：

1. 定位报错 ID；服务隐藏 ID 时回退定位最近一个非 object 参数的 tool call。
2. 将消息历史截断到包含该 tool call 的 assistant 小步骤之前，同时丢弃其后的 tool result。
3. 将转录标记为原子重写。

此后分两种行为：

- 若该模型配置了 `experimental_bad_toolcall_auto_retries > 0`，则在重试次数未耗尽时刷新消息投影、发布一条 LlmRetry 通知并重新请求模型，不再停下来等待用户输入。
- 否则（未配置或次数已耗尽），发布错误、Idle 和 TurnEnd 事件，按 Esc 中断语义结束当前执行，用户可以检查现场后发送“继续”。

这个恢复只撤销一条 Agent 小步骤，不撤销整条用户回合，也不会反向恢复工具已经产生的文件或外部副作用。其他 HTTP 400 仍沿用正常错误和重试策略。
