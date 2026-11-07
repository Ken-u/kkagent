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
- **Deferred 按需加载**：`Agent`、`EnterPlanMode`、`Web`、`ReadMediaFile`、`Goal`、`Cron` 及全部 MCP 工具。仅在请求中以名字公告，模型调用 `SelectTools` 按名加载 schema 后使用。
- **条件可见**：`WritePlan`/`ExitPlanMode` 仅在 Plan 模式激活时出现在工具列表；执行层（permission guard）独立兜底，隐藏不影响安全性。

Kimi provider 原生支持 `messages[].tools`：加载的 schema 追加在历史消息里，顶层 `tools[]` 保持字节级稳定，provider prompt cache 不受影响。OpenAI/Anthropic/Google 模型也可通过 `capabilities = ["dynamically_loaded_tools"]` 显式启用渐进式披露；这些 adapter 会在序列化时把已加载 schema 合并到顶层 `tools[]`，因此每次首次加载新工具都可能刷新 provider prompt cache。这是减少常规请求 schema 体积与缓存命中率之间的显式取舍。

官方 OpenAI endpoint 会为主 Agent 请求自动发送一个由模型、system prompt 与稳定工具集共同派生的 `prompt_cache_key`。自定义 OpenAI-compatible endpoint 默认不发送该字段；确认端点兼容后，可在模型 `capabilities` 中加入 `prompt_cache_key`。`/usage` 同时读取 OpenAI 返回的 cache read/write token。

基线约 4.5k token 的工具 schema 降至常规请求约 2.3k token（约 -49%）。随后通过合并语义重叠的冷门工具（Task/Agent/AgentSwarm → Agent；TaskList/TaskStop → TaskOutput action；Goal 四件套 → Goal；Cron 三件套 → Cron；WebSearch/FetchURL → Web），并把零调用的沙箱元工具（RequestToolchainAccess、ToolchainDoctor）转为 Deferred，工具总数从 29 降至 19，公告名从 15 降至 8，inline schema 降至 12 个。再后来这两个沙箱元工具因长期零调用被彻底移除：工具链诊断并入 `kkagent doctor`（`toolchain` 检查项含完整报告与缓存配额告警），一次性路径授权改为在 `~/.kkagent/config.toml` 的 `[toolchain.profiles.<name>]` 里静态声明（`runtime_read_only` / `agent_cache_read_write` / `env`），模型侧引导收敛到内置 skill `toolchain-sandbox`。

## TodoList

TodoList 对模型的输入宽容：模型常把 `updates` 当作“按标题镜像整份清单”来用，也常写 `completed`、数字 id、`id: null` 等变体。工具在 schema 校验前先做归一化，而不是整批拒绝（近 30 天 TodoList 失败率约 19.6%，主要由这类写法造成）。

- 状态同义词：`completed` / `complete` / `finished` → `done`，`in-progress` / `doing` → `in_progress`，`todo` → `pending`，`canceled` / `skipped` → `cancelled` 等；`content` / `text` / `task` / `name` 视为 `title`；字符串数组视为标题列表；被序列化成字符串的数组会还原；多余字段忽略；`id: null` 或空串视为缺省。
- 标题匹配：先完全相同，再忽略大小写/空白/标点，最后在较长标题（≥ 8 个字符）上做唯一包含匹配；重名时取第一个未完成的任务。
- `updates` 里的数字或 `t1` 形式的 id 视为清单中的第几项（此时随带的 `title` 只用于选择，不改名）；未知 id 若同时带有标题，则按标题处理。
- `updates` 里匹配不到任何任务的标题：若状态是 `done` / `cancelled`（或只改名），视为当前 `in_progress` 任务（该任务未被本次其它更新指定时），并改成这个标题；其余情况追加为新任务，结果中会提示 `Added new task`。
- 同一任务的多条更新依次合并，后者覆盖前者；清单任何时候至多一个 `in_progress`，本次最后设为进行中的任务保留，其余退回 `pending`（`todos` 整体替换时保留最后一个），结果中会说明。
- `todos` 里不存在或重复的 id 视为新任务的标签并忽略，任务靠标题保持身份。

无法恢复的输入（未知状态值、空标题、没有可定位的 id、没有 `in_progress` 任务却不带标题和 id）仍然整批拒绝，并保证清单不被部分修改。

| 参数 | 用途 |
|---|---|
| 无参数 `{}` | 读取完整清单、状态和 ID。 |
| `todos: [{title, status}]` | 创建或整体替换清单（推荐的日常推进方式）；未改动的标题保持原有身份；空数组清空。 |
| `updates: [{id?, status?, title?}]` | 只修改指定字段；每项至少提供 `status` 或 `title`，按标题定位任务，无需 id。所有目标按调用前的清单解析。 |
| `add: [{title, status?}]` | 追加任务，由工具生成 ID；默认状态为 `pending`。 |

例如，按已有标题完成当前项并启动下一项：

```json
{
  "updates": [
    {"title": "审核完整 diff", "status": "done"},
    {"title": "检查边界条件", "status": "in_progress"}
  ]
}
```

状态支持 `pending`、`in_progress`、`done`、`cancelled`。使用 `updates` 将任务设为 `cancelled` 可取消该任务。`updates` 与 `add` 可以组合，整次请求验证通过后才应用；无法定位的 ID、无效状态或空标题会拒绝整次变更。`todos` 不与 `updates/add` 混用。旧的 `action/items/merge` 调用仍兼容。

ID 是 12 位随机 hex（如 `a4f2c1e93b7d`），只在读取结果里出现；模型凭空写的 ID（如 `1`、`t2`、`verify-baseline`）不会误改真实任务：数字/`t1` 形式按清单位置解释，其它无法定位的 ID 在没有标题时拒绝整批变更并回滚。ID 随会话持久化，重命名、重排、跨回合和恢复会话时保持不变；旧的完整 UUID 或数字 ID 继续保留、不迁移。整体替换时应携带已有任务的 ID；省略 ID 的条目会尽量按相同标题匹配旧任务，否则生成新 ID。

写入结果只返回进度统计和当前任务的标题（不含 ID）；ID 只在读取结果里出现。完整快照仍用于持久化和 TUI 更新，模型需要操作其他任务或全局视图时主动读取。自动推进：本次调用把某个此前未完成的任务改成 `done` / `cancelled`、且调用后没有任何任务处于 `in_progress` 时，清单中第一个 `pending` 任务会自动变成 `in_progress`，结果里提示 `Started the next task`，省掉“只为说明开始下一项”的那次调用；只创建清单（没有任务被本次完成）不会自动开始任何任务，工具也不会自己把任务标为完成。工具描述和写入结果还提醒模型不要单独调用 TodoList：近 30 天约 5.7% 的 LLM 请求只含一个 TodoList 调用，应把进度更新和下一步真实工作放进同一次响应。

提示词引导模型最多保持一项 `in_progress`，先处理当前任务及必要依赖，进入后续任务时再展开细节。多项同时进行时，工具保留最近设为进行中的一项，其余退回 `pending` 并在结果中说明。恢复或压缩后如果缺少当前任务状态，模型应读取已保存清单。以上是执行引导，不能保证模型的实际思考范围。

## Read

`Read` 返回带行号的文本。没有 `limit` 时只返回前 400 行（此前是 1000 行 / 100KB）：近 30 天约 48% 的 Read 不带范围，其中超过 2 万字符的大读取占了一半以上的 Read 输出。文件更长时，结果末尾会给出总行数和下一段的 `offset`；需要更大窗口时显式传 `limit`（上限 1000），定位具体内容用 `Grep`。短于 400 行的文件仍然整份返回。

`files: [{path, offset?, limit?}]` 一次读取多个文件（最多 8 个，总输出约 160KB，超出的文件会列出并要求另起一次调用）。每个文件前有 `==> path <==` 标题，单个文件失败不影响其它文件；每个成功读取的文件都会建立自己的内容基线，供“文件在读取后被外部修改”的写入保护使用。`paths: [...]`、数组里的纯字符串和 `file_path` 会自动转换成对应形式。

## Grep

`Grep` 基于本机 `rg`，覆盖日常 `rg` / `grep -rn` 用法，不必再绕道 `Bash`：

| 参数 | 对应 rg 选项 |
|---|---|
| `pattern`、`fixed_strings`、`word`、`case_insensitive`、`multiline` | 正则；`-F`；`-w`；`-i`；`-U` |
| `invert_match`、`only_matching`、`max_per_file` | `-v`；`-o`；`-m N` |
| `output_mode`：`content` / `files_with_matches`（默认）/ `count_matches` | 匹配行；`-l`；`-c`（结果头部给出总数和文件数） |
| `context` / `context_before` / `context_after` | `-C` / `-B` / `-A` |
| `path`、`paths[]` | 一个或多个搜索目标，逐个经过工作区路径守卫 |
| `glob`、`globs[]`、`type` | `--glob`（前缀 `!` 表示排除）；`-t` |
| `hidden`、`include_ignored` | `--hidden`（仍排除 `.git`）；`--no-ignore` |
| `head_limit`（默认 200）、`offset` | 结果分页 |

`pattern` 总是以 `-e` 传给 `rg`，以 `-` 开头的模式（如 `--no-verify`）不会被当成选项。`glob` / `path` 传数组、`paths` 传字符串会自动转换成对应的复数形式。

## Bash 的读取/搜索/列目录提示

近 30 天约 55% 的 `Bash` 调用其实只是读文件（`cat` / `head` / `tail` / `sed -n`）、搜索（`rg` / `grep -r`）或列目录（`ls` / `find` / `fd`）。这些命令仍然照常执行，只是结果末尾追加一行指向 `Read` / `Grep` / `Glob` 的提示（带行号、输出有上限、可与其它调用并行）。

判断刻意保守：命令含重定向、`&&` / `||` / `;`、后台 `&`、`$(…)`、反引号、heredoc，或管道里出现 `head` / `tail` / `wc` / `sort` / `uniq` / `cut` / `nl` / `grep` / `rg` / `tr` / `column` 之外的命令（如 `xargs`、`tee`、`sed`）时不给提示；`tail -f`、`sed -i`、带 `-exec` / `-delete` 的 `find` 也不会命中。开头的 `cd dir &&` 和 `2>&1` / `2>/dev/null` 会被忽略。后台运行的命令不追加提示。

提示只会指向当前模型能调用的工具（被 `[model_profiles.*]` 的 `hide` / `expose` 去掉的工具不会被提到）。想完全关闭，在对应 profile 里写 `[model_profiles.NAME.behavior] bash_route = "off"`；`models = ["*"]` 的 profile 可以对所有模型生效。

## 批量查询提醒

近 30 天约 86% 的 LLM 请求只发一个工具调用，其中大量是互不依赖的只读查询，每个都多花一次完整的 LLM 往返。连续两次请求都只含一个只读查询（`Read` 单文件、`Grep`、`Glob`，或只读的 `Bash`，判断同上节）时，最后一个工具结果末尾会追加一条 `<system-reminder>`，提示把互不依赖的查询放进同一次响应并行执行；之后每 4 次请求才重复一次。一次请求里有多个调用、多文件 `Read`、写入/编辑等真实工作时计数清零，每个用户回合开始时也清零。

用 `[model_profiles.NAME.behavior] batch_reminder = false` 关闭。

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
- 命令支持超时和取消，取消时会尝试结束进程树；取消状态会在 TUI 中正确展示。已被中断的会话不会再启动新命令（前台、后台都一样），SSH/kaos 前台命令也会在中断时立即停止等待（远端进程无法被远程结束，结果里会如实说明）。
- 用户中断（`Esc` / `session.interrupt`）只中断当前 turn，不会杀掉已经在跑的后台进程；后台任务完成的通知也不会在中断后自动唤醒模型，而是留到下一次用户输入时一并送达。需要连同后台进程一起结束时，用 `TaskStop` / `/tasks`，或在 `session.interrupt` 里带 `cancel_background: true`（TUI 的「Terminate 并退出」、关闭会话标签、删除会话会这样做）。
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
