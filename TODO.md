# kkagent 与 ref/kimi-code 功能差距清单

> 核对更新（2026-08-09）：除 **Kimi provider** 外，TODO 第五节缺口已落地。

## 一、工具集（Tools）

| 工具 | 状态 |
|------|------|
| Read / Write / Edit / Grep / Glob / Bash | **已实现**（Bash **AST** + 启发式安全） |
| TodoList / Plan / Goal* / SetGoalBudget | **已实现** |
| Task* / Agent* / AskUserQuestion | **已实现** |
| Skill / WebSearch / FetchURL / ReadMediaFile / Cron* / SelectTools / MCP | **已实现** |

## 二、AgentCore

| 能力 | 状态 |
|------|------|
| contextMemory / toolPolicy / swarm / usage / undo / media / scope | **已实现** |
| tokenCounting / compact / hooks / dedupe / blob / plugin / … | **已实现** |
| **Session 子系统对齐**（store/index/workdir-key、metadata `state.json`、lifecycle、activity、interaction、agentLifecycle、todo/btw/cron、toolPolicyGate、swarm batch、subagent、terminal/process、init/instructions、skill+profile catalog、mcp view、export、external hooks；RPC：fork/archive/rename/export） | **已实现** |

## 三、TUI

| 能力 | 状态 |
|------|------|
| pi-tui 编辑器原语 + chrome + reverse-rpc + controllers | **已实现** |
| **terminal-image**（Kitty / iTerm2 协议编码） | **已实现** |
| Welcome / footer git 徽章 / 流式光标 / 滚动提示 | **已实现** |
| Ctrl-F 转录搜索 overlay + `/search` | **已实现** |
| Ctrl-G btw 侧栏 + 输入框状态标题 | **已实现** |

## 四、REST / WS / ACP

| 能力 | 状态 |
|------|------|
| HTTP 全路由矩阵（tools/tasks/skills/fs/files/workspaces/config/modelCatalog/search/snapshot/prompts/questions/terminals/connections/export + WS） | **已实现** |
| HTTP ↔ **AgentLoop / ServerState 共享绑定**（`AgentHttpBackend`） | **已实现** |
| ACP：terminal / model-catalog / modes / slash / approval / 事件映射 | **已实现** |

## 五、平台包

| 包 | 状态 |
|------|------|
| **OpenAI Responses API**（`/v1/responses` + catalog 自动选择） | **已实现** |
| **kkagent-oauth**（PKCE / device code / token storage，无 Kimi 身份） | **已实现** |
| **kkagent-kaos**（local + SSH via system ssh/scp） | **已实现** |
| **sdk/node**（`@kkagent/sdk` HTTP + JSON-RPC） | **已实现** |
| **apps/vscode**（ACP/HTTP 最小扩展） | **已实现** |
| Bash AST（`bash_ast`，tree-sitter-bash 语义对齐） | **已实现** |

## 六、保留未做

- [ ] **Kimi 专用 provider / files API / Kimi OAuth identity**（按用户要求保留）

## 七、验证

```
cargo test -p kkagent-core -p kkagent-tools -p kkagent-llm -p kkagent-tui -p kkagent-oauth -p kkagent-kaos -p kkagent-acp -p kkagent-rpc --lib
cargo build -p kkagent
kkagent server --http 127.0.0.1:8787
kkagent acp
```
