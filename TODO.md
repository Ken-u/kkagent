# kkagent 与 ref/kimi-code 功能差距清单

> 核对更新（2026-08-09）：TUI + AgentCore 深度对齐一轮已落地。**Kimi provider 按要求保留未做。**

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

## 二、AgentCore（本轮加深）

| 能力 | 状态 |
|------|------|
| tokenCounting / contextProjector / auto-compact | **已实现** |
| fullCompaction（keep/vacuous/handoff） | **已实现** |
| **contextMemory**（vacuous fold / loop-event fold / handoff / undo participants） | **已实现** |
| **toolPolicy** 分层激活（workspace/profile/global/session） | **已实现** |
| **swarm** enter/exit + AgentSwarm 排他 + roster | **已实现** |
| **usage** 会话级 token/cache/steps | **已实现** |
| **undoService** 多参与者协调 | **已实现** |
| **media_pipeline**（@path / mime / size gates） | **已实现** |
| **scopeContext** | **已实现** |
| Hooks / toolDedupe / stepRetry / ModelCapability / Goal 预算 | **已实现** |
| blob / replay / eventBus / activity / plugin / systemReminder | **已实现** |

## 三、TUI（本轮加深）

| 能力 | 状态 |
|------|------|
| **pi-tui** 原语：fuzzy / kill-ring / undo / paste-burst / word-nav / keybindings / autocomplete | **已实现** |
| Editor：Ctrl-A/E/K/W/Y/Z、word 移动、bracketed paste | **已实现** |
| **chrome**：tab strip + status bar model | **已实现** |
| **reverse-rpc**：approval/question modal coordinator | **已实现** |
| **controllers**：session event router / streaming UI / cache hint | **已实现** |
| 斜杠命令（含 `/swarm enter|exit`、config/auth/skills/…） | **已实现** |
| tool-renderers / panes / streaming cursor | **已实现** |
| 批准 / AskUser / tasks / sessions / todos | **已实现** |

## 四、REST / WS / ACP

| 能力 | 状态 |
|------|------|
| HTTP REST v1（meta/sessions/messages/approvals） | **已实现** `kkagent server --http` |
| WebSocket `/api/v1/ws` | **已实现** |
| ACP stdio JSON-RPC | **已实现** `kkagent acp` |
| RPC：`swarm.enter` / `swarm.exit` / `session.usage` | **已实现** |

## 五、保留未做 / 仍浅

- [ ] **Kimi 专用 provider / files API**（按用户要求保留）
- [ ] OpenAI Responses API（可选增强）
- [ ] tree-sitter-bash 完整 AST（现为启发式 shell_safety）
- [ ] VSCode 插件 / node-sdk / kaos SSH / oauth 平台包
- [ ] pi-tui terminal-image（Kitty/iTerm/Sixel）真内嵌
- [ ] kap-server 全量路由矩阵（tools/fs/workspaces/…）与 AgentLoop 深绑定
- [ ] ACP 终端桥 / model-catalog / 完整事件映射

## 六、验证

```
cargo test -p kkagent-core -p kkagent-tui --lib
cargo build -p kkagent
kkagent server --http 127.0.0.1:8787
kkagent acp
```
