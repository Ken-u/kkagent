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
  -d '{"workspace":"/absolute/path/to/project","title":"review"}' \
  "$API/sessions"

curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -H 'Idempotency-Key: job-20260809-001' \
  -d '{"text":"运行测试并总结失败"}' \
  "$API/sessions/<session-id>/messages"
```

发送消息先把 turn 原子写入 SQLite，再返回稳定的 `task_id`。相同 Session、相同 `Idempotency-Key` 和相同正文会返回原任务；同 key 不同正文返回 `409`。最终回答、工具调用、批准请求和状态变化通过 WebSocket 事件流返回。

## HTTP 路由

| 方法与路径 | 说明 |
|---|---|
| `GET /api/v1/meta` | 名称、版本、API 和 capability。 |
| `GET /api/v1/health` | 存活、持久化状态、uptime 和 Session 数。 |
| `GET /api/v1/ready` | 可接流量时返回 200；降级或持久化失败返回 503。 |
| `GET /api/v1/metrics` | Prometheus 文本指标。 |
| `GET /api/v1/events?since=&session_id=&limit=` | 从 SQLite 回放持久事件；单次最多 10000 条。 |
| `GET /api/v1/turns/{task-id}` | 查询持久 turn 状态；也兼容 Session ID 查询最近一项。 |
| `DELETE /api/v1/turns/{task-id}` | 取消排队/运行/待审批 turn，并中断对应 Session。 |
| `GET /api/v1/sessions` | 列出会话。 |
| `POST /api/v1/sessions` | 创建会话；body 为 `{workspace, title?}`，`workspace` 必填。 |
| `GET /api/v1/sessions/{id}` | 读取会话。 |
| `DELETE /api/v1/sessions/{id}` | 删除会话。 |
| `POST /api/v1/sessions/{id}/messages` | 提交 `{text}`。 |
| `GET /api/v1/sessions/{id}/export` | 导出 JSON 会话。 |
| `POST /api/v1/approvals/{id}` | 提交 `{decision, feedback?}`。 |
| `GET /api/v1/questions` | 列出待回答问题。 |
| `POST /api/v1/questions/{id}` | 提交问题响应 JSON。 |
| `GET /api/v1/tools`、`/tasks`、`/skills` | 当前能力和后台任务。 |
| `GET /api/v1/modelCatalog` | 模型目录；`/models` 是别名。 |
| `GET /api/v1/config` | 安全裁剪后的运行配置。 |
| `GET /api/v1/workspaces` | 工作区信息；`/workspaceFs` 是别名。 |
| `GET /api/v1/fs?path=...` | 读取可信工作区内文本文件。 |
| `POST /api/v1/fs` | 写入 `{path, content?}`。 |
| `GET /api/v1/files?path=...` | 列目录。 |
| `GET /api/v1/search?q=...` | 搜索工作区。 |
| `GET /api/v1/snapshot` | 会话与工作区快照。 |
| `GET /api/v1/prompts` | 可用提示模板。 |
| `GET/POST /api/v1/terminals` | 列出或创建终端命令。 |
| `GET/DELETE /api/v1/terminals/{id}` | 轮询输出或停止并删除终端。 |
| `GET /api/v1/connections` | 可用实时连接。 |
| `GET /api/v1/ws` | WebSocket 事件流。 |

终端创建 body 为 `{command?, cwd?}`。API 必须用 `--allow-terminal-api` 开启并持有 `terminal` 或 `admin` scope。最多同时保留 64 个 HTTP terminal，命令最大 64 KiB，stdout 和 stderr 各最多保留约 1 MiB。

## WebSocket

连接示例：

```text
ws://127.0.0.1:8787/api/v1/ws?token=<url-encoded-token>&session_id=<id>&since=<seq>
```

连接后先收到 hello，其中包含 `latest_event_seq` 和实时广播窗口 `history_capacity`。每个事件包含由 SQLite 分配、跨进程重启保持单调的 `event_seq` 和 `emitted_at`。`since` 从持久事件表回放，`session_id` 在服务端过滤。发送 `{"type":"subscribe","session_id":"..."}` 可设置过滤；发送包含 `ping` 的文本会收到 pong。

WebSocket 是广播流；慢客户端会收到 `resync_required`，随后应从最后确认的序号调用 events、turn 和 snapshot 恢复。2048 是实时 channel 的内存窗口，不是持久历史上限。

## 本地 RPC

默认 TUI 使用进程内 memory transport。独立 Server 在 Unix 使用 UDS；Windows endpoint 文件指向本机 loopback 端口。内部 RPC 覆盖会话创建、恢复、prompt、中断、模式/模型切换、compact、undo、历史轮次枚举与精确分叉、usage、skills、plugins、tasks、approval、question 和 swarm 等操作。

这是内部版本化接口；外部 Node 集成优先使用 HTTP/WS SDK，编辑器集成优先使用 ACP。

## 错误处理

- `401`：token 缺失或错误。
- `404`：会话、终端或批准 ID 不存在。
- `400`：请求字段、路径或后端操作无效。
- `409`：幂等键冲突、任务不可取消或 Session 正忙。
- `413`：终端命令超过限制。
- `429`：终端数量达到上限。

客户端应设置请求超时、记录响应 body，并把提交消息和等待最终事件视为两个阶段。
