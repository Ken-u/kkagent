# kkagent VS Code 扩展

这是实验性的最小 IDE bridge，用来验证 VS Code 与 `kkagent acp` / HTTP API 的连通性，不是完整聊天 UI。

## 当前能力

- `kkagent: Start ACP bridge`：启动 `kkagent acp` 子进程并发送 `initialize`。
- `kkagent: Send prompt`：输入 prompt，优先尝试 HTTP 创建 Session 和提交消息；失败时若 ACP 已启动则写入 ACP stdin。
- 设置 `kkagent.binary`：`kkagent` 可执行文件路径。
- 设置 `kkagent.httpUrl`：Agent Server HTTP base URL，默认 `http://127.0.0.1:8787`。

## 使用 ACP

确保 kkagent 已安装且默认配置可用，然后在命令面板依次执行：

1. `kkagent: Start ACP bridge`
2. `kkagent: Send prompt`

当前扩展只提交 prompt 和显示发送通知，尚未解析 ACP stdout、创建完整 Session 生命周期或渲染流式回答。

## HTTP 模式限制

Agent Server 的 HTTP API 强制 Bearer token，而当前扩展尚未提供 token 设置。因此直接 HTTP 提交会失败并回退到已启动的 ACP。需要完整 HTTP/WS 集成时使用 [Node.js SDK](../../sdk/node/README.md)，或先为扩展实现 token 与事件订阅。

## 开发

```bash
cd apps/vscode
npm install
```

在 VS Code 中打开该目录并启动 Extension Development Host。该目录目前没有发布、打包或自动测试流程。

协议说明见 [ACP 与扩展机制](../../docs/extensions.md)，Server 路由见 [Agent Server API](../../docs/server-api.md)。
