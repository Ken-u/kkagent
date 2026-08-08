# kkagent 与 ref/kimi-code 功能差距清单

> 核对更新（2026-08-09 二次）：聚焦 **agentcore / tools / tui** 对照 `ref/kimi-code`（`agent-core` + `agent-core-v2` + `apps/kimi-code/tui` + `pi-tui`）。
>
> 体量对照（约）：
> | 模块 | kkagent | ref | 约覆盖 |
> |------|---------|-----|--------|
> | agentcore | `kkagent-core` ~2.8k 行 / 9 文件 | `agent-core-v2/agent` ~36k 行 | ~8% |
> | tools | `kkagent-tools` ~3.5k 行 | `agent-core/tools` ~13k 行 | ~27% |
> | tui | `kkagent-tui` ~4.3k 行 / 6 文件 | `tui`+`pi-tui` ~54k 行 | ~8% |
>
> 结论：工具**名单**已基本齐；agentcore / tui 仍是骨架级，深度能力差距最大。Kimi provider 按要求保留未做。

---

## 一、工具集（Tools）— 名单 vs 深度

| 工具 | 状态 | 相对 ref 差距 |
|------|------|----------------|
| Read / Write / Edit | **名单+基础增强** | 缺 list-directory 辅助、更完整 result-builder / args-validator |
| Grep / Glob | **已增强** | 缺独立 `rg-locator` / rule-match 策略层（功能大部分可用） |
| Bash | **已增强** | 缺 tree-sitter 命令 AST 安全分析、env/stdin 精细控制 |
| TodoList | **已对齐** | — |
| EnterPlanMode / ExitPlanMode | **已实现** | — |
| CreateGoal / GetGoal / UpdateGoal | **已实现** | **缺独立 `SetGoalBudget`**；`wall_clock_budget_ms` 未暴露 |
| Task / TaskOutput / TaskList / TaskStop | **已实现** | 输出落盘有；缺 background task 展示协议深度（display schemas） |
| Agent / AgentSwarm | **已实现** | profile 有；缺 swarm 编排/roster/预算联动的 core 侧深度 |
| AskUserQuestion | **已实现** | 缺 question-as-background-task |
| Skill | **已实现** | 缺 builtin sub-skill（consolidate/review 等）与官方 skill 包 |
| WebSearch / FetchURL | **已实现** | moonshot/local 有；缺 provider 抽象与失败回退矩阵 |
| ReadMediaFile | **浅实现** | **缺** image-compress / image-limits / originals / webp-decode / video-delivery |
| CronCreate/List/Delete | **浅实现** | **仅内存**；缺 persist、jitter、cron-expr、fire-xml、session-store、遥测事件 |
| SelectTools | **已实现** | 缺 toolActivation / toolPolicy 分层（激活规则、工作区策略） |
| MCP | **传输完整** | stdio/SSE/HTTP+OAuth 有；缺 tools display / 状态事件与 core 深度集成 |

### Tools 待办（相对 ref 仍缺）

- [x] `SetGoalBudget` 独立工具 + Goal 预算热更新
- [x] Goal `wall_clock_budget_ms` 暴露与强制停
- [x] Cron **持久化**（落盘 `~/.kkagent/cron.json` + 重启恢复）+ jitter
- [ ] Cron fire 注入协议（XML/结构化 reminder，对齐 ref `cron-fire-xml`）
- [ ] ReadMediaFile：压缩、尺寸上限、原图保留、webp 解码、视频投递策略
- [ ] Bash：tree-sitter / AST 危险命令分析（可复用 `tree-sitter-bash`）
- [ ] git-worktree 支持（子代理隔离工作树）
- [ ] 工具 `args-validator` + `display/schemas`（给 TUI chip/summary 用）
- [ ] Skill builtin / sub-skill 目录对齐
- [ ] AskUserQuestion → background task（可异步回答）
- [ ] Web provider 抽象（moonshot / local）与统一错误码

---

## 二、Agent 核心（agentcore）— 骨架有，深度缺

当前 `kkagent-core`：`agent_loop` / `permission` / `session` / `tool_scheduler` / `subagent_runtime` / `transcript` / `git_context`。

| 能力 | 状态 | 相对 ref（agent-core-v2） |
|------|------|---------------------------|
| max_steps / loop_control | **已接** | — |
| 并行 ToolScheduler | **已实现** | — |
| ToolResult 截断外置 | **已实现** | — |
| PermissionChain | **简化对齐** | 策略链有；缺完整 policy 模块化与 path-utils 深度 |
| Undo（文件快照+截断消息） | **已实现** | 缺 conversationUndoParticipants / 多参与者协调 |
| SelectTools allowlist | **已实现** | — |
| Hooks | **部分** | 仅触发 TurnStart / PreToolCall；**未触发** PostToolCall / TurnEnd / SessionStart/End / Notification |
| Compaction | **浅** | LLM 总结+截断；缺 fullCompaction strategy / handoff / vacuous fold / contextMemory |
| 系统提醒 | **部分** | todo reminder + AGENTS.md 注入；缺 agentsMdReminder 生命周期、interruptionReminder、dateChange 专用模块 |
| Git 上下文 | **已实现** | — |
| 子代理镜像 | **已实现** | — |
| tokenCounting | **未做** | 无预估 / 无超窗裁剪前计数 |
| contextProjector | **未做** | `build_messages` 直出，无投影/预算裁剪 |
| toolDedupe | **未做** | 无 canonical-args 去重 |
| stepRetry | **未做** | LLM/工具步失败无结构化重试 |
| usage 预算回路 | **浅** | Goal 有 tokens_used；loop 未强制停、TUI `/usage` 弱 |
| blob / media resolve | **未做** | turn 内媒体解析与 blob 存储缺失 |
| replayBuilder | **未做** | 无法从 wire/transcript 重建 loop 状态 |
| activityView | **未做** | 无活动视图模型供 TUI/RPC |
| plugin（core 侧） | **未做** | — |
| ModelCapability 注册表 | **未做** | — |
| eventBus | **未做** | 仅 mpsc `AgentEvent`，无统一总线/订阅 |
| shellCommand 分析 | **未做** | — |
| goal 注入主循环 | **浅** | 工具可建 Goal；缺 goal injection 驱动多 turn 自治回路 |
| swarm roster / 预算 | **浅** | 并行 spawn 有；缺名册与资源治理 |
| scopeContext / userTool | **未做** | — |

### AgentCore 待办（P0）

- [x] **tokenCounting**：请求前估算 + 超窗告警/触发 compact
- [x] **contextProjector**：按预算投影消息（工具结果折叠、旧 turn 摘要）
- [x] **auto compact**：超窗时本地 digest + `compact_messages`（fullCompaction/handoff 仍待深化）
- [ ] **fullCompaction**：strategy + handoff + contextMemory（不仅 DB/本地截断）
- [x] **Hooks 全事件**：PostToolCall / TurnEnd / SessionStart 已触发（SessionEnd / Notification / block-rewrite 仍缺）
- [x] **toolDedupe**：同 turn 相同工具+canonical args 去重 + 跨 turn streak reminder
- [x] **stepRetry**：空/失败流按 `max_attempts_per_step` 重试
- [x] **usage 回路**：goal turn·token·wall-clock 预算强制结束
- [x] **goal injection**：active goal reminder + budget gate（连续自治多 turn 调度仍可加强）
- [x] **ModelCapability**：按模型声明 tools/vision/thinking/max_context
- [ ] **blob + media resolve**：附件入库与 turn 内解析
- [ ] **replayBuilder**：从 transcript/wire 重建可续跑状态
- [ ] **activityView**：供 TUI activity-pane / RPC 的结构化活动流
- [ ] **permissionPolicy 模块化**：与 ref 策略文件一一对应、可单测
- [ ] **systemReminder 模块**：agentsMd / interruption / dateChange / plan / todo 统一注入点
- [ ] **eventBus**：统一事件订阅（telemetry / TUI / RPC / hooks）
- [ ] **plugin 钩子面**：工具/提示/命令扩展点（可后置，但 core 需预留）

---

## 三、TUI — 差距最大

> `kkagent-tui` ~4.3k 行 vs ref `tui`(~41k) + `pi-tui`(~12k)。部分斜杠命令与面板已可用，但渲染/编辑器/会话/媒体仍是最小实现。

### 已有（浅对齐）

- [x] 基础对话流 + markdown 行样式
- [x] 工具调用折叠行 + ctrl+o 展开截断
- [x] 批准面板 / AskUserQuestion 面板
- [x] Todo 面板、Tasks 面板（详情/停止）
- [x] Session / Model picker（列表）
- [x] Footer 状态（模型/权限/plan 等）
- [x] `/` 自动补全菜单
- [x] 斜杠已实现：`yolo/auto/permission/plan/model/effort/help/new/sessions/compact/goal/undo/init/title/status/usage/mcp/tasks/copy/export-md/version/exit`

### 斜杠命令仍缺（相对 ref `tui/commands`）

- [ ] `/config` — 查看/修改运行配置
- [ ] `/auth` — 认证状态与登录
- [ ] `/plugins` — 插件管理
- [ ] `/skills` — 技能列表与查看
- [ ] `/swarm` — swarm 状态
- [ ] `/provider` — 切换 provider/model 目录
- [ ] `/reload` — 重载配置
- [ ] `/web` — Web 搜索/抓取入口
- [ ] `/info` — 系统信息（版本/路径/模型/token）
- [ ] `/add-dir` — 添加工作目录
- [ ] `/btw` — 备注/提醒
- [ ] `/prompts` — 提示模板
- [ ] `/experimental-flags` — 实验开关
- [ ] 命令注册表增强：`complete-args` / `dispatch` / `resolve`（参数补全与路由）

### 7.1 tool-renderers

- [x] `registry` — 按工具名分发（`tool_renderers.rs`）
- [x] `chip` — 紧凑芯片摘要
- [x] `summary` — 结果摘要（截断+折叠）
- [x] `truncated` — 「显示更多」提示
- [ ] `media` — 图片/视频/音频
- [ ] `goal` — Goal 状态专用面板
- [x] 分工具渲染初版：bash 着色、diff 色、grep 高亮（json 折叠仍缺）

### 7.2–7.4 controllers / panes

- [ ] `streaming-ui` — token 级流式 + 光标动画（当前偏块更新）
- [ ] `session-event-handler` / `subagent-event-handler` — 事件路由模块化
- [ ] `session-replay` — wire 回放重建 UI
- [ ] `auth-flow` / `plugin-update-notifier` / `cache-hint` / `clipboard-image-hint`
- [ ] `activity-pane` / `btw-panel` / `queue-pane`

### 7.5–7.8 dialogs / editor / media / chrome

- [ ] 设置对话框（配置编辑）
- [ ] 多行编辑器：选区 / 撤销重做 / 粘贴防抖（现有 input 很薄）
- [ ] vim 模式（可选）
- [ ] 终端内嵌图（iTerm2 / Kitty / Sixel）
- [ ] 状态栏增强（token 实时 / cache hit）
- [ ] tab strip 多会话
- [ ] 标题栏（会话标题 / cwd）

### 7.9–7.10 session-picker / pi-tui 基础库

- [ ] 会话搜索/过滤/预览/fork/删除（现仅列表恢复）
- [ ] `autocomplete` / `fuzzy` / `kill-ring` / `undo-stack` / `paste-burst`
- [ ] `terminal-image` / `terminal-colors` / `word-navigation` / `keybindings`
- [ ] 组件库：`box/text/truncated-text/editor/markdown/loader/select-list/...`
- [ ] diff 渲染 / 双缓冲核心

### 7.11–7.12 其他 + reverse-rpc

- [ ] export-markdown 增强、terminal-theme/focus/notification/state
- [ ] MCP 状态面板 + MCP OAuth UI
- [ ] background-task / background-agent 状态条
- [ ] goal-queue-store / goal-completion 通知
- [ ] thinking-config UI、hook-result 展示
- [ ] reverse-rpc `approval/` / `question/`（与 core 正式协议化；现为通道直连）

---

## 四、权限 / 子代理（交叉）

| 能力 | 状态 |
|------|------|
| 敏感路径 / git 控制路径 | **有**（简化） |
| user allow/ask/deny 规则 | **有** |
| Profile 子代理 | explore/coder/general |
| 任务输出持久化 | `.kkagent/tasks/<id>.md` |
| 镜像事件 | spawned/started/completed/failed + child → 父 TUI |
| git-worktree 隔离 | **未做** |
| swarm roster / 预算治理 | **未做** |

---

## 五、Skill / MCP / 配置

| 能力 | 状态 |
|------|------|
| Skill 扫描与调用 | **已实现**（缺 builtin sub-skill） |
| secondary_model | **已实现** |
| 环境变量覆盖 | KKAGENT_* / OPENAI_API_KEY / ANTHROPIC_API_KEY / GOOGLE_API_KEY |
| trusted_workspaces | **已实现** |
| MCP `[type]` | stdio / sse / http / streamable-http + oauth |

---

## 六、LLM Provider

| 能力 | 状态 |
|------|------|
| Anthropic Messages | 已实现 |
| OpenAI Chat Completions | 已实现（含重试） |
| Google GenAI SSE | 已实现（含重试） |
| **Kimi 协议** | **未做（保留）** |
| OpenAI Responses API | **未做** |
| 错误重试 | 已实现（429/5xx/timeout） |

---

## 七、其它已落地

- [x] Git 上下文注入
- [x] DI（`kkagent-di`）
- [x] Wire 1.0→1.5 + journal JSONL
- [x] 云遥测 CloudAppender（`KKAGENT_TELEMETRY_CLOUD=1`）
- [x] MCP SSE/HTTP + OAuth
- [x] 并行 ToolScheduler
- [x] 子代理事件镜像

---

## 八、优先级总览（按影响）

### P0 — 先补齐「能正确跑长会话」

1. [x] agentcore：`tokenCounting` + `contextProjector` + auto compact
2. [x] agentcore：Hooks 主事件 + usage/goal 预算强制停
3. [x] tools：`SetGoalBudget` + Cron 持久化
4. [~] tui：tool-renderers（chip/summary/truncated）已落地；streaming-ui 仍缺

### P1 — 体验与可观测

5. tui：斜杠补齐（config/skills/swarm/provider/info…）+ activity/queue pane
6. agentcore：toolDedupe / stepRetry / ModelCapability / activityView
7. tools：ReadMediaFile 深度 + Bash AST + display schemas
8. tui：editor / session-picker 增强 / chrome tab-strip

### P2 — 外部对接（原 P1 大项，仍重要但非本次三模块焦点）

#### REST / WebSocket（`kap-server` → `kkagent-rpc`）

> 当前 `kkagent-rpc` 仅基础 codec/transport；ref `kap-server` 完整 REST+WS。

- [ ] sessions/messages/prompts/approvals/questions/tools/tasks/skills/files/fs/workspaces/config/auth/oauth/modelCatalog/meta/snapshot/search/export…
- [ ] WS v1：连接、事件广播、journal、subagent roster、fsWatch、inFlightTurn
- [ ] 中间件：auth / rateLimit / CORS / securityHeaders / schema
- [ ] OpenAPI / envelope / 错误处理

#### ACP（`acp-adapter` + `acp-server`）

> 当前完全缺失；对接 IDE 客户端必需。

- [ ] adapter：session/convert/events-map/approval/question/mcp/model-catalog/modes/slash…
- [ ] server：fs/terminal/interaction-bridge/replay…

### P3 — 增强

- Kimi provider / files API（按要求保留）
- tree-sitter-bash、SSH(kaos)、视频输入、thinking effort 精细控制
- 结构化输出 json_schema、models.dev、plugin 市场、vscode、node-sdk
- minidb（当前 SQLite/JSON 功能近似）、migration-legacy

---

## 九、建议落地顺序（agentcore → tools → tui）

```
1) contextProjector + tokenCounting + compact 触发
2) Hooks 全事件 + usage/goal 预算停
3) SetGoalBudget + Cron persist
4) tool-renderers + streaming-ui
5) toolDedupe / stepRetry / ModelCapability
6) 媒体工具深度 + Bash AST
7) TUI 斜杠/面板/editor/session-picker
8) REST/WS → ACP（对外）
```
