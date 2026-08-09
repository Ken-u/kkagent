# 架构

kkagent 是 Rust workspace。交互前端不直接运行 Agent loop，而是通过 RPC facade 操作会话；默认模式把两端放在同一进程，独立 Server 模式则复用相同协议边界。

```text
TUI / prompt / ACP / HTTP+WS
            │
        client + RPC
            │
      session runtime
       ┌────┴────┐
   LLM provider  tool scheduler
                    │
       builtin / MCP / tasks / hooks
            │
 transcript DB + session journal + files
```

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

HTTP WebSocket 是进程级广播，不是每 Session 一条可靠队列；客户端负责按 `session_id` 过滤，并通过 REST 快照恢复状态。

## 持久化

- SQLite transcript 保存结构化消息和会话索引数据。
- session 目录保存事件 journal、元数据和运行产物。
- workspace 下 `.kkagent/` 保存计划、超大工具结果等项目相关产物。
- wire 记录包含迁移支持，升级时不应手工编辑。

详细路径见[运维指南](operations.md)。
