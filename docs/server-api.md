# Agent Server API

Agent Server 让 TUI、Node.js、编辑器或自动化程序复用同一后台。它同时提供本地 RPC endpoint 和可选的 HTTP/WebSocket API。

## 启动

```bash
export KKAGENT_HTTP_TOKEN="replace-with-a-long-random-token"
kkagent server --listen ~/.kkagent/server.sock --http 127.0.0.1:8787
```

也可用 `--http-token` 传 token。两者都未提供时，Server 会生成本次进程使用的高熵 token 并打印到 stderr。HTTP/WS 不应在无反向代理、TLS 和网络访问控制的情况下监听公网地址。

本地客户端连接：

```bash
kkagent --connect ~/.kkagent/server.sock
```

`POST /api/v1/fs` 和 terminal API 默认关闭。确实需要时显式启用：

```bash
kkagent server --http 127.0.0.1:8787 \
  --allow-fs-write-api \
  --allow-terminal-api \
  --http-rate-limit 600
```

## 认证

主 token 拥有 `admin` scope：

```http
Authorization: Bearer <token>
```

也兼容 `?token=<token>`，主要用于不方便设置 Header 的 WebSocket 客户端。查询参数可能进入日志和代理记录，普通 HTTP 请求应优先使用 Header。

还可用环境变量配置最小权限 token：

| 环境变量 | scope |
|---|---|
| `KKAGENT_HTTP_READ_TOKEN` | GET、WebSocket、health、metrics。 |
| `KKAGENT_HTTP_WRITE_TOKEN` | read + Session、消息、批准和直接文件写等非 terminal 写操作。 |
| `KKAGENT_HTTP_TERMINAL_TOKEN` | read + terminal；不能创建 Agent Session。 |

每个 token 默认每分钟最多 600 个请求；`--http-rate-limit 0` 可关闭。请求会获得 `x-request-id`，脱敏 token 指纹、方法、路径、状态码和 request ID 默认写入 `~/.kkagent/http-audit.jsonl`。

## 最小流程

```bash
API=http://127.0.0.1:8787/api/v1
AUTH="Authorization: Bearer $KKAGENT_HTTP_TOKEN"

curl -sS -H "$AUTH" "$API/meta"

curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: session-20260809-001' \
  -d '{"workspace":"/absolute/path/to/project","title":"review"}' \
  "$API/sessions"

curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: job-20260809-001' \
  -d '{"text":"运行测试并总结失败"}' \
  "$API/sessions/<session-id>/messages"
```

发送消息先把 turn 原子写入 SQLite，再返回稳定的 `task_id`。相同 Session、相同 `Idempotency-Key` 和相同正文会返回原任务；同 key 不同正文返回 `409`。最终回答、工具调用、批准请求和状态变化通过 WebSocket 事件流返回。

### 创建会话的幂等性

`POST /sessions` 同样接受 `Idempotency-Key`（最长 256 字节）。网络超时后用同一个 key、同样的 `{workspace, title}` 重试，得到的是第一次创建的那个会话（响应里 `replayed: true`），不会多出一个孤儿会话；第一次调用的响应则带 `replayed: false`。同 key 不同参数返回 `409`；同 key 的另一次调用还在创建中也返回 `409`（稍后重试即可）。创建失败不占用 key；key 对应的会话已被删除时，重试会新建一个。key 保留 7 天。不带 key 的请求行为不变（响应没有 `replayed` 字段）。

### 服务实例标识

`GET /meta` 返回：

| 字段 | 含义 |
|---|---|
| `instance_id` | 这份数据（SQLite 数据库）的标识：创建数据库时生成，**跨重启不变**。换了一个 server 或数据库被清空，它就变了。 |
| `boot_id` | 本次进程启动的标识，**每次启动都变**。 |
| `started_at` | 本次进程启动时间（RFC 3339）。 |
| `latest_event_seq` | 当前最新事件序号。 |

宿主缓存 `instance_id`：`boot_id` 变而 `instance_id` 不变是普通重启，事件游标、`task_id`、审批 ID 仍然有效；`instance_id` 变了则本地缓存的 session、游标和任务都应视为失效。没有持久化数据库时 `instance_id` 等于 `boot_id`。

### 步骤事件与任务终态

事件流里的 `error` 表示本轮已经终止（随后是 `status_update: idle` 与 `turn_end`）；`notice` 是不终止本轮的人类可读警告（例如跨会话文件冲突），客户端只需展示，不应据此判定 turn 结束。旧客户端遇到未知事件类型时可直接忽略 `notice`。

**`turn_start` / `turn_end` 是模型步骤边界，不是任务边界。** 一个任务（一次 `POST …/messages`）内每一轮模型调用都会发 `turn_start`，目标模式下 `turn_end` 也可能出现多次。判断任务是否真正结束，等的是任务终态事件：

```json
{"type": "task_finished", "task_id": "…", "session_id": "…", "state": "completed", "error": null,
 "event_seq": 1234, "emitted_at": "…"}
```

`state` 是持久化的任务终态：`completed`、`failed` 或 `cancelled`（`failed` 时 `error` 给出原因）。它在该任务的所有其他事件（含 `turn_end`、`turn_committed`）之后发出，并且每个带 `task_id` 的任务恰好一次，进程重启后恢复执行的任务同样会发。`GET /turns/{task-id}` 返回同样的 `state`；`waiting_approval` / `running` 之类是中间状态。

### 审批事件与取消

`approval_requested` 事件带 `session_id` 与 `request.approval_id`；`POST /approvals/{id}` 的回执事件（`approval`）和取消事件（`turn_cancelled`）也都带 `session_id`，所以只订阅了某个会话（`?session_id=`）的 WebSocket 同样看得到它们。

`POST /approvals/{id}` 只投递给**正在等这个审批的那一个会话**。已经回答过、超时、被中断或从未存在的审批 ID 返回 `404`（响应说明 `expired or is no longer pending`），不再返回 `200`。成功响应里有 `session_id`。

审批 ID 在持久化任务内是稳定的：它由任务 ID、该任务内的审批序号、工具名和输入派生（`apr_…`），所以重启后恢复执行的同一个任务如果再次请求同一个审批，会得到**同一个** ID，宿主对重启前审批的回答仍然有效。没有持久化任务的路径（本地会话等）使用随机 UUID。

审批默认等待 900 秒，到期按拒绝处理；`[background] approval_timeout_s` 可调（上限 86400），`0` 表示一直等。

### 取消与后台命令

`DELETE /turns/{task-id}` 取消排队/运行/待审批的 turn。返回前会等 turn 真正放开会话（最多 3 秒），响应里说明实际结果：

```json
{"ok": true, "task_id": "…", "session_id": "…", "state": "cancelled",
 "turn_stopped": true, "cancel_background": false,
 "background": {"requested": [], "stopped": [], "still_running": ["bash-3"]}}
```

- `turn_stopped`：turn 是否已在等待窗口内释放会话（`false` 表示还在收尾，随后会发 `task_finished`）。
- 默认只取消 turn，**不**杀会话已有的后台 Bash 任务，`background.still_running` 列出仍在运行的任务 ID，由宿主决定。
- `DELETE /turns/{id}?cancel_background=true` 同时取消它们并等待最多 3 秒：`requested` 是收到取消请求的任务，`stopped` 是确认已停止的，`still_running` 是窗口结束时仍在运行的（比如杀不掉的远端任务）。**只有出现在 `stopped` 里的任务才算已被证明停止。**
- 不存在的任务返回 `404`，已结束的任务返回 `409`。

`session.interrupt`（HTTP 的 interrupt 与 TUI 的 `Esc` 同语义）只中断当前 turn，不会杀掉会话已有的后台 Bash 任务；需要同时结束它们时在参数里带 `"cancel_background": true`，响应里回显该值并附带同样结构的 `background` 报告。用户中断后，后台任务的完成通知不会自动唤醒模型，而是留到该会话的下一次输入。

### 空闲退出

独立 Server 在空闲 `[server].idle_timeout_secs`（默认 1800）后自动退出，退出会中止后台任务。下列情况默认**不会**触发空闲退出，每一类都能在 `[server]` 里关闭：

| 配置 | 持有条件 |
|---|---|
| `idle_hold_for_turns`（默认 `true`） | 有进行中的 turn |
| `idle_hold_for_approvals`（默认 `true`） | 有待决的审批或提问 |
| `idle_hold_for_background_tasks`（默认 `true`） | 有运行中的后台 Bash 或子 Agent |

另外，只要有本地客户端连接或打开着 HTTP WebSocket 事件流就不退出；**每个 HTTP 请求**都会重置空闲计时，所以只靠 HTTP 轮询的宿主不会被误判为空闲。需要由外部管理生命周期时把 `idle_timeout_secs` 设为 `0`。

## HTTP 路由

| 方法与路径 | 说明 |
|---|---|
| `GET /api/v1/meta` | 名称、版本、API、capability，以及 `instance_id` / `boot_id` / `started_at`。 |
| `GET /api/v1/health` | 存活、持久化状态、uptime 和 Session 数。 |
| `GET /api/v1/ready` | 可接流量时返回 200；降级或持久化失败返回 503。 |
| `GET /api/v1/metrics` | Prometheus 文本指标。 |
| `GET /api/v1/events?since=&session_id=&limit=` | 从 SQLite 回放持久事件；单次最多 10000 条。 |
| `GET /api/v1/turns/{task-id}` | 查询持久 turn 状态；也兼容 Session ID 查询最近一项。 |
| `DELETE /api/v1/turns/{task-id}?cancel_background=` | 取消排队/运行/待审批 turn，并中断对应 Session；响应含 `turn_stopped` 与后台任务清理报告。 |
| `GET /api/v1/sessions` | 列出会话。 |
| `POST /api/v1/sessions` | 创建会话；body 为 `{workspace, title?}`，`workspace` 必填；支持 `Idempotency-Key`。 |
| `GET /api/v1/sessions/{id}` | 读取会话。 |
| `DELETE /api/v1/sessions/{id}` | 删除会话。 |
| `POST /api/v1/sessions/{id}/messages` | 提交 `{text}`。 |
| `GET /api/v1/sessions/{id}/export` | 导出 JSON 会话。 |
| `POST /api/v1/approvals/{id}` | 提交 `{decision, feedback?, selected_label?, scope?}`；ID 不再待决时 `404`。 |
| `GET /api/v1/questions` | 列出待回答问题。 |
| `POST /api/v1/questions/{id}` | 提交问题响应 JSON。 |
| `GET /api/v1/tools`、`/tasks`、`/skills` | 当前能力和后台任务。 |
| `GET /api/v1/modelCatalog` | 模型目录；`/models` 是别名。 |
| `GET /api/v1/config` | 安全裁剪后的运行配置。 |
| `GET /api/v1/workspaces` | 工作区信息；`/workspaceFs` 是别名。 |
| `GET /api/v1/fs?path=&offset=&limit=&encoding=` | 读取可信工作区内的文件：UTF-8 文本或 base64 二进制，可按字节范围读，见[文件接口](#文件接口)。 |
| `POST /api/v1/fs` | 写入 `{path, content?}`。 |
| `GET /api/v1/files?path=&limit=&cursor=` | 分页列目录，每项带大小与修改时间。 |
| `GET /api/v1/search?q=...` | 搜索工作区。 |
| `GET /api/v1/snapshot` | 会话与工作区快照。 |
| `GET /api/v1/prompts` | 可用提示模板。 |
| `GET/POST /api/v1/terminals` | 列出或创建终端命令。 |
| `GET/DELETE /api/v1/terminals/{id}` | 轮询输出或停止并删除终端。 |
| `GET /api/v1/connections` | 可用实时连接。 |
| `GET /api/v1/ws` | WebSocket 事件流。 |

终端创建 body 为 `{command?, cwd?}`。API 必须用 `--allow-terminal-api` 开启并持有 `terminal` 或 `admin` scope。最多同时保留 64 个 HTTP terminal，命令最大 64 KiB，stdout 和 stderr 各最多保留约 1 MiB。

## 文件接口

`GET /api/v1/fs` 与 `GET /api/v1/files` 只访问 `trusted_workspaces`（未配置时为 Server 的当前目录）内的路径；相对路径相对 Server 当前目录，符号链接按真实路径判定。错误一律用 HTTP 状态码表达，body 为 `{"error": "…", "code": "…"}`，不会再用 `200` 包一个错误：

| 状态 | `code` | 场景 |
|---|---|---|
| `400` | `bad_request` | 路径为空、读目录/列文件、`offset` 超过文件长度、`encoding` 不认识 |
| `403` | `forbidden` | 路径在可信工作区之外（包括不存在的路径，防止探测），或系统拒绝访问 |
| `404` | `not_found` | 可信工作区内没有这个文件或目录 |
| `413` | `too_large` | 文件超过单次读取上限且没有指定 `limit` |
| `415` | `not_text` | 请求 `encoding=utf8`（默认）但内容不是合法 UTF-8 |
| `500` | `internal` | 其他 I/O 错误 |

**读取文件**（`GET /api/v1/fs`）：

| 参数 | 含义 |
|---|---|
| `path` | 文件路径（必填） |
| `offset` | 起始字节，默认 `0` |
| `limit` | 最多读多少字节。不给：读到文件末尾，但超过 4 MiB 的文件返回 `413`（不会悄悄截断）；给了：超过 4 MiB 按 4 MiB 处理，部分读取是正常结果；`0` 只返回元数据 |
| `encoding` | `utf8`（默认）或 `base64`（二进制文件用这个） |

```json
{"path": "src/main.rs", "encoding": "utf8", "content": "…", "offset": 0, "bytes_read": 4096,
 "size": 20480, "modified_at": "2026-08-09T10:00:00+00:00", "truncated": true, "next_offset": 4096}
```

`truncated: true` 时从 `next_offset` 继续读。`utf8` 范围读取在字符边界处收尾：落在多字节字符中间的范围会少读那个字符，`next_offset` 指向它的起点；`offset` 本身必须落在字符边界上（顺着 `next_offset` 读总是满足）。`base64` 没有这个限制。

**列目录**（`GET /api/v1/files`）按名称字节序排序、分页：`limit` 默认 200、最大 1000，`cursor` 取上一页的 `next_cursor`（上一页最后一项的名字）。

```json
{"path": ".", "total": 512, "count": 200, "truncated": true, "next_cursor": "lib.rs",
 "entries": [{"name": "Cargo.toml", "path": "/…/Cargo.toml", "is_dir": false, "type": "file",
              "size": 1024, "modified_at": "2026-08-09T10:00:00+00:00"}]}
```

`type` 为 `file` / `dir` / `symlink` / `other`；目录和符号链接的 `size` 为 `null`。`truncated` 为 `true` 时用 `next_cursor` 取下一页，页与页之间目录发生变化也不会重复或漏项（游标是名字而不是下标）。

## WebSocket

连接示例：

```text
ws://127.0.0.1:8787/api/v1/ws?token=<url-encoded-token>&session_id=<id>&since=<seq>
```

连接后先收到 hello，其中包含 `latest_event_seq` 和实时广播窗口 `history_capacity`。每个事件包含由 SQLite 分配、跨进程重启保持单调的 `event_seq` 和 `emitted_at`。`since` 从持久事件表回放，`session_id` 在服务端过滤。发送 `{"type":"subscribe","session_id":"..."}` 可设置过滤；发送包含 `ping` 的文本会收到 pong。

WebSocket 是广播流；慢客户端会收到 `resync_required`，随后应从最后确认的序号调用 events、turn 和 snapshot 恢复。2048 是实时 channel 的内存窗口，不是持久历史上限。

## 本地 RPC

默认 TUI 使用进程内 memory transport。独立 Server 在 Unix 使用 UDS；Windows endpoint 文件指向本机 loopback 端口。内部 RPC 覆盖会话创建、恢复、prompt、中断、模式/模型切换、compact、undo、历史轮次枚举与精确分叉、usage、skills、plugins、tasks、cron、approval、question 和 swarm 等操作。

这是内部版本化接口；外部 Node 集成优先使用 HTTP/WS SDK，编辑器集成优先使用 ACP。

## ACP 外部 agent 模型

已启用插件里声明了 `transport: "acp"` 的子 Agent（如官方 `kk-cursor-agent`）可以作为**会话后端**使用，模型别名为 `acp:<插件 id>.<agent 名>`，例如 `acp:kk-cursor-agent.cursor`。别名只在运行时有效：不需要 `config.toml`，`[models]` 里也不允许使用以 `acp:` 开头的别名。

- **发现**：`GET /api/v1/modelCatalog`（别名 `/models`）与 `kkagent acp` 桥的 `models/list` 会在配置模型之后追加 ACP 项，按 `kind == "acp"` 区分：

  ```json
  {"id": "acp:kk-cursor-agent.cursor", "alias": "acp:kk-cursor-agent.cursor",
   "model": "kk-cursor-agent.cursor", "provider": "acp", "kind": "acp",
   "display_name": "cursor", "description": "Cursor CLI agent driven over ACP …"}
  ```

  本地 RPC `plugins.list` 里每个插件带 `acp_agents: [{id, agent, name, description}]`；插件被禁用时该数组为空，ACP 项随之消失。
- **思考等级**：别名可以带后缀 `#<等级>`：`acp:kk-cursor-agent.cursor#max`。等级只能由字母、数字和 `-` `_` `.` 组成（最长 32 字符），否则别名无效；没有后缀就不改外部 agent 自己的设置。后缀属于别名本身，所以 `session.set_model`、会话持久化和 `session_config_changed` 里的 `model` 都带着它；`/model` 列表里的 ACP 项永远不带后缀。每个回合开始时，kkagent 把后缀里的等级设给外部 agent（取它提供的最接近的一档，设不上时只提示一次）。
- **选择**：本地 RPC `session.set_model` 和 `kkagent acp` 桥的 `session/set_model` 接受该别名（可带上述后缀）；对应插件不存在或已禁用时返回可读错误（提示改用 `/model` 选择其它模型）。持久化会话里保存的 `acp:` 别名在插件被禁用后，下一个回合起点同样给出该错误，切到普通模型不受影响。
- **运行**：此后该会话的每个 `session.prompt` 回合由外部 agent 执行，事件流与普通会话一致：`message_delta`、`thinking_delta`、`tool_call` / `tool_result`（`tool_name` 为 `<agent>/<kind>`，如 `cursor/execute`）、`todo_updated`、`plan_file_updated`、`turn_end`。外部 agent 的权限请求、提问和计划评审以普通的 `approval_requested` / `question_asked` 事件出现，用 `approval.respond` / `question.respond` 回答；计划评审是 `tool_name: "ExitPlanMode"` 的审批，`selected_label` 取 `执行` / `修改意见` / `拒绝`。被用户或策略拒绝的工具调用以 `is_error: true` 的 `tool_result` 结束。权限请求是否出现取决于外部 agent 自己的审批设置。
- **`acp_agent_state` 事件**：每个 ACP 回合开始时（工具与文本事件之前）发一次，描述外部 agent 此刻的状态：

  ```json
  {"type": "acp_agent_state", "session_id": "…", "agent": "cursor",
   "model": "claude-sonnet-5-5", "thought_level": "high",
   "thought_levels": ["low", "medium", "high", "xhigh", "max"],
   "note": "…"}
  ```

  `model`、`thought_level`、`thought_levels`、`note` 没有内容时省略；没有 `thought_levels` 表示 agent 不让客户端选择等级（等级只能在 agent 自己的配置里改）。`note` 只在值得告诉用户时出现（请求的等级被换成最接近的一档、agent 不支持或拒绝了设置），同一个请求不会重复提示。旧客户端可以忽略这个事件。
- **steer**：运行中的 `session.steer` 照常返回 `steered: true`，但外部 agent 没有「运行中插话」，内容会排队到本轮结束后作为下一轮发送。`session.interrupt` 先发 `session/cancel`，8 秒内没有响应则回收外部进程。
- **转录**：每个 ACP 回合写入用户消息、助手最终文本和一条 harness-only 的活动摘要，不含 `ToolUse` 块，所以同一个会话随时可以换回普通模型（反之外部 agent 会收到它没见过的消息作为前言）。
- **撤销警告**：被 `session.undo` / `session.restore` 回退的回合里含外部 agent 的回合时，响应多一个 `warnings: [string]`（外部 agent 改动的文件不会回退）；没有这类回合时不出现该字段，旧客户端不受影响。

## 错误处理

- `401`：token 缺失或错误。
- `403`：scope 不够，或文件路径在可信工作区之外。
- `404`：会话、终端、任务或批准 ID 不存在（包括已不再待决的审批），文件接口里指文件不存在。
- `400`：请求字段、路径或后端操作无效。
- `409`：幂等键冲突、任务不可取消或 Session 正忙。
- `413`：终端命令超过限制，或文件超过单次读取上限。
- `415`：文件不是合法 UTF-8（改用 `encoding=base64`）。
- `429`：终端数量达到上限。

客户端应设置请求超时、记录响应 body，并把提交消息和等待最终事件视为两个阶段。
