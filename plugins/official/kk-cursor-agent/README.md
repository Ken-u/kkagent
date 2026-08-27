# KK Cursor Agent

官方示例插件：把 Cursor CLI（`agent acp`，ACP 协议）注册为一个外部子 Agent 类型，
通过 kkagent 的 `Agent` 工具委派任务。

## 工作方式

1. kkagent 启动时发现本插件的 `subagents` 声明，注册限定名 `kk-cursor-agent.cursor`；
2. 模型调用 `Agent` 工具并传 `subagent_type: "kk-cursor-agent.cursor"`；
3. kkagent 以 ACP 客户端身份 spawn `agent acp`（stdio + JSON-RPC），
   走 `initialize` → `session/new` → `session/prompt` 完成一轮委派；
4. 外部 agent 的权限请求默认自动批准（`autoApprove`），结果写回
   SubagentManager，`TaskOutput` / 状态轮询与内建子 agent 完全一致。

## 前置条件

- Cursor CLI 已安装且 `agent` 在 PATH 上；
- 已完成认证：环境变量 `CURSOR_API_KEY` / `CURSOR_AUTH_TOKEN`，或先运行 `agent login`。
  本 manifest 用 `skipAuth: true` 跳过 ACP `authenticate` 步骤，凭据由环境注入。

## 自定义

把 `transportConfig.command` 换成任意其它 ACP agent 的启动命令即可接入
非 Cursor 的外部 agent；`mode` 支持 `agent` / `plan` / `ask`。
