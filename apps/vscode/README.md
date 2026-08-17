# kkagent VS Code 扩展

IDE bridge，基于 Agent Client Protocol (ACP) v1 与 `kkagent acp` 子进程通信，并保留 HTTP API 作为降级通道。

## 当前能力

- `kkagent: Start ACP bridge`：启动 `kkagent acp` 子进程，完成 `initialize` + `session/new`，工作目录为当前 workspace。
- `kkagent: Send prompt`：通过 `session/prompt` 发送官方 content block（`[{type:"text",text}]`）。
- 流式渲染：`session/update` 通知中的 `agent_message_chunk`（打字机输出）、`agent_thought_chunk`、`tool_call` / `tool_call_update` 实时写入 “kkagent (ACP)” 输出通道。
- 权限处理：agent 发起 `session/request_permission` 时弹出模态选择（allow_once / allow_always / reject_once / reject_always），响应 `{outcomeKind:"selected", optionId}` 或取消。
- 输入处理：`session/request_input` 映射为 QuickPick（select/多选）或输入框（text/password）。
- `/commit`、`/explain`、`/fix` 快捷 slash prompt（explain 附带当前选中文本）。
- 设置 `kkagent.binary`：`kkagent` 可执行文件路径；设置 `kkagent.httpUrl`：HTTP 降级模式 base URL。
- Sessions 树视图（HTTP）与 diff 预览保留。

## 使用

1. 确保 `kkagent` 已安装且默认配置可用；
2. 命令面板执行 `kkagent: Start ACP bridge`；
3. 执行 `kkagent: Send prompt`，在输出面板 “kkagent (ACP)” 查看流式回复与工具调用；
4. 权限/输入请求会以 VS Code 原生弹窗呈现，选择后 turn ���续。

协议细节见 [docs/extensions.md](../../docs/extensions.md)。
