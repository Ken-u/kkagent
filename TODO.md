# kkagent 与 ref/kimi-code 功能差距清单

> 本文件记录当前 Rust 版 kkagent 与参考 TS 实现（`ref/kimi-code/packages/agent-core-v2`）相比，尚未实现或仅部分实现的功能。用于指引后续迭代。
>
> **核对说明（2026-08-08）**：已对照当前代码修订过时项。标注 `[已核对]` 的状态以仓库现状为准。

## 一、工具集（Tools）

| 工具 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| Read | 已实现 | 已实现 | **不足**：无 UTF-16 转码、无二进制/图片拒绝、无 `MAX_LINE_LENGTH` 截断、无结构化输出（lineCount） |
| Write | 已实现 | 已实现 | **不足**：无结构化输出 `bytesWritten`，缺少 path access policy 前置校验 |
| Edit | 已实现 | 已实现 | **不足**：未处理 CRLF 回合（写入时未按原文件换行风格还原），无 `replace_all` 外的模糊匹配 |
| Grep | 已实现 | 已实现 | **不足**：缺少 `output_mode`（content/files_with_matches/count）、`-A/-B/-C` 上下文、`head_limit/offset`、type filter、`include_ignored`、敏感文件过滤 |
| Glob | 已实现 | 已实现 | **不足**：未尊重 `.gitignore/.ignore/.rgignore`（仅硬编码跳过 `.`/`node_modules`/`target`）、无 `include_ignored`、最大匹配 500 且无提示、未过滤敏感文件 |
| Bash | 已实现 | 已实现 | **不足**：无后台运行、无 `cwd` 参数、无描述字段、超时后未转后台/未杀进程、无 Bash parser 服务；配置里有 `bash_auto_background_on_timeout` 占位未用 |
| TodoList | 已实现 | 已实现 | **不足** `[已核对]`：schema 用 `items`/`id`/`content`/`completed`，ref 用 `todos`/`title`/`done`；无持久化；无 reminder 注入；无 `SetTodoList` 别名。另有未接入的死文件 `todo_list.rs` |
| ExitPlanMode | 已实现 | 已实现 | 基本对齐；ref 有 enter-plan-mode 对应工具，当前由 TUI/配置触发 |
| CreateGoal / GetGoal / UpdateGoal | 已实现 | 已实现 | **不足**：无 `SetGoalBudget`、无 `cancelGoal`/`markBlocked`、goal 未持久化、未注入系统提示 |
| Task / TaskOutput / TaskList | **部分实现** `[已核对]` | 已实现 | 已能 spawn 并真正跑 `run_subagent`，可用 TaskOutput/TaskList 取结果；**仍缺** TaskStop、输出持久化、镜像事件、profile 选择 |
| Agent | 未实现 | 已实现 | ref 的 `Agent` 工具是完整 SubagentTool，支持 profile 选择、后台运行、恢复、镜像运行 |
| AgentSwarm | 未实现 | 已实现 | ref 支持多子代理并发编排 |
| AskUserQuestion | 事件已有，工具未实现 `[已核对]` | 已实现 | `QuestionAsked` / permission 已预留；无工具、TUI 未渲染选项 |
| Skill | 发现代码有，未接入 `[已核对]` | 已实现 | `kkagent-mcp::SkillsManager` 能扫 skill / AGENTS.md，但无 Skill 工具、未注入系统提示 |
| WebSearch | 未实现 | 已实现 | 配置 `services.moonshot_search` 占位 |
| FetchURL | 未实现 | 已实现 | 配置 `services.moonshot_fetch` 占位 |
| ReadMediaFile | 未实现 | 已实现 | 当前无法读取图片/视频 |
| CronCreate / CronList / CronDelete | 未实现 | 已实现 | 当前无定时任务能力 |
| SelectTools | 未实现 | 已实现 | 当前无渐进式工具披露 |
| MCP 工具 | **已接入** `[已核对]` | 已实现 | stdio 客户端 + `mcp__*` 注册进 AgentLoop；仍缺 SSE/HTTP、OAuth、重连 |

## 二、Agent 核心循环

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| 单步 turn | 已实现 | 已实现 | 当前每用户消息运行一次 `run_turn_inner` |
| 多步骤/多工具并行 | 部分实现 | 已实现 | 当前串行执行 tool_calls；ref 有 `ToolScheduler`，可按资源冲突并发执行 |
| 最大步数限制 | **部分实现** `[已核对]` | 已实现 | `AgentLoop` 有硬编码 `max_rounds`（主 64 / 子 24）；**未读** `loop_control` 配置；ref 还有 per-step retry |
| 步骤重试 | 未实现 | 已实现 | ref 有 `stepRetry`、错误处理与重新入队 |
| 上下文压缩/Compaction | 已实现（仅 DB 截断） | 已实现 | 当前 `/compact` 只是删除旧消息；ref 有 LLM 总结压缩、保留用户消息、压缩预算 |
| Token 计数服务 | 未实现 | 已实现 | 当前仅简单相加 usage；ref 有 measured+estimated 策略 |
| 上下文记忆服务 | 未实现 | 已实现 | ref 有 undo/appendLoopEvent/applyCompaction 的完整服务层（文件 undo 已有） |
| ToolResult 截断 | 未实现 | 已实现 | ref 超大工具结果会外置存储，内联替换为 preview |
| 工具去重 | 未实现 | 已实现 | ref 有 `toolDedupe` |
| 工具选择策略 | 未实现 | 已实现 | ref 有 `toolSelect`、 progressive disclosure |
| 系统提醒注入 | 部分实现 | 已实现 | 当前只有 plan-mode reminder；ref 有 dateChange、agentsMd、interruption、todo reminder 等 |
| 会话种子/Seed | 未实现 | 已实现 | ref 有 sessionSeed |

## 三、权限与审批

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| manual / auto / yolo | 已实现 | 已实现 | 基本对齐 |
| 用户配置规则 | 已实现 | 已实现 | 支持 allow/deny/ask |
| session 级批准记忆 | 已实现 | 已实现 | 当前按 `tool_name:input_pattern` 记录 |
| 敏感文件访问询问 | 简化实现 | 已实现 | 当前只有静态敏感词列表；ref 统一 `isSensitiveFile` |
| git 控制路径询问 | 简化实现 | 已实现 | 当前只检查 `.git` 字符串；ref 会探测 worktree 并精确判断 |
| 计划模式守卫 | 已实现 | 已实现 | 当前只允许写入 plan 文件 |
| 审批范围（单次/会话） | 已实现 | 已实现 | TUI 支持 1/2/3（批准/会话/拒绝） |
| 审批超时/取消 | 部分实现 | 已实现 | 当前通过 interrupt 取消；ref 有更完整的审批生命周期 |

## 四、子代理 / 任务（Subagent & Task）

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| 子代理生命周期 | **部分实现** `[已核对]` | 已实现 | 有 `SubagentManager`（spawn/complete/fail/cancel/list）；无 fork/remove 完整服务 |
| 子代理运行 | **已实现** `[已核对]` | 已实现 | `run_subagent` + Task 启动后真正跑 AgentLoop |
| Agent Profile | 未实现 | 已实现 | ref 有 agent/coder/explore 等 profile |
| 任务工具链 | **部分实现** `[已核对]` | 已实现 | 有 Task / TaskOutput / TaskList；**缺 TaskStop** |
| 后台任务 | 部分实现 | 已实现 | Task 已 fire-and-forget；Bash 无后台；无 detach/timeout 转后台 |
| 任务输出持久化 | 未实现 | 已实现 | 结果只在内存 `SubagentManager` |
| 子代理镜像运行 | 未实现 | 已实现 | ref `mirrorAgentRun` 会把子代理事件同步到父代理 |

## 五、Skill 系统

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| Skill 发现 | 代码有、未接线 `[已核对]` | 已实现 | `SkillsManager` 扫 project/user/AGENTS.md |
| Skill 目录扫描 | 同上 | 已实现 | 有基础扫描 |
| Skill 解析/注入 | 未实现 | 已实现 | 未注入系统提示 |
| Skill 工具调用 | 未实现 | 已实现 | 无 `Skill` 工具 |
| 内置 product skills | 未实现 | 已实现 | ref 有 `builtin_product_skills` 开关 |

## 六、MCP

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| MCP stdio 客户端 | 已实现 | 已实现 | `kkagent-mcp` 使用 rmcp |
| MCP SSE/HTTP | 未实现 | 已实现 | ref 有 client-sse/client-http |
| MCP 工具接入 Agent | **已实现** `[已核对]` | 已实现 | 启动时 connect，注册 `mcp__server__tool` 到 ToolRegistry |
| MCP 权限/OAuth | 未实现 | 已实现 | ref 有 mcpCore/oauth |
| MCP 连接管理 | 简单实现 | 已实现 | 有 connect/list/call；无重连、timeout 配置 |

## 七、配置与模型

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| providers/models | 已实现 | 已实现 | 支持多 provider、多 model 别名 |
| 模型目录刷新 | 未实现 | 已实现 | ref 有 modelCatalog 刷新 |
| 辅助模型 | 未实现 | 已实现 | ref 有 secondary_model 用于子代理/总结 |
| 环境变量覆盖 | 部分实现 | 已实现 | ref 大量 `KIMI_*` 环境变量覆盖 |
| OAuth/认证服务 | 未实现 | 已实现 | ref 有完整 oauth 包 |
| Web 搜索/抓取服务 | 配置占位 | 已实现 | 配置有 services，无实现 |
| thinking | 已实现 | 已实现 | 支持 on/off/effort，但只传给 Anthropic |
| token_counting 策略 | 未实现 | 已实现 | 见上文 |
| experimental flags | 未实现 | 已实现 | ref 有 flag 系统 |

## 八、LLM Provider 协议

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| Anthropic Messages | 已实现 | 已实现 | 基础流式解析 |
| OpenAI / OpenAI Responses | 未实现 | 已实现 | ref 有 openai-legacy/openai-responses |
| Google GenAI | 未实现 | 已实现 | ref 有 google-genai base |
| Kimi 协议 | 未实现 | 已实现 | ref 有 kimi contrib/schema/errors/files |
| 错误分类与重试 | 未实现 | 已实现 | ref 按错误码决定重试 |
| 工具调用流解析 | 部分实现 | 已实现 | 当前只解析 Anthropic SSE；ref 统一抽象 |

## 九、持久化与 Wire

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| 会话列表/消息持久化 | 已实现 | 已实现 | SQLite transcript DB |
| 会话标题 | 已实现 | 已实现 | 自动从首条用户消息提取 |
| Wire 格式/迁移 | 未实现 | 已实现 | ref 有 wire 持久化格式和多版本迁移 |
| 会话导出 | 已实现 | 已实现 | `/export-md` |
| 会话索引 | 未实现 | 已实现 | ref 有 sessionIndex |
| 会话元数据 | 未实现 | 已实现 | ref 有完整 sessionMetadata |
| Snapshot/Resume | 部分实现 | 已实现 | 当前只恢复消息；ref 恢复完整 agent 状态 |

## 十、TUI / CLI

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| TUI 会话界面 | 已实现 | 已实现 | 消息、工具折叠、滚动、输入历史 |
| 审批面板 | 已实现 | 已实现 | 鼠标+键盘 |
| TODO 面板 | 已实现 | 已实现 | sticky 折叠/展开 |
| 模型选择器 | 已实现 | 已实现 | `/model` |
| 会话选择器 | 已实现 | 已实现 | `/sessions` |
| 任务面板 | UI 占位 | 已实现 | `/tasks` 只有列表，无输出详情/停止 |
| 问题回答 UI | 未实现 `[已核对]` | 已实现 | TUI 未处理 `QuestionAsked` |
| 计划模式 UI | 已实现 | 已实现 | Shift-Tab 切换 |
| Shell 模式 | 已实现 | 已实现 | `!` 触发 |
| 撤销 | 已实现 | 已实现 | Esc Esc |
| 剪贴板复制 | 已实现 | 已实现 | `/copy` |
| 命令补全 | 已实现 | 已实现 | `/` 菜单 |
| 非交互式 prompt | 已实现 | 已实现 | `-p` |
| 独立 server | 已实现 | 已实现 | `server` subcommand，UDS |
| server 多客户端 | 已实现 | 已实现 | 每个连接独立 handler |
| 日志 | 已实现 | 已实现 | TUI 写文件，其它写 stderr |

## 十一、Git / 工作区

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| git 上下文收集 | 未实现 | 已实现 | ref `gitContext.ts` 收集分支/状态/最近提交 |
| 工作区信任 | 未实现 | 已实现 | ref 有 workspaceTrust |
| 工作区 instructions | 未实现 `[已核对]` | 已实现 | AGENTS.md 可被 SkillsManager 读到，但未注入会话 |
| 文件监听 | 未实现 | 已实现 | ref 有 fsWatchService |
| 进程运行器 | 部分实现 | 已实现 | 当前 BashTool 直接用 tokio；ref 有统一 process runner |

## 十二、外部 Hook / 遥测

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| 外部 Hook | 配置有 + HookManager 有，未触发 `[已核对]` | 已实现 | agent loop 未调用 HookManager |
| 遥测 | 未实现 | 已实现 | ref 有 telemetryService、cloudAppender |
| Cron 服务 | 未实现 | 已实现 | ref 有 app/cron |

## 十三、测试与基础设施

| 能力 | kkagent 状态 | ref 状态 | 差距说明 |
|------|-------------|---------|---------|
| 单元测试 | 极少 | 大量 | 当前仅 permission / transcript 等少量测试 |
| DI/服务容器 | 未实现 | 已实现 | ref 使用自研 DI（`_base/di`） |
| 统一错误类型 | 未实现 | 已实现 | ref 有 Error2/ErrorCodes |
| 生命周期管理 | 未实现 | 已实现 | ref 有 scope/lifecycle |

## 十四、近期已完成（避免重复）

- [x] TODO 面板与 TodoUpdated 事件
- [x] 按 session 的 plan 文件路径
- [x] README server/TUI 配对说明
- [x] Task 真正驱动子代理（`run_subagent`）+ TaskOutput / TaskList
- [x] AgentLoop `max_rounds` 硬限制（尚未接配置）
- [x] MCP 工具注册为 `mcp__server__tool` 并接入 AgentLoop

## 十五、建议优先级（仅供参考）

1. **P0 - 让工具真正可用**
   - ~~MCP 工具接入 AgentLoop（`mcp__server__tool`）~~
   - TaskStop + 任务面板停止
   - Bash `cwd` / 后台运行 / 超时处理

2. **P1 - 补齐核心体验**
   - AskUserQuestion 工具 + TUI 选项渲染
   - TodoList schema 对齐 ref（`todos`/`title`/`done`）
   - Grep/Glob/Read 参数对齐
   - AGENTS.md / Skill 注入

3. **P2 - 扩展能力**
   - WebSearch / FetchURL / ReadMediaFile
   - Cron 工具
   - OpenAI / Kimi provider

4. **P3 - 工程化**
   - 增加测试覆盖
   - 统一错误类型
   - DI/服务容器（若需要接近 ref 的架构）
