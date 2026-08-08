# kkagent 与 ref/kimi-code 功能差距清单

> 核对更新（2026-08-09）：MCP SSE/HTTP+OAuth、并行 ToolScheduler、子代理镜像、DI/Wire/云遥测已落地。仍保留 **Kimi provider** 未做。

## 一、工具集（Tools）

| 工具 | 状态 | 说明 |
|------|------|------|
| Read / Write / Edit | **已增强** | UTF-16 解码、二进制拒绝、行截断/lineCount、bytesWritten、CRLF 保留、模糊提示 |
| Grep / Glob | **已增强** | output_mode/上下文/分页；gitignore |
| Bash | **已增强** | cwd/后台/超时转后台 |
| TodoList | **已对齐** | todos/title/done + reminder |
| EnterPlanMode / ExitPlanMode | **已实现** | 工具 + AgentLoop 切换 |
| Goal / Task / TaskStop | **已实现** | Task 链路完整；输出落盘 `.kkagent/tasks/` |
| Agent / AgentSwarm | **已实现** | profile（explore/coder/general）+ 并行 swarm |
| AskUserQuestion | **已实现** | 工具 + TUI |
| Skill | **已实现** | SkillCatalog 扫描 + Skill 工具 + 系统提示目录 |
| WebSearch / FetchURL | **已实现** | moonshot 服务配置或直接 HTTP |
| ReadMediaFile | **已实现** | base64 + metadata |
| CronCreate/List/Delete | **已实现** | 内存调度 + 轮询 |
| SelectTools | **已实现** | 渐进式工具披露 |
| MCP | **已完整** | stdio / SSE / streamable-HTTP + OAuth（DCR/PKCE/凭证落盘） |

## 二、Agent 核心循环

| 能力 | 状态 |
|------|------|
| max_steps / loop_control | **已接配置** `max_steps_per_turn` |
| ToolResult 截断外置 | **已实现** → `.kkagent/tool-results/` |
| 系统提醒 | plan + date + todo reminder + skills 目录 |
| Compaction | **LLM 总结**（secondary_model 优先）后再截断 DB |
| Hooks | **已触发** TurnStart / PreToolCall（config + hooks.json） |
| 并行 ToolScheduler | **已实现**（ToolAccesses 冲突矩阵，非冲突并发） |

## 三–四、权限 / 子代理

| 能力 | 状态 |
|------|------|
| 敏感路径 | 共用 path_policy |
| Profile 子代理 | explore/coder/general |
| 任务输出持久化 | `.kkagent/tasks/<id>.md` |
| 镜像事件 | **已实现** spawned/started/completed/failed + child tool/message → 父 TUI |

## 五–七、Skill / MCP / 配置

| 能力 | 状态 |
|------|------|
| Skill 系统与调用 | **已实现** |
| secondary_model | **已实现** |
| 环境变量覆盖 | KKAGENT_* / OPENAI_API_KEY / ANTHROPIC_API_KEY / GOOGLE_API_KEY |
| trusted_workspaces | **已实现** |
| MCP `[type]` | `stdio` / `sse` / `http`/`streamable-http` + `url`/`headers`/`oauth` |

## 八、LLM Provider

| 能力 | 状态 |
|------|------|
| Anthropic Messages | 已实现 |
| OpenAI Chat Completions | **已实现**（含重试） |
| Google GenAI SSE | **已实现**（含重试） |
| **Kimi 协议** | **未做（按用户要求保留）** |
| 错误重试 | **已实现**（429/5xx/timeout） |

## 九–十三、其它

| 能力 | 状态 |
|------|------|
| Git 上下文注入 | **已实现** |
| TUI `/tasks` | 详情 Enter + 停止 `x`/`s` |
| DI 容器 | **已实现** `kkagent-di` |
| Wire 多版本迁移 | **已实现** 1.0→1.5 + journal JSONL |
| 云遥测 | **已实现** console/file + CloudAppender（`KKAGENT_TELEMETRY_CLOUD=1`） |

## 十四、近期已完成

- [x] MCP SSE/HTTP + OAuth（DCR/PKCE/本地 callback/凭证 `~/.kkagent/credentials/mcp/`）
- [x] 真正并行 ToolScheduler（资源冲突调度）
- [x] 子代理事件镜像到父 TUI
- [x] DI / Wire 1.0–1.5 / 云遥测
- [x] MCP / TaskStop / Bash 后台 / AskUserQuestion / TodoList / Skills / Web / Cron / Agent*
- [x] OpenAI + Google providers（不含 Kimi）

## 十五、仍可选

1. Kimi 专用 provider / schema / files API
