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

## 认证

所有 HTTP 路由使用同一认证：

```http
Authorization: Bearer <token>
```

也兼容 `?token=<token>`，主要用于不方便设置 Header 的 WebSocket 客户端。查询参数可能进入日志和代理记录，普通 HTTP 请求应优先使用 Header。

## 最小流程

```bash
API=http://127.0.0.1:8787/api/v1
AUTH="Authorization: Bearer $KKAGENT_HTTP_TOKEN"

curl -sS -H "$AUTH" "$API/meta"

curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"workspace":"/absolute/path/to/project","title":"review"}' \
  "$API/sessions"

curl -sS -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"text":"运行测试并总结失败"}' \
  "$API/sessions/<session-id>/messages"
```

发送消息只表示任务已提交。最终回答、工具调用、批准请求和状态变化通过 WebSocket 事件流返回。

## HTTP 路由

| 方法与路径 | 说明 |
|---|---|
| `GET /api/v1/meta` | 名称、版本、API 和 capability。 |
| `GET /api/v1/sessions` | 列出会话。 |
| `POST /api/v1/sessions` | 创建会话；body 为 `{workspace?, title?}`。 |
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

终端创建 body 为 `{command?, cwd?}`。最多同时保留 64 个 HTTP terminal，命令最大 64 KiB，stdout 和 stderr 各最多保留约 1 MiB。该接口能执行任意已认证命令，是部署时最高风险的接口之一。

## WebSocket

连接示例：

```text
ws://127.0.0.1:8787/api/v1/ws?token=<url-encoded-token>
```

连接后先收到 `{"type":"hello","api":"v1"}`。发送 `{"type":"subscribe"}` 会收到订阅确认；发送包含 `ping` 的文本会收到 pong。随后 Server 推送全局 Agent 事件。事件通常含 `type`，会话相关事件通常含 `session_id`；客户端应自行按会话过滤并忽略未知字段和未知事件类型。

WebSocket 是广播流，慢客户端可能丢失事件。持久状态应通过会话、任务和 snapshot HTTP API 重新读取，不应只依赖事件重放。

## 本地 RPC

默认 TUI 使用进程内 memory transport。独立 Server 在 Unix 使用 UDS；Windows endpoint 文件指向本机 loopback 端口。内部 RPC 覆盖会话创建、恢复、prompt、中断、模式/模型切换、compact、undo、usage、skills、plugins、tasks、approval、question 和 swarm 等操作。

这是内部版本化接口；外部 Node 集成优先使用 HTTP/WS SDK，编辑器集成优先使用 ACP。

## 错误处理

- `401`：token 缺失或错误。
- `404`：会话、终端或批准 ID 不存在。
- `400`：请求字段、路径或后端操作无效。
- `413`：终端命令超过限制。
- `429`：终端数量达到上限。

客户端应设置请求超时、记录响应 body，并把提交消息和等待最终事件视为两个阶段。
