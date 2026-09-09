# ACP、MCP、Skills、Hooks 与插件

## ACP

`kkagent acp` 在 stdin/stdout 上运行 newline-delimited JSON-RPC（Agent Client Protocol v1），供编辑器和 IDE 启动为子进程：

```bash
kkagent --config ~/.kkagent/config.toml acp
```

客户端先调用 `initialize`，返回官方 `agentCapabilities`（`loadSession`、`promptCapabilities.tools`）与 `authMethods`。随后：

- `session/new`（支持 `cwd` 与可选 `initialMessage`）返回 `sessionId` 与 `modes`；
- `session/load` 按 transcript sessionId 恢复历史会话，并把历史按 `user_message_chunk` / `agent_message_chunk` 回放；
- `session/prompt` 接受官方 content block 数组（text/image 等），turn 期间通过 `session/update` 通知流式推送 `agent_message_chunk`、`agent_thought_chunk`、`tool_call`、`tool_call_update` 等官方变体，turn 结束返回 `{stopReason}`（`end_turn` / `cancelled` / `max_tokens` …）；
- 工具批准映射为 agent→client 请求 `session/request_permission`（options：`allow_once` / `allow_always` / `reject_once` / `reject_always`，kind 为 `edit` / `command` / `fetch`），客户端以 `{outcomeKind: "selected", optionId}` 或 `{outcomeKind: "cancelled"}` 响应；
- 用户提问映射为 `session/request_input`（text 或 select，多选遵循 `multiSelect`），响应为 `{content: [...]}` 或 `{canceled: true}`。

`model/list`、`session/set_mode`、`session/set_model`、`session/cancel`、`fs/*`、`terminal/*`、`mcp/list`、`commands/list` 与 slash commands 保持可用；旧 `approval/*` 接口保留为兼容层。

ACP 进程的 stdout 是协议通道，不能混入普通日志；诊断信息应读取 stderr。当前 [VS Code 扩展](../apps/vscode/README.md) 是一个实现了流式更新与权限/输入请求的 ACP 客户端示例。

## MCP Client

kkagent 可把外部 MCP Server 的工具动态注册给模型。支持：

- stdio 子进程；
- SSE；
- HTTP / Streamable HTTP；
- 远程 OAuth 配置。

配置见[配置参考](configuration.md)。启动后通过 `/mcp` 检查连接和工具。给不可信 MCP Server 的权限应按远程代码执行能力对待：它可以看到传入参数，也可能访问网络或本机文件。

## MCP Server

`kkagent mcp serve` 把 kkagent 本身暴露为一个 MCP Server——一个异步编码任务监督/委派 API，供外部编排方（网页 ChatGPT、Codex、另一个 agent）决策与规划、kkagent 执行：

- `list_workspaces` / `get_context` — 发现工作区（自动注册自 kkagent 会话历史 + 配置的 trusted_workspaces）与项目监督级状态；
- `inspect` / `Glob` / `Grep` — 只读查阅源码、日志、git diff、图片与任务产物，不跑 agent；Glob/Grep 复用内置搜索工具和路径策略；
- `write_plan` / `get_plan` — 持久化编排方撰写的执行计划（markdown），返回 `plan_id` / `plan_version`；修订产生新版本，历史正文仍可读取；
- `delegate` — 启动异步编码任务并立即返回 `task_id`；可传 `plan_id`，计划会安装到 session 的 `plan_file_path`，prompt 只引用路径。模型、工具、worktree 隔离均由 kkagent 自行决定；
- `get_progress` / `continue_task` / `get_result` / `cancel` — 轮询状态（queued / running / waiting_input / waiting_permission / completed / failed / cancelled）、回答提问、审批动作、追加指令、取审查摘要、取消任务。

### 客户端与 worker 协作

推荐流程：`get_context` → `Glob/Grep/inspect` → `write_plan` → `delegate` → `get_progress` → `get_result/inspect` → `continue_task`。

- 新任务：`delegate({workspace, prompt, plan_id?, plan_version?, request_id?})`。返回 `task_id`、`session_id` 和实际 `run_dir`；审查或搜索隔离任务的代码时使用这个 `run_dir`。
- 历史会话：`continue_task({session_id, instruction})`，session_id 支持唯一前缀；恢复原会话消息、模型、fallback、工作目录和 core 保存的检查点/计划状态。与 `task_id` 互斥；已有 MCP task 会被复用，返回 task_id 后使用它轮询、回答问题和审批。
- 接续计划：`continue_task({task_id, plan_id, plan_version, instruction?})`。省略版本使用最新版本；仅编辑 `write_plan` 不会影响正在执行的任务。
- 审查交接：执行状态 `completed` 与 `review_status: awaiting_review` 分开。`get_result` 返回 base/head、包含未提交及未跟踪修改的 snapshot、工作区状态、worker 报告及产物路径。验收使用 `continue_task({task_id, review:{accepted:true, head, snapshot}})`；传 false 记录需修改，然后另发 instruction。代码快照不匹配时拒绝旧审查结果。验收不会自动提交或发布；继续执行时需明确指令和验证范围。快照要求 Git 仓库至少已有一个 commit；非 Git 工作区仍可执行、查阅和继续任务。
- 指令重试：`delegate`、`continue_task`、`write_plan`、`cancel` 支持 `request_id`，相同工具及相同参数重试返回保存的回执；不同参数复用同一个 ID 会报错。它防止正常网络重试重复派工，不承诺进程恰在动作与回执落盘之间崩溃时的 exactly-once；崩溃后先查任务状态再决定是否重发。
- 增量进度：`get_progress({task_id, after_event})` 返回 `event_cursor` 和较新的 recent_events；`events_lost` 表示已超出有限保留窗口。需要历史背景时用 `get_session_context({session_id, offset?, limit?})` 分页获取最近用户/助手正文，不返回 thinking 或全量工具 transcript。

任务、计划版本和重试回执复用 transcript SQLite 连接持久化；会话消息由原有 AgentLoop 持久化。服务重启后不会自动执行未完成任务，而是标记失败并提示显式继续；已经完成的报告和审查记录仍可查询。重启前尚未消费的指令及交互回答不会自动重放，客户端应核对历史后重新给出需要执行的指令。历史 CLI/TUI session 的恢复面向已经停止执行的会话，不用于接管另一个进程中仍在运行的 turn。

搜索参数保留 `workspace`、`pattern`、`path`、`limit`；Grep 另有 `glob`、`case_insensitive`、`context` 和 `offset`，返回匹配行号。Glob 被截断时可缩小 pattern/path 或提高 limit（最大 2000）；Grep 依赖本机 `rg`。只读搜索限制在指定 workspace 内，并沿用敏感路径和忽略规则。

Diff 示例：

```json
{"kind":"diff","workspace":"/path/to/repo","base":"HEAD~1","head":"HEAD","format":"files"}
{"kind":"diff","workspace":"/path/to/repo","base":"main","head":"feature","merge_base":true,"path":"src/main.rs","offset":0,"limit":400}
```

`base/head` 都不传时对比 HEAD 与当前工作区（含已暂存修改）；只传 base 时对比该版本与工作区；head 必须与 base 一起传。`format` 为 `patch`（默认）、`stat` 或 `files`，通过 `next_offset` 继续分页。未跟踪文件不在 git diff 内，应按 `get_result.working_tree_status` 用 inspect 读取。`reviewed_snapshot` 记录已验收的快照，后续代码变化后应重新审查当前 snapshot。

### 传输方式

```bash
# stdio（默认）：MCP 客户端以子进程方式拉起,协议走 stdin/stdout
kkagent mcp serve

# Streamable HTTP(本地端口,方便远程/网页客户端连接)
kkagent mcp serve --http                    # 默认 127.0.0.1:8788
kkagent mcp serve --http 0.0.0.0:9000 --http-token <token>

# 后台运行(与 `kkagent server` 相同的 daemon 模式)
kkagent mcp serve --daemon --http           # 立即返回,日志在 ~/.kkagent/mcp-http-daemon.log
kkagent mcp status                          # 查看状态(pid/地址/tunnel)
kkagent mcp status --json                   # 机器可读
kkagent mcp stop                            # 停止(SIGTERM,会一并停掉 tunnel-client)
```

HTTP 模式下 MCP 客户端 `POST http://<addr>/mcp`（JSON-RPC,响应为 `application/json`；通知返回 `202`）,`GET /healthz` 免认证探活。Bearer 认证必须：`--http-token` > `KKAGENT_MCP_HTTP_TOKEN` 环境变量 > 复用 `~/.kkagent/http_token`（与 `kkagent server --http` 共用,0600 权限）。HTTP 端口能被路由到的任何进程访问——kkagent mcp 可执行任意委派编码任务,因此未配置 token 时拒绝启动；绑定非回环地址前请确认网络边界。浏览器带 `Origin` 的请求仅接受 loopback；无 Origin 的非浏览器客户端（含 tunnel-client）不受影响。

`kkagent mcp serve` 启动时会连接（或自动拉起）本机 standalone `kkagent server`（与 TUI 相同的 UDS）。`delegate` 在该 server 上创建 session 并跑 AgentLoop，因此可用 TUI `--resume <session_id>` **直播**同一会话；turn 进行中 TUI 侧再发 prompt 会因 busy 被拒绝。协议版本协商支持 `2024-11-05` / `2025-03-26` / `2025-06-18`（故意保持兼容，不强制更新 transport）。

`write_plan` 将计划正文落盘为 markdown（`~/.kkagent/mcp-plans/`）。`delegate` 经 `sessions.create` 拿到真实的 `plan_file_path` 后拷到该确切路径；Agent 侧 `Read` 仅对该路径做只读特例（不放开整个 session 目录），对齐默认 Plan 模式；**不会**把全文灌进首条 user message。

### OpenAI Secure MCP Tunnel（接入 ChatGPT）

Secure MCP Tunnel 让 **ChatGPT / Codex / Responses API** 调用你本机的 `kkagent mcp serve`，**不需要**把 MCP 端口暴露到公网：本机只出站访问 `api.openai.com:443`，OpenAI 产品走托管的 tunnel 端点，由 `tunnel-client` 把请求转到本地 HTTP MCP。

官方总览：[Secure MCP Tunnel](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)；操作细节也可对照 [tunnel-client end-user guide](https://github.com/openai/tunnel-client/blob/master/docs/end-user-guide.md)。

#### 1. 准备权限与三个值

| 值 | 从哪里拿 | 用途 |
| --- | --- | --- |
| `tunnel_id`（形如 `tunnel_` + 32 位 hex） | [Platform → Tunnels](https://platform.openai.com/settings/organization/tunnels)，或 `tunnel-client admin tunnels create` | ChatGPT 与 `tunnel-client` / `kkagent --tunnel` 必须用**同一个** id |
| `CONTROL_PLANE_API_KEY`（`sk-...`） | [Platform → API keys（Runtime）](https://platform.openai.com/settings/organization/api-keys) | 长期守护进程鉴权（`tunnel-client run` / `kkagent --tunnel`） |
| `OPENAI_ADMIN_KEY`（可选） | [Platform → Admin keys](https://platform.openai.com/settings/organization/admin-keys) | **仅**用于 CLI 创建/改/删 tunnel，**不要**塞进长期 daemon |

权限拆分（Platform **组织级**，不是 project 级）：

- **创建 / 编辑 tunnel**：Tunnels **Read + Manage**
- **跑 tunnel-client / 在 ChatGPT 里选 tunnel**：Tunnels **Read + Use**
- 建 Runtime key 时选 **Restricted**，勾选 Tunnels **Read + Use**；不要用 Admin key 或 “All” 当长期密钥
- ChatGPT **开发者模式**是另一套 workspace 权限：Enterprise/Edu 需 workspace 管理员开通，用户再在 Settings → Security and login 里打开（见 OpenAI Help Center 的 developer mode 说明）

角色 / 组： [Organization roles](https://platform.openai.com/settings/organization/people/roles)、[groups](https://platform.openai.com/settings/organization/people/groups)。新角色生效可能需要最多约 30 分钟。

#### 2. 申请（创建）tunnel，并挂上 ChatGPT workspace

1. 打开 [Platform tunnel settings](https://platform.openai.com/settings/organization/tunnels)，选中正确的 Platform organization。
2. **Create tunnel**，填名称 / 描述。
3. **关联（association）**——这一步决定 ChatGPT 里能不能看到它：
   - 勾选（或添加）**管理该 tunnel 的 Platform organization**；
   - 勾选（或添加）**要用它的 ChatGPT workspace**（Business / Enterprise / Edu 等）；
   - 若 Codex / Responses API 会从**另一个** Platform org 调用，也把那个 org 加进去。
4. 保存后复制 `tunnel_id`。

也可用 Admin key 脚本化创建（需已有 org / workspace id）：

```bash
export OPENAI_ADMIN_KEY=...
tunnel-client admin tunnels create \
  --name "kkagent local" \
  --description "Routes ChatGPT to local kkagent mcp serve" \
  --organization-id <ORG_ID> \
  --workspace-id <CHATGPT_WORKSPACE_ID>
```

注意：

- 只关联个人 Platform org、**没有**目标 ChatGPT workspace 时，Enterprise/Edu 的 workspace 选择器里通常**不会**出现该 tunnel。
- Platform org 与 ChatGPT workspace 需能被 OpenAI 校验为同一实体；企业侧自动关联失败时，需联系 OpenAI account team 做人工 mapping（客户侧无法强制绑定）。

#### 3. 创建 Runtime API key

1. 打开 [Runtime API keys](https://platform.openai.com/settings/organization/api-keys)。
2. Create → **Restricted** → Tunnels **Read** + **Use**。
3. 复制 `sk-...`，仅放进环境变量 / secret store，不要写进 argv 或 git。

#### 4. 安装 tunnel-client

```bash
# macOS
brew install openai/tools/tunnel-client

# 其他平台：Platform Tunnels 页的下载链接，或
# https://github.com/openai/tunnel-client/releases/latest
tunnel-client --version
tunnel-client help quickstart   # 可选自检路径
```

本机还需能出站访问 `api.openai.com:443`（若配置 control-plane mTLS 则为 `mtls.api.openai.com:443`），并访问本地 kkagent HTTP MCP。

#### 5. 启动 kkagent（推荐一条命令）

先确保本机已有可用的 kkagent 配置（`~/.kkagent/config.toml`）、trusted workspace，以及 HTTP bearer token（`--http-token` / `KKAGENT_MCP_HTTP_TOKEN` / 默认 `~/.kkagent/http_token`）。

```bash
export CONTROL_PLANE_API_KEY=sk-...          # 上一步的 Runtime key
kkagent mcp serve --tunnel tunnel_xxxxxxxxxxxx  # 隐含 --http，默认 127.0.0.1:8788
```

可选：

```bash
# 指定 tunnel-client 路径 / 监听地址 / token
kkagent mcp serve --tunnel tunnel_xxx \
  --tunnel-client /path/to/tunnel-client \
  --http 127.0.0.1:8788 \
  --http-token "$(cat ~/.kkagent/http_token)"

# 后台（日志 ~/.kkagent/mcp-http-daemon.log）
kkagent mcp serve --daemon --tunnel tunnel_xxx
kkagent mcp status
kkagent mcp stop    # 会一并停掉 tunnel-client
```

kkagent 会：

1. 先起本地 Streamable HTTP MCP（`/mcp` + `/healthz`）；
2. 再以子进程跑 `tunnel-client`，把本地 `http://127.0.0.1:<port>/mcp` 挂到该 `tunnel_id`；
3. 自动注入 `MCP_EXTRA_HEADERS` / `MCP_DISCOVERY_EXTRA_HEADERS`（Bearer）与 `CONTROL_PLANE_POLL_CHANNELS=main`。

启动失败（缺 API key、找不到 binary、tunnel-client 启动窗口内退出）会让整个 `mcp serve` 失败；运行中断线仅警告，本地 HTTP 仍可用。

手动跑 tunnel-client（一般不必）时，HTTP MCP 示例：

```bash
export CONTROL_PLANE_API_KEY=sk-...
export KKAGENT_MCP_HTTP_AUTH="Bearer $(cat ~/.kkagent/http_token)"
# 先单独起：kkagent mcp serve --http
tunnel-client run \
  --control-plane.tunnel-id tunnel_xxx \
  --mcp-server-url http://127.0.0.1:8788/mcp
# 仍需自行设置 MCP_EXTRA_HEADERS / MCP_DISCOVERY_EXTRA_HEADERS；
# env: 引用必须是整个 header 值，例如 Authorization: env:KKAGENT_MCP_HTTP_AUTH
```

#### 6. 把 tunnel 挂到 ChatGPT

在 **`kkagent mcp serve --tunnel ...` 保持运行**（`/healthz` 正常）的前提下：

1. 用目标 workspace 登录 [ChatGPT](https://chatgpt.com)。
2. 确认账号已开 **developer mode**（Settings → Security and login；企业需管理员授权）。
3. 打开连接器 / 插件设置之一：
   - [Connectors](https://chatgpt.com/#settings/Connectors)，或
   - ChatGPT Plugins → **+** 新建 developer-mode app。
4. **Connection** 选 **Tunnel**。
5. 在列表中选中你的 tunnel，或粘贴同一个 `tunnel_id`。
6. 保存后，在对话里启用该 connector / app，即可调用 kkagent 暴露的工具（`write_plan`、`delegate`、`get_progress` 等）。

创建或发现 connector 时 tunnel-client **必须在线**；停掉 `kkagent mcp serve` 后工具调用会失败。

Codex / Responses API：在 MCP tool 定义里传 `tunnel_id`（不要把 OpenAI 托管 tunnel URL 填进 `server_url`）。示例见 [官方 Secure MCP Tunnel 文档](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)。

#### 7. 建议验收顺序

1. 本机：`curl -sS -H "Authorization: Bearer <token>" http://127.0.0.1:8788/healthz`
2. （若单独跑 tunnel-client）打开其 `/readyz` 与 `/ui`，确认 healthy / ready / connected
3. ChatGPT：Connectors 里能看到并选中 tunnel → 对话里能 `list_workspaces` / `get_context`
4. 完整链路：`write_plan` → `delegate` → `get_progress` → `get_result`；可用 TUI `kkagent --resume <session_id>` 直播同一 session

#### 8. 常见问题

| 现象 | 排查 |
| --- | --- |
| Platform 提示 “Tunnels access required” | 组织选错，或角色缺 Read/Manage/Use；等角色传播后再试 |
| Platform 有 tunnel，ChatGPT 列表没有 | tunnel 未关联目标 **ChatGPT workspace**；操作者缺 Tunnels **Use**；daemon 未 ready；新建后稍等传播 |
| ChatGPT 能选 tunnel 但工具 401 / discover 失败 | 检查 kkagent bearer token；缺 `MCP_DISCOVERY_EXTRA_HEADERS`（kkagent 托管模式已自动设） |
| `kkagent mcp serve --tunnel` 立刻退出 | `CONTROL_PLANE_API_KEY`、tunnel id、出站 `api.openai.com:443`、`tunnel-client doctor` / 日志 |
| 企业 workspace 无法关联 Platform org | 联系 OpenAI account team 做人工 association，客户无法自助强制绑定 |

#### 相关链接

- [Platform Tunnels](https://platform.openai.com/settings/organization/tunnels)
- [Runtime API keys](https://platform.openai.com/settings/organization/api-keys)
- [ChatGPT Connectors](https://chatgpt.com/#settings/Connectors)
- [openai/tunnel-client](https://github.com/openai/tunnel-client) / [Releases](https://github.com/openai/tunnel-client/releases/latest)
- [OpenAI Secure MCP Tunnel guide](https://developers.openai.com/api/docs/guides/secure-mcp-tunnels)

## Skills

Skill 是一个目录中的 `SKILL.md`。发现顺序包括：

1. `~/.kkagent/skills/<name>/SKILL.md`；
2. `.kkagent/skills/<name>/SKILL.md`；
3. 兼容目录 `.agents/skills/` 和 `.kimi/skills/`；
4. 工程根目录 `AGENTS.md` 和 `.kkagent/AGENTS.md` 作为项目指令。

在工程目录运行 `kkagent --dump-system-prompt` 可确认 Skill 目录段和项目指令是否被注入。

最小示例：

```markdown
# rust-review

当用户要求审查 Rust 代码时，先运行 cargo fmt --check 和 cargo clippy，
再按正确性、安全性、跨平台和测试覆盖率输出问题。
```

Skill 可使用简单 frontmatter：

```markdown
---
name: rust-review
description: Review Rust changes before release
version: 1.0.0
triggers: [review, release]
---

按 references/checklist.md 执行检查。
```

Skill 名只能包含 ASCII 字母、数字、`-`、`_`。`SKILL.md` 最大 256 KiB；目录中的资源会被列给模型，`Skill` 工具可用 `resource` 参数读取最大 1 MiB 的 UTF-8 文本资源，并阻止绝对路径、`..` 和符号链接逃逸。每次列出或加载都会重新扫描，因此编辑无需重启。

同名优先级为项目 `.kkagent` > 项目 `.agents` > 项目 `.kimi` > `extra_skill_dirs` > 用户目录。独立 Server 会按每个 Session workspace 单独发现。Skill 不会绕过工具权限；`merge_all_available_skills = true` 会增加初始上下文占用。

## Hooks

除 TOML 配置外，还会发现：

- `~/.kkagent/hooks.json`；
- `<workspace>/.kkagent/hooks.json`。

JSON 示例：

```json
[
  {
    "event": "pre_tool_call",
    "command": "/absolute/path/to/policy-check",
    "args": ["--strict"],
    "timeout_ms": 5000
  }
]
```

Hook 进程工作目录是当前 workspace，并收到 `KKAGENT_HOOK_EVENT` 和 `KKAGENT_HOOK_CONTEXT`。stdout 可返回 JSON：

```json
{"block": true, "reason": "production deploy is disabled"}
```

或返回 `{"rewrite": {...}}` 改写上下文。TOML、用户 JSON 和项目 JSON 会合并；项目 Hook 按 Session workspace 动态读取。`matcher` 支持 `Bash`、`mcp_*` 等模式。`pre_tool_call` 启动失败、非零退出或超时会阻断工具；stdout/stderr 会持续排空并各限制为 64 KiB，timeout 最大 300 秒。Hook 本身是本机可执行代码，项目 Hook 只应在可信仓库中启用。

## 插件

> 面向插件作者的上手指南与 manifest 字段参考见[插件开发指南](plugin-development.md)；
> 本节是机制原理参考。

插件从 `~/.kkagent/plugins/<directory>/` 发现，也兼容
`~/.kkagent/plugins/managed/<id>/` 布局。Manifest 按以下优先级读取：

1. `kk.plugin.json`；
2. `.kk-plugin/plugin.json`；
3. 旧版 kkagent `plugin.json`。

KK plugin 使用 `mcpServers` 字段声明工具服务。最小示例：

```json
{
  "name": "code-search",
  "version": "0.1.0",
  "description": "Remote source search",
  "systemPrompt": "Use CodeSearch to locate remote source before reading local files.",
  "mcpServers": {
    "search": {
      "transport": "stdio",
      "command": "python3",
      "args": ["./scripts/mcp_server.py"],
      "cwd": "./"
    }
  }
}
```

stdio `command` 必须是 PATH 中的命令，或以 `./` 开头、位于插件根目录内的文件；
`cwd` 同样必须以 `./` 开头且不能通过 `..` 或符号链接逃逸插件目录。未填写
`cwd` 时默认使用插件根目录。运行时会注入 `KKAGENT_HOME` 和
`KKAGENT_PLUGIN_ROOT`。

Plugin MCP server 使用 `plugin-<plugin-id>:<server-name>` 作为运行时名称，避免与
`config.toml` 中的 MCP server 冲突。kkagent 启动时自动连接；每个 Agent turn 构建工具
注册表时都会读取当前 MCP 工具集，因此新 session 自动获得插件工具。修改或安装插件后
执行 `/plugins reload`，会重新扫描 manifest、重启 MCP 连接，并让后续 turn 使用新的工具。

暴露给模型的工具名使用缩短的命名空间：单 server 插件为 `mcp__<plugin-id>__<tool>`，
多 server 插件为 `mcp__<plugin-id>_<server-name>__<tool>`（例如
`mcp__rk-codesearch__CodeSearch`）。命名空间超过 32 字符或不同插件产生相同命名空间时，
会追加稳定的短哈希后缀消歧。运行时名称（`/mcp`、`disabled.toml`、OAuth 凭据存储）
不受影响，仍使用完整形式。

旧版 `prompt_append` 仍兼容，等价于 `systemPrompt`。插件 MCP 可通过 `/mcp` 以其运行时
名称启用或禁用，状态保存在 `~/.kkagent/disabled.toml`。插件进程是本机可执行代码，只应
安装可信插件；损坏的 MCP 声明会作为 `/plugins` diagnostics 展示，并且不会阻止其他插件加载。

### 插件 Override

插件可以覆盖内置工具和替换基础系统提示词，用于深度定制（如把 `Web` 换成 Tavily
MCP、发行版自定义 persona）：

```json
{
  "name": "kk-web-tavily",
  "version": "1.0.0",
  "description": "Replace built-in Web with Tavily MCP",
  "systemPrompt": "You are MyAgent...",
  "replaceSystemPrompt": true,
  "toolOverrides": {
    "Web": "tavily.tavily_search"
  },
  "mcpServers": {
    "tavily": { "command": "npx", "args": ["-y", "tavily-mcp"] }
  }
}
```

- **`toolOverrides`**：`内置工具名 → "<server>.<tool>"`（server 必须属于本插件的
  `mcpServers`）。应用时机是工具注册表构建的最后一步：桥接后的 MCP 工具会以内置
  工具的原始名字（如 `Web`）注册，内置实现被完全替换，对模型不可见。子代理 profile
  按名字过滤工具，因此替换自动对子代理生效。替换采用**身份继承**语义：wire 名、渐进
  披露（Inline/Deferred）、只读判定、审批规则与被覆盖工具完全一致，只有描述、参数
  schema 与执行逻辑来自替换者。
- **`services`**：服务后端覆盖（免 MCP）。`webSearch`/`webFetch` 字段与 `config.toml`
  的 `[services.web_search]`/`[services.web_fetch]` 同构，启用即整体替换用户配置。
  适合"只换搜索 provider"这类场景：不换工具实例，schema/权限/披露零变化，不拉起
  任何进程。多插件声明同一服务按插件名字典序取第一个。详见
  [插件开发指南](plugin-development.md#服务覆盖免-mcp)。
- **`replaceSystemPrompt`**：`systemPrompt` 从「追加」变为「替换基础 persona」。
  workspace 注入（AGENTS.md）、skills、其他插件的追加段仍然叠加在后面。主会话与
  子代理统一生效。多个插件声明替换时按插件名字典序取第一个，其余记入日志警告。

**策略边界**（`kkagent-config/src/plugin_policy.rs`）。内置工具共 19 个，全部列出：

**永不可覆盖（4 个）**——权限/计划模式守卫按工具名硬编码，替换会绕过安全检查，即使写入 `extra_overridable_tools` 也会被拒绝：

| 工具 | 原因 |
|---|---|
| `AskUserQuestion` | auto 模式强制 deny 守卫（防止无人值守会话卡在提问） |
| `EnterPlanMode` | 计划模式入口守卫 |
| `ExitPlanMode` | 计划模式审批出口守卫 |
| `Goal` | 目标生命周期与预算守卫 |

**默认可覆盖（5 个）**——低风险/纯数据面工具，manifest 声明即可替换：

| 工具 | 说明 |
|---|---|
| `Web` | web search/fetch（最常见的 override 目标） |
| `TaskOutput` | 后台任务/子代理结果查询 |
| `Skill` | skill 加载 |
| `Cron` | 定时任务管理 |
| `ReadMediaFile` | 媒体文件读取 |

**需配置显式开启（10 个）**——文件写入、命令执行、代理与内部机制类工具，须在 `config.toml` 中 opt-in：

```toml
[plugins]
extra_overridable_tools = ["Bash"]  # 可列多项
```

| 工具 | 风险点 |
|---|---|
| `Bash` | 绕过沙箱/shell AST 安全检查/path policy |
| `Edit` | 绕过路径守卫与文件冲突检测 |
| `Write` | 同上 |
| `Read` | 绕过敏感文件检查与工作区边界（只读，风险较低） |
| `Grep` / `Glob` | 绕过工作区路径边界 |
| `TodoList` | 低风险，但与 TUI 状态展示联动 |
| `Agent` | 子代理启动（权限模式随宿主，但描述/协议可能被替换） |
| `WritePlan` | 计划模式配套（覆盖后计划文件写入流程失效） |
| `SelectTools` | 延迟工具加载机制（覆盖可能破坏 MCP 工具发现） |

MCP 工具（`mcp__*` 命名空间）不属于内置工具：`toolOverrides` 的目标是内置工具名，对
`mcp__*` 名字默认同样被策略拒绝，如确需替换另一插件的 MCP 工具也要走
`extra_overridable_tools` 显式 opt-in。

被策略拒绝或源工具不可用（server 未连接/被禁用）的 override 会**自动回退**到内置实现，
并在日志中输出诊断，不会让工具消失。

### 插件 Slash 命令

`slashCommands` 支持完整命令定义，TUI 中以 `/plugin:<name>` 形式补全和执行：

```json
"slashCommands": [
  {
    "name": "search",
    "description": "Search the web",
    "argumentHint": "<query>",
    "promptTemplate": "Search the web for {{args}} and summarize the top results."
  }
]
```

执行时模板中的 `{{args}}`（全部参数）和 `{{arg0}}`、`{{arg1}}`…（逐词）展开为用户
输入，渲染结果作为普通用户消息提交给 agent。旧版纯命令名列表仍兼容，仅出现在补全中。

### 插件外部子 Agent（ACP 与 internal）

`subagents` 字段把外部能力注册为新的子 Agent 类型，kkagent 两种执行方式：
`transport: "acp"`（外部 agent 进程，典型 Cursor CLI 的 `agent acp` 模式，stdio
JSON-RPC 驱动）与 `transport: "internal"`（进程内 kk agent loop：kk 模型 + 工具
allowlist + 插件私有 MCP，委派时懒加载、主会话零上下文成本，适合接入工具很多的
wiki/知识库）。类型以限定名 `<plugin-id>.<name>` 进入 `Agent` 工具的 profile 枚举，
同步委派、后台运行 + `TaskOutput` 轮询、取消均与内建子 agent 一致，事件镜像一致。
字段参考与完整示例见
[插件开发指南](plugin-development.md#外部子-agentacp-与-internal)、
`plugins/official/kk-cursor-agent/`（acp）与 `plugins/official/kk-wiki-agent/`
（internal）。


### 插件市场

顶层 `plugin_marketplace` 配置默认市场（本地路径、`file://` 或 HTTP(S) URL），
`KKAGENT_PLUGIN_MARKETPLACE_URL` 只覆盖这一项。多个市场用 `plugin_marketplaces`：

```toml
plugin_marketplace = "https://plugins.example.com/marketplace.json"
plugin_marketplaces = [
  "http://10.10.10.205:8091/bjc/kk-plugins",
  { name = "team", source = "/data/kk-plugins/marketplace.json" },
]
```

两者都未设置时，如果 `~/.kkagent/plugins/marketplace.json` 存在则自动使用。
通过 `/plugins` 弹窗添加的市场仍保存在 `~/.kkagent/plugins/marketplaces.json`。
Marketplace JSON 至少包含 `id` 和 `source`：

```json
{
  "version": "1",
  "plugins": [
    {
      "id": "code-search",
      "tier": "curated",
      "displayName": "Code Search",
      "version": "1.2.0",
      "description": "Search remote source indexes",
      "keywords": ["code-search"],
      "source": "./code-search"
    }
  ]
}
```

本地 marketplace 的 `source` 支持相对目录或 ZIP、绝对路径和 `file://`；远程
marketplace 的相对 `source` 应指向 ZIP。也支持普通 HTTP(S) ZIP，以及 GitHub /
GitBucket 等 GitHub 兼容 forge 的仓库、`tree/<ref>`、`tree/<ref>/<subdir>`、
release tag 和 commit URL。多插件单体仓库用 `tree/<branch>/<plugin-dir>` 指向子目录；
安装时下载 `{repo}/archive/<ref>.zip`（GitHub 走 `codeload.github.com`）并只取该子目录。
若 marketplace 配置的是仓库首页（HTML），会自动尝试
`raw/main/marketplace.json` 与 `raw/master/marketplace.json`。安装内容先进入临时目录，
验证 `kk.plugin.json` 后复制到 `~/.kkagent/plugins/managed/<id>/`，再原子更新
`~/.kkagent/plugins/installed.json`；失败时恢复原版本。ZIP 下载限制为 64 MiB、解压后
限制为 256 MiB/10000 个文件，并拒绝路径逃逸与符号链接。

直接执行 `/plugins` 会打开多级管理弹窗：首页可进入已安装插件、插件市场，也可添加
marketplace、从本地目录/ZIP/GitHub 来源安装或重新加载。选择 marketplace 后会先显示
插件列表，再进入插件详情执行安装或更新；已安装插件详情支持启用、禁用、更新和带确认
的移除。通过弹窗添加的 marketplace 会验证后保存到
`~/.kkagent/plugins/marketplaces.json`，下次启动仍然可用。配置文件中的
`plugin_marketplace` / `plugin_marketplaces` 以及环境变量指定的默认 marketplace
也会显示在同一列表中（配置来源不可从弹窗删除）。

```text
/plugins marketplace [source]
/plugins install <marketplace-id-or-source>
/plugins update <id>
/plugins enable <id>
/plugins disable <id>
/plugins remove <id>
/plugins info <id>
/plugins reload
```

这些子命令保留用于脚本和快速操作；不带参数的 `/plugins` 是推荐的交互入口。

`remove` 删除安装记录但保留托管副本，重新安装即可恢复。安装和更新会执行插件声明的
本机程序，因此只能使用可信 marketplace 和插件源。
