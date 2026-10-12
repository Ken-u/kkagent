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

## 作为会话后端（`/model`）

插件启用后，`/model` 里会自动多出 `acp:kk-cursor-agent.cursor`（`/model cursor` 也可直接
切换，无需任何配置）。选中后该会话的每一轮都交给 Cursor CLI 执行，kkagent 的 TUI 负责
渲染文本、思考、工具卡片和 Todo，并转发 Cursor 的权限请求、`ask_question` 和
`create_plan`（计划以「执行 / 修改意见 / 拒绝」评审卡呈现）。

- 这种用法**忽略** manifest 里的 `autoApprove` 和 `promptPrefix`（它们只用于 `Agent`
  工具委派）：权限按会话的权限模式处理。
- Cursor 是否发出权限请求由它自己的审批设置决定。`~/.cursor/cli-config.json` 里
  `approvalMode` 为 `unrestricted`（Run Everything）时 Cursor 不会请求权限，kkagent
  拦不住；想在 kkagent 里逐项确认，请改为 `allowlist`。
- Cursor 自己管理工具、沙箱、MCP 和上下文；运行中的插话会排队到本轮结束后发送，
  `Esc` 中断本轮。
- 模型由 Cursor 决定（`~/.cursor/cli-config.json`，或在 Cursor CLI 里 `/model`），
  kkagent 不会换它的模型；footer 显示它汇报的模型与思考等级（如
  `claude-sonnet-5-5  thinking: high`）。思考等级可以在 kkagent 里用 `/effort`（或
  `/model cursor#max`）调整，Cursor 提供什么等级就能选什么（如 Sonnet 5.5 的
  `low` / `medium` / `high` / `xhigh` / `max`）。注意 Cursor 把它保存为该模型的全局
  默认值，之后直接运行 Cursor CLI 也会沿用。
- 会话随时可以 `/model` 切回普通模型接管，`/export-md` 照常可用；`/undo` 只回退对话，
  Cursor 改动的文件需自行用 git 处理。

## 前置条件

- Cursor CLI 已安装且 `agent` 在 PATH 上；
- 已完成认证：环境变量 `CURSOR_API_KEY` / `CURSOR_AUTH_TOKEN`，或先运行 `agent login`。
  本 manifest 用 `skipAuth: true` 跳过 ACP `authenticate` 步骤，凭据由环境注入。

## 自定义

把 `transportConfig.command` 换成任意其它 ACP agent 的启动命令即可接入
非 Cursor 的外部 agent；`mode` 支持 `agent` / `plan` / `ask`。
