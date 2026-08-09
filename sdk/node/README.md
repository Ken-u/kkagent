# @kkagent/sdk

`@kkagent/sdk` 是 kkagent 的 Node.js 客户端。它不在 Node.js 里运行 Coding Agent，
而是通过 HTTP、WebSocket 或 JSON-RPC 控制已经启动的 Rust `kkagent server`。

最常见的使用场景是：你有一个 Node.js 后台、自动化脚本或编辑器扩展，希望让
kkagent 在指定项目中读取代码、调用模型、执行工具，并把过程和结果返回给你的程序。

```text
Node.js 程序
    │
    │  @kkagent/sdk（HTTP / WebSocket）
    ▼
kkagent server
    │
    ├── LLM
    ├── Read / Grep / Bash 等工具
    └── Session 与 transcript
```

## 快速开始

### 1. 启动 kkagent server

先准备正常可用的 `~/.kkagent/config.toml`，然后启动 HTTP 服务：

```bash
export KKAGENT_HTTP_TOKEN="replace-with-a-long-random-token"

kkagent server \
  --http 127.0.0.1:8787 \
  --http-token "$KKAGENT_HTTP_TOKEN"
```

服务默认只监听本机。不要把未加 TLS 和额外访问控制的端口直接暴露到公网。

### 2. 在 Node.js 中创建 Agent 会话

从仓库直接引用 SDK 时：

```js
import { KkagentClient } from "./sdk/node/src/index.js";

const client = new KkagentClient({
  baseUrl: "http://127.0.0.1:8787",
  token: process.env.KKAGENT_HTTP_TOKEN,
});

const session = await client.createSession(
  "/absolute/path/to/project",
  "检查项目",
);

console.log("session:", session.session_id);

await client.postMessage(
  session.session_id,
  "阅读这个项目，运行测试，并总结最需要修复的三个问题。",
);
```

`workspace` 必须是 server 所在机器上的绝对路径，并且需要符合
`config.toml` 中的 `trusted_workspaces` 限制。

### 3. 接收实时事件

Agent 的回复和工具执行是异步的。`postMessage()` 表示消息已被 server 接受，不代表
Agent 已经完成。需要通过 WebSocket 接收增量文本、工具调用、错误和回合结束事件：

```js
let lastEventSeq = 0;
const socket = client.connectEvents((event) => {
  console.log("agent event:", event);
  if (event?.event_seq) lastEventSeq = event.event_seq;

  if (event?.type === "turn_end") {
    console.log("Agent 本轮执行完成");
  }
}, { sessionId: session.session_id, since: lastEventSeq });

socket.addEventListener("open", () => {
  console.log("event stream connected");
});

socket.addEventListener("error", (error) => {
  console.error("event stream failed", error);
});

// 不再需要事件时：
// socket.close();
```

Node.js 22 提供全局 `WebSocket`。在没有全局 `WebSocket` 的运行时中，需要先安装并注入
兼容实现，例如：

```bash
npm install ws
```

```js
import WebSocket from "ws";
globalThis.WebSocket = WebSocket;
```

## 一个实际例子：代码检查 API

下面的 Express 风格处理函数把普通 HTTP 请求转换为一个 kkagent 任务：

```js
import { KkagentClient } from "@kkagent/sdk";

const kkagent = new KkagentClient({
  baseUrl: process.env.KKAGENT_URL ?? "http://127.0.0.1:8787",
  token: process.env.KKAGENT_HTTP_TOKEN,
});

export async function reviewRepository(req, res) {
  try {
    const session = await kkagent.createSession(
      req.body.workspace,
      `Review ${req.body.changeId}`,
    );

    await kkagent.postMessage(
      session.session_id,
      "检查当前改动，运行相关测试，并给出带文件位置的审查结论。不要修改文件。",
    );

    res.status(202).json({
      sessionId: session.session_id,
      status: "accepted",
    });
  } catch (error) {
    res.status(502).json({
      error: error instanceof Error ? error.message : String(error),
    });
  }
}
```

生产应用通常还需要把 WebSocket 事件按 `session_id` 转发给对应的网页客户端，或写入
自己的任务数据库。

## 当前 API

### `new KkagentClient(options)`

`KkagentClient` 是 `KkagentHttpClient` 的别名。

```ts
interface KkagentClientOptions {
  baseUrl?: string; // 默认 http://127.0.0.1:8787
  token?: string;
  binary?: string;  // 预留字段；当前不会自动启动二进制
}
```

### HTTP 方法

| 方法 | 作用 | 返回值 |
|------|------|--------|
| `meta()` | 查询 server 名称、版本和能力 | 原始 JSON |
| `listSessions()` | 列出当前 Session | `Session[]` |
| `createSession(workspace?, title?)` | 创建 Session | `Session` |
| `postMessage(sessionId, text)` | 提交一条用户消息 | 接受结果 JSON |
| `listTools()` | 查询可用工具 | 原始 JSON |
| `modelCatalog()` | 查询模型目录 | 原始 JSON |
| `eventsSince(since?, sessionId?, limit?)` | 回放带序号的事件窗口 | `EventHistory` |
| `turnStatus(sessionId)` | 查询最近 turn 状态 | 原始 JSON |
| `connectEvents(callback, {since?, sessionId?})` | 连接可恢复、可过滤的 WebSocket 事件流 | `WebSocket` |

### `JsonRpcClient`

`JsonRpcClient` 只负责生成和解析单次 JSON-RPC 2.0 消息。传输由调用方提供：

```js
import { JsonRpcClient } from "@kkagent/sdk";

const rpc = new JsonRpcClient(async (requestLine) => {
  // 这里可以写入 stdio、socket 或其他传输，并返回对应响应行。
  return sendToKkagent(requestLine);
});

const result = await rpc.call("sessions.list", { limit: 20 });
```

它目前不是一个完整的连接管理器，不负责自动生成并发请求 ID、处理异步事件、重连或超时。

## 认证与安全

- SDK 对 HTTP 使用标准 Bearer Header；WebSocket 因浏览器 API 限制使用 URL token。
- token 不应写入源码或提交到 Git，建议通过环境变量传入。
- `workspace` 指向 server 机器上的目录，不是浏览器或调用方机器上的目录。
- 建议设置 `trusted_workspaces`，限制 Agent 可以创建 Session 的根目录。
- Server 已有 token scope、基础限流和审计；公网部署仍应增加 TLS 和网络访问控制。

## 构建与测试

```bash
cd sdk/node
npm install
npm run build
npm test
```

仓库内的零构建 JavaScript 入口是 `src/index.js`；TypeScript 构建结果写入 `dist/`。

## 当前边界

当前 SDK 是可用的最小客户端，还没有覆盖 server 的全部 API：

- 尚未封装审批、问题回答、文件、终端、任务、导出和 Session 删除等接口。
- 尚未提供自动启动/停止本地 `kkagent` 子进程的能力。
- WebSocket 支持 Session 过滤和 `since` 回放，但尚未自动重连、心跳和提供完成态 Promise；调用方可配合 `eventsSince()`、`turnStatus()` 恢复。
- HTTP 错误目前主要包含状态码，尚未统一解析结构化错误响应。
- JavaScript 与 TypeScript 入口目前分别维护。

这些边界意味着它适合内部集成、原型和受控服务调用；若用于长期运行的生产服务，调用方
需要自行补充任务状态持久化、重试、超时和事件路由。
