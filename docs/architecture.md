# 架构

kkagent 是 Rust workspace。交互前端不直接运行 Agent loop，而是通过 RPC facade 操作会话。默认 TUI 模式会自动启动并连接独立 standalone server（UDS / Windows loopback endpoint）；也可用 `--connect` 连接已有 server，或在 `[server] standalone = false` 时退回同进程 memory pair。

```text
TUI / prompt / ACP / HTTP+WS
            │
     client + RPC (UDS)
            │
   standalone server process
      session runtime + agent tasks
       ┌────┴────┐
   LLM provider  tool scheduler
                    │
       builtin / MCP / tasks / hooks
            │
 transcript DB + session journal + files
```

`Ctrl+B` 或确认退出时写入 `~/.kkagent/active-session`；下次 `kk` 检测到存活 server 后自动 resume。无客户端且无 active turn 超过 `[server].idle_timeout_secs` 时 server 自动退出；也可用 `kkagent server stop` / `kkagent server status` 管理。

Agent turn 事件（含 Thinking / StatusUpdate / TurnEnd）由 server 扇出到当前所有已连接的 RPC writer；TUI detach 后 turn 继续在 server 内跑，重新 attach 后新连接会立刻收到后续事件（断线窗口内的增量不补发，完整 transcript 靠 resume/history）。`AskUserQuestion` / 工具审批 / `/btw` / compact / 排队 prompt / 子 agent 摘要 / 最近 StatusUpdate 与部分 streaming 缓冲会记在 server，由 `session.resume` 带回。

## Crate 职责

| crate | 职责 |
|---|---|
| `kkagent` | CLI、进程装配、Server backend 和认证入口。 |
| `kkagent-config` | TOML schema、默认值、环境变量覆盖和校验。 |
| `kkagent-protocol` | RPC 消息、事件、权限、问题、目标和工具类型。 |
| `kkagent-rpc` | codec、client/server、memory/UDS transport、HTTP/WS。 |
| `kkagent-client` | 面向 TUI/CLI 的客户端 facade。 |
| `kkagent-core` | Agent loop、会话生命周期、上下文、压缩、权限、调度和持久化。 |
| `kkagent-llm` | Anthropic、OpenAI Chat/Responses、Kimi、Google 协议适配和流解析。 |
| `kkagent-tools` | 内置工具、参数验证、路径策略、Shell 安全和后台任务。 |
| `kkagent-mcp` | MCP client、OAuth、Skills 和 Hooks。 |
| `kkagent-tui` | ratatui UI、输入编辑、slash command 和事件渲染。 |
| `kkagent-acp` | stdio ACP JSON-RPC 桥。 |
| `kkagent-oauth` | Kimi device-code、刷新和凭据存储。 |
| `kkagent-wire` | 会话 journal、记录格式与迁移。 |
| `kkagent-telemetry` | 本地/云事件 appender 和隐私清洗。 |
| `kkagent-di` | 运行时依赖注册。 |
| `kkagent-kaos` | 故障注入和韧性测试辅助。 |

## 一轮请求

1. 前端创建或恢复 Session，并提交 prompt。
2. Session 合并系统指令、`AGENTS.md`、Skill、插件提示和历史上下文。
3. LLM adapter 按 Provider 方言发起流式请求。
4. 文本和工具调用增量转换为统一事件，同时写入会话记录。
5. Tool policy、Plan guard、权限规则和 Hook 依次参与工具决策。
6. Scheduler 执行允许的内置/MCP 工具，把结果送回下一次模型调用。
7. 没有工具调用或达到终止条件时产生最终回答；超预算时可自动 compact。

## 会话与并发

每个 Session 有独立上下文和执行状态。Server 可承载多个 Session；同一 Session 的 prompt、interrupt、approval 和 question 通过 ID 关联。后台 Task 有单独并发控制，Bash 后台进程还有独立硬上限。

HTTP WebSocket 是进程级实时广播；事件先写入 SQLite、再发送给订阅者。Server 使用数据库自增序号，支持服务重启后的 `session_id` 过滤和 `since` 回放；内存中另外保留最近 2048 条作为低延迟窗口。

## 持久化

- SQLite transcript 保存结构化消息和会话索引数据；`tool_results` 表记录超大工具结果文件与 session/tool call 的映射。
- 同一 SQLite 数据库保存 HTTP 事件、幂等 turn 队列、租约/重试状态和后台 Agent 配置。启动时 `running`/待审批任务会进入恢复队列，最多尝试三次。
- session 目录保存事件 journal、元数据和运行产物。
- 超大工具结果外置到 `~/.kkagent/tool-results/<session_id>/`，不占用 workspace；subagent 结果写入父 session 桶。
- 删除 session 时先完整导出到 `~/.kkagent/trash/<session_id>.jsonl` 回收站（session 行、全部消息、工具结果全文），再清空 DB 行与文件；回收站仅供离线分析，运行时不读取。
- wire 记录包含迁移支持，升级时不应手工编辑。

详细路径见[运维指南](operations.md)。
