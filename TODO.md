# kkagent 与 ref/kimi-code 功能差距清单

> 核对更新（2026-08-09 三次）：P0–P2 主体已落地。**Kimi provider 按要求保留未做。**

## 一、工具集（Tools）

| 工具 | 状态 |
|------|------|
| Read / Write / Edit / Grep / Glob / Bash | **已实现**（Bash 含危险命令启发式拦截） |
| TodoList / Plan / Goal* / SetGoalBudget | **已实现** |
| Task* / Agent* / AskUserQuestion | **已实现**（Task 可选 git worktree） |
| Skill（含 builtin consolidate/review/write-goal） | **已实现** |
| WebSearch / FetchURL（moonshot + local fallback） | **已实现** |
| ReadMediaFile（limits/originals/video meta） | **已实现** |
| Cron*（persist + jitter + cron-fire XML） | **已实现** |
| SelectTools / MCP | **已实现** |
| args-validator / display schemas / shell_safety | **已实现** |

## 二、AgentCore

| 能力 | 状态 |
|------|------|
| tokenCounting / contextProjector / auto-compact | **已实现** |
| fullCompaction（keep/vacuous/handoff） | **已实现** |
| Hooks（SessionStart/Turn*/Pre/Post + block/rewrite + Notification） | **已实现** |
| toolDedupe / stepRetry / ModelCapability | **已实现** |
| Goal 预算门禁 + injection | **已实现** |
| blob store + media @path resolve | **已实现** |
| replayBuilder / activityView / eventBus | **已实现** |
| systemReminder / plugin manager | **已实现** |
| 并行 ToolScheduler / 子代理镜像 | **已实现** |

## 三、TUI

| 能力 | 状态 |
|------|------|
| 斜杠命令（含 config/auth/skills/swarm/provider/web/info/btw/…） | **已实现** |
| tool-renderers（chip/summary/bash/grep/diff/media/goal） | **已实现** |
| panes（activity/btw/queue 模块）+ streaming cursor/delta | **已实现** |
| 批准 / AskUser / tasks / sessions / todos | **已实现** |

## 四、REST / WS / ACP

| 能力 | 状态 |
|------|------|
| HTTP REST v1（meta/sessions/messages/approvals） | **已实现** `kkagent server --http` |
| WebSocket `/api/v1/ws` | **已实现** |
| ACP stdio JSON-RPC | **已实现** `kkagent acp` |

## 五、保留未做

- [ ] **Kimi 专用 provider / files API**（按用户要求保留）
- [ ] OpenAI Responses API（可选增强）
- [ ] tree-sitter-bash 完整 AST（现为启发式 shell_safety）
- [ ] VSCode 插件 / node-sdk / 可视化 vis

## 六、验证

```
cargo test -p kkagent-core -p kkagent-tools --lib
cargo build -p kkagent
kkagent server --http 127.0.0.1:8787
kkagent acp
```
