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

## 十五、优先级 P1 — 三大核心差距（最重要）

> 以下三项是 kkagent 与 ref/kimi-code 之间最重大的功能缺失，影响用户体验和外部对接能力，需优先实现。

### 7. TUI 全面增强（`pi-tui` + `apps/kimi-code/tui` → `kkagent-tui`）

> 当前 TUI 仅 6 文件 / 4,258 行，ref 有 221 文件 / ~200k+ 行，覆盖率约 5%。这是**差距最大的模块**。

**7.1 tool-renderers（工具结果渲染器）**
- [ ] `tool-renderers/registry.ts` — 工具渲染注册表，按工具名分发到对应渲染器
- [ ] `tool-renderers/chip.ts` — 工具调用芯片（紧凑摘要）
- [ ] `tool-renderers/summary.ts` — 工具结果摘要（截断 + 折叠）
- [ ] `tool-renderers/truncated.ts` — 大结果截断渲染（"显示更多"）
- [ ] `tool-renderers/media.ts` — 图片/视频/音频结果渲染
- [ ] `tool-renderers/goal.ts` — Goal 状态专用渲染器
- [ ] 各工具独立渲染逻辑（bash 输出着色、diff 视图、grep 高亮、json 折叠等）

**7.2 commands（斜杠命令系统）**
- [ ] `/config` — 查看和修改运行配置
- [ ] `/auth` — 认证状态查看和登录
- [ ] `/goal` — Goal 管理（创建/暂停/恢复/取消/查看预算）
- [ ] `/plugins` — 插件管理（安装/卸载/列表）
- [ ] `/skills` — 技能列表和查看
- [ ] `/session` — 会话管理（列表/切换/恢复/fork/清除）
- [ ] `/swarm` — 子代理 swarm 状态查看
- [ ] `/provider` — 切换 LLM provider / model
- [ ] `/copy` — 复制最后回复到剪贴板
- [ ] `/undo` — 撤销上次文件操作
- [ ] `/reload` — 重新加载配置
- [ ] `/web` — Web 搜索/抓取
- [ ] `/info` — 系统信息（版本/路径/模型/token 用量）
- [ ] `/add-dir` — 添加工作目录
- [ ] `/btw` — 备注 / 提醒
- [ ] `/prompts` — 提示模板管理
- [ ] `/experimental-flags` — 实验标志开关
- [ ] 命令注册表 `registry.ts` + 自动补全 `complete-args.ts`

**7.3 controllers（TUI 控制器）**
- [ ] `streaming-ui.ts` — 流式 UI 状态机（token 逐字渲染 + 光标动画）
- [ ] `session-event-handler.ts` — 会话事件路由（message/tool/turn/goal/mcp 事件分发到组件）
- [ ] `subagent-event-handler.ts` — 子代理事件处理（spawned/started/completed/failed → TUI 更新）
- [ ] `tasks-browser.ts` — 任务浏览器（列表/详情/停止）
- [ ] `session-replay.ts` — 会话回放（从 wire 记录重建 UI）
- [ ] `auth-flow.ts` — 认证流程（OAuth 登录引导）
- [ ] `editor-keyboard.ts` — 编辑器键盘控制（vim 模式可选）
- [ ] `cache-hint-controller.ts` — prompt cache 提示
- [ ] `clipboard-image-hint.ts` — 剪贴板图片检测提示
- [ ] `plugin-update-notifier.ts` — 插件更新通知
- [ ] `btw-panel.ts` — BTW 面板控制

**7.4 panes（侧面板）**
- [ ] `activity-pane.ts` — 活动面板（当前 turn 的工具调用流）
- [ ] `btw-panel.ts` — BTW 备注/提醒面板
- [ ] `queue-pane.ts` — 队列面板（goal queue / pending tasks）

**7.5 dialogs（对话框系统）**
- [ ] 确认对话框（工具批准 / goal 创建确认）
- [ ] 选择对话框（AskUserQuestion 多选/单选）
- [ ] 输入对话框（文本输入 / 搜索过滤）
- [ ] 设置对话框（配置编辑）

**7.6 editor 组件**
- [ ] 多行文本编辑器（光标移动 / 选区 / 复制粘贴 / 撤销重做）
- [ ] vim 模式支持（可选）
- [ ] 行号 / 语法高亮

**7.7 media 渲染**
- [ ] 终端内嵌图片（iTerm2 / Kitty / Sixel 协议）
- [ ] 图片缩略图和占位符
- [ ] 图片附件存储和预览

**7.8 chrome（框架装饰）**
- [ ] 状态栏（模型 / token 用量 / 权限模式 / plan mode 指示）
- [ ] tab strip（多会话标签页）
- [ ] 标题栏（会话标题 / 工作目录）

**7.9 session-picker（会话选择器）**
- [ ] 会话列表（最近会话 / 搜索 / 过滤）
- [ ] 会话预览（消息摘要 / 创建时间 / 工作目录）
- [ ] 新建 / 恢复 / fork / 删除

**7.10 pi-tui 基础组件库**
- [ ] `autocomplete.ts` — 自动补全（命令 / 文件路径 / 模型名）
- [ ] `fuzzy.ts` — 模糊搜索
- [ ] `kill-ring.ts` — kill ring（Emacs 式剪贴板历史）
- [ ] `undo-stack.ts` — 撤销栈
- [ ] `paste-burst.ts` — 粘贴防抖（大粘贴分片注入）
- [ ] `terminal-image.ts` — 终端图片渲染
- [ ] `terminal-colors.ts` — 256 色 / truecolor 检测
- [ ] `word-navigation.ts` — 单词级光标移动
- [ ] `native-modifiers.ts` — 平台修饰键适配（Meta/Ctrl/Alt）
- [ ] `stdin-buffer.ts` — stdin 原始模式缓冲
- [ ] `keybindings.ts` — 可配置快捷键
- [ ] `tui.ts` — TUI 核心渲染引擎（diff 渲染 / 双缓冲）
- [ ] 组件：`box / text / truncated-text / input / editor / markdown / loader / cancellable-loader / select-list / settings-list / spacer / image`

**7.11 其他 TUI 功能**
- [ ] `export-markdown.ts` — 导出会话为 Markdown
- [ ] `session-picker-rows.ts` — 会话选择行渲染
- [ ] `terminal-theme.ts` — 终端主题适配
- [ ] `terminal-focus.ts` — 终端焦点管理
- [ ] `terminal-notification.ts` — 终端通知（标题闪烁 / 系统通知）
- [ ] `terminal-state.ts` — 终端状态跟踪（尺寸 / 滚动 / 交替屏幕）
- [ ] `mcp-server-status.ts` — MCP 服务器状态面板
- [ ] `mcp-oauth.ts` — MCP OAuth 流程 UI
- [ ] `background-task-status.ts` — 后台任务状态
- [ ] `background-agent-status.ts` — 后台代理状态
- [ ] `goal-queue-store.ts` — Goal 队列状态存储
- [ ] `goal-completion.ts` — Goal 完成通知
- [ ] `thinking-config.ts` — Thinking effort 配置 UI
- [ ] `hook-result-format.ts` — Hook 结果格式化显示
- [ ] `input-latency.ts` — 输入延迟测量
- [ ] `message-replay.ts` — 消息回放
- [ ] `refresh-providers.ts` — Provider 刷新
- [ ] `startup.ts` — 启动流程（banner / 版本检查 / 配置加载提示）
- [ ] `printable-key.ts` — 可打印键检测
- [ ] `render-cache.ts` — 渲染缓存
- [ ] `cache-hint.ts` — 缓存命中提示
- [ ] `dead-terminal.ts` — 死终端检测和恢复
- [ ] `tab-strip.ts` — 标签页栏
- [ ] `searchable-list.ts` — 可搜索列表组件
- [ ] `status-line-command.ts` — 状态行命令
- [ ] `media-url.ts` — 媒体 URL 处理
- [ ] `component-capabilities.ts` — 组件能力声明
- [ ] `shell-output.ts` — Shell 输出渲染
- [ ] `image-placeholder.ts` — 图片占位符
- [ ] `image-attachment-store.ts` — 图片附件存储
- [ ] `object-patch.ts` — 对象补丁（增量更新）
- [ ] `transcript-window.ts` — Transcript 窗口
- [ ] `transcript-component-metadata.ts` — Transcript 组件元数据
- [ ] `transcript-id.ts` — Transcript ID
- [ ] `plugin-source-label.ts` — 插件来源标签
- [ ] `foreground-task.ts` — 前台任务跟踪
- [ ] `event-payload.ts` — 事件载荷
- [ ] `tmux-keyboard.ts` — tmux 键盘适配
- [ ] `errors.ts` — TUI 错误处理
- [ ] `mcp-tool-name.ts` — MCP 工具名显示

**7.12 reverse-rpc（反向 RPC）**
- [ ] `reverse-rpc/approval/` — 工具批准反向 RPC（从 TUI 发送到 core）
- [ ] `reverse-rpc/question/` — 用户问题反向 RPC（AskUserQuestion 响应）

---

### 8. REST / WebSocket 服务端（`kap-server` → `kkagent-rpc`）

> 当前 `kkagent-rpc` 仅 324 行基础 codec/transport，ref `kap-server` 有 139 文件，包含完整 REST API v1/v2、WebSocket、认证、限流等。**几乎没有服务端实现。**

**8.1 REST API v1/v2 路由**
- [ ] `routes/sessions.ts` — 会话 CRUD（创建/列表/获取/删除/fork）
- [ ] `routes/v2/sessions.ts` — v2 会话接口
- [ ] `routes/messages.ts` — 消息发送和列表
- [ ] `routes/prompts.ts` — Prompt 发送
- [ ] `routes/approvals.ts` — 工具批准（批准/拒绝）
- [ ] `routes/questions.ts` — 用户问题回答
- [ ] `routes/tools.ts` — 工具列表和详情
- [ ] `routes/tasks.ts` — 任务管理（列表/输出/停止）
- [ ] `routes/skills.ts` — 技能列表和调用
- [ ] `routes/files.ts` — 文件读写
- [ ] `routes/fs.ts` — 文件系统操作（列表/搜索/读写）
- [ ] `routes/workspaceFs.ts` — 工作区文件系统
- [ ] `routes/workspaces.ts` — 工作区管理
- [ ] `routes/config.ts` — 配置读写
- [ ] `routes/auth.ts` — 认证（登录/登出/token 刷新）
- [ ] `routes/oauth.ts` — OAuth 回调
- [ ] `routes/modelCatalog.ts` — 模型目录
- [ ] `routes/meta.ts` — 元信息（版本/能力）
- [ ] `routes/snapshot.ts` — 会话快照
- [ ] `routes/search.ts` — 搜索
- [ ] `routes/sessionExport.ts` — 会话导出
- [ ] `routes/connections.ts` — 连接管理
- [ ] `routes/terminals.ts` — 终端管理
- [ ] `routes/guiStore.ts` — GUI 状态存储
- [ ] `routes/shutdown.ts` — 关闭
- [ ] `routes/webAssets.ts` — Web 静态资源
- [ ] `routes/registerApiV1Routes.ts` — v1 路由注册
- [ ] `routes/registerApiV2Routes.ts` — v2 路由注册
- [ ] `routes/action-suffix.ts` — 路由 action 后缀处理

**8.2 WebSocket v1（实时通信）**
- [ ] `transport/ws/v1/wsConnectionV1.ts` — WebSocket v1 连接管理
- [ ] `transport/ws/v1/registerWsV1.ts` — v1 协议注册
- [ ] `transport/ws/v1/protocol.ts` — v1 协议定义
- [ ] `transport/ws/v1/events.ts` — 事件定义
- [ ] `transport/ws/v1/sessionEventBroadcaster.ts` — 会话事件广播
- [ ] `transport/ws/v1/sessionEventJournal.ts` — 事件 journal
- [ ] `transport/ws/v1/subagentRosterTracker.ts` — 子代理名册跟踪
- [ ] `transport/ws/v1/fsWatchBridge.ts` — 文件系统监听桥接
- [ ] `transport/ws/v1/inFlightTurnTracker.ts` — 进行中 turn 跟踪
- [ ] `transport/ws/bearerProtocol.ts` — Bearer token 协议
- [ ] `transport/ws/connectionRegistry.ts` — 连接注册表
- [ ] `transport/channel.ts` — 通道抽象
- [ ] `transport/channelRegistry.ts` — 通道注册表
- [ ] `transport/dispatcher.ts` — 消息分发器
- [ ] `transport/mainAgent.ts` — 主代理绑定
- [ ] `transport/serviceDispatcherRoutes.ts` — 服务分发路由
- [ ] `transport/registerDebugRoutes.ts` — 调试路由
- [ ] `transport/errors.ts` — 传输错误

**8.3 中间件**
- [ ] `middleware/auth.ts` — 认证中间件（Bearer token / 密码）
- [ ] `middleware/rateLimit.ts` — 限流中间件
- [ ] `middleware/origin.ts` — CORS 来源检查
- [ ] `middleware/hostnames.ts` — 主机名白名单
- [ ] `middleware/securityHeaders.ts` — 安全响应头
- [ ] `middleware/schema.ts` — 请求 schema 验证
- [ ] `middleware/validate.ts` — 参数验证
- [ ] `middleware/defineRoute.ts` — 路由定义辅助

**8.4 安全服务**
- [ ] `services/auth/authTokenService.ts` — 认证 token 服务
- [ ] `services/auth/credentials.ts` — 凭证管理
- [ ] `services/auth/password.ts` — 密码哈希/验证
- [ ] `services/auth/persistentToken.ts` — 持久 token
- [ ] `services/auth/privateFiles.ts` — 私有文件保护
- [ ] `services/auth/tokenStore.ts` — Token 存储
- [ ] `security/bindClassify.ts` — 绑定分类（安全级别）

**8.5 其他服务**
- [ ] `services/transcript/transcriptService.ts` — Transcript 服务
- [ ] `services/transcript/coreBinding.ts` — Core 绑定
- [ ] `services/transcript/coreEventMap.ts` — Core 事件映射
- [ ] `services/transcript/wireRecords.ts` — Wire 记录
- [ ] `services/messages/messageHistory.ts` — 消息历史
- [ ] `services/messages/messageProjection.ts` — 消息投影
- [ ] `services/modelCatalog/modelCatalogRefreshScheduler.ts` — 模型目录刷新调度
- [ ] `services/guiStore/guiStore.ts` — GUI 状态存储
- [ ] `services/guiStore/guiStoreService.ts` — GUI 状态服务
- [ ] `services/legacyStatus/legacyStatus.ts` — 旧版状态兼容
- [ ] `services/pinoLoggerService.ts` — 结构化日志
- [ ] `services/telemetry.ts` — 遥测服务

**8.6 协议层**
- [ ] `protocol/envelope.ts` — 请求/响应信封
- [ ] `protocol/tool.ts` — 工具协议
- [ ] `protocol/goal.ts` — Goal 协议
- [ ] `protocol/request-id.ts` — 请求 ID
- [ ] `protocol/pagination.ts` — 分页协议
- [ ] `protocol/rest-question.ts` — REST 问题协议

**8.7 OpenAPI / 基础设施**
- [ ] `openapi/` — OpenAPI 文档生成
- [ ] `envelope.ts` — 响应信封封装
- [ ] `contract.ts` — 服务契约定义
- [ ] `error-handler.ts` — 全局错误处理
- [ ] `request-id.ts` — 请求 ID 生成
- [ ] `requestLogging.ts` — 请求日志
- [ ] `instanceRegistry.ts` — 实例注册表

---

### 9. ACP 适配层（`acp-adapter` + `acp-server`）

> ref 有 `acp-adapter`（19 文件）+ `acp-server`（26 文件）= 45 文件。当前**完全缺失**。ACP (Agent Client Protocol) 是与 VSCode 插件等外部客户端对接的标准协议层。

**9.1 ACP Adapter（适配器层）**
- [ ] `server.ts` — ACP 服务器启动和生命周期
- [ ] `session.ts` — ACP 会话管理（创建/恢复/关闭）
- [ ] `convert.ts` — ACP ↔ 内部消息格式转换
- [ ] `events-map.ts` — 内部事件 → ACP 事件映射
- [ ] `approval.ts` — 工具批准 ACP 桥接
- [ ] `question.ts` — 用户问题 ACP 桥接
- [ ] `mcp.ts` — MCP 配置 ACP 桥接
- [ ] `model-catalog.ts` — 模型目录 ACP 桥接
- [ ] `modes.ts` — 权限模式（plan/auto/yolo）ACP 桥接
- [ ] `auth-methods.ts` — 认证方式适配
- [ ] `config-options.ts` — 配置选项适配
- [ ] `builtin-commands.ts` — 内置命令适配
- [ ] `slash.ts` — 斜杠命令适配
- [ ] `marker.ts` — ACP 标记协议
- [ ] `log-guard.ts` — 日志保护
- [ ] `kaos-acp.ts` — KAOS → ACP 适配
- [ ] `types.ts` — ACP 类型定义
- [ ] `version.ts` — 版本协商

**9.2 ACP Server（服务端层）**
- [ ] `server.ts` / `start.ts` — ACP 服务端启动
- [ ] `acp-client.ts` — ACP 客户端通信
- [ ] `acp-fs/` — ACP 文件系统桥接（读写/搜索/监听）
- [ ] `acp-terminal/` — ACP 终端桥接（创建/写入/调整/关闭）
- [ ] `interaction-bridge.ts` — 交互桥接（approval/question 转发）
- [ ] `replay.ts` — 会话回放
- [ ] `approval.ts` — 批准流程服务端
- [ ] `question.ts` — 问题流程服务端
- [ ] `session.ts` — 会话管理服务端
- [ ] `convert.ts` — 格式转换服务端
- [ ] `events-map.ts` — 事件映射服务端
- [ ] `builtin-commands.ts` — 内置命令服务端
- [ ] `slash.ts` — 斜杠命令服务端
- [ ] `model-catalog.ts` — 模型目录服务端
- [ ] `modes.ts` — 模式管理服务端
- [ ] `auth-methods.ts` — 认证方法服务端
- [ ] `config-options.ts` — 配置选项服务端
- [ ] `marker.ts` — 标记服务端
- [ ] `log.ts` — 日志服务端
- [ ] `types.ts` — 类型定义
- [ ] `version.ts` — 版本服务端

---

## 十六、其他差距（P2-P3，非紧急）

### P0 核心功能（已在上方记录，此处交叉引用）
- tokenCounting + contextProjector — 无上下文预算管理
- ModelCapability 注册表
- usage 统计（token/turn 预算追踪）
- permissionPolicy（敏感文件/git控制路径/auto/yolo）
- OpenAI Responses API
- eventBus 统一事件总线

### P2 架构差异
- minidb 自研存储引擎（当前用 SQLite/JSON，功能等价）
- DI 容器增强（instantiation/descriptors/lifecycle/extensions）
- transcript 数据模型（frame/turn/step/task/todo/attachment 完整模型）
- plugin 系统（manager/source/archive/github-resolver）
- session index/search
- session export (zip/manifest)
- workspace 管理（多目录/instructions/toolPolicy/fsWatch/context）

### P3 增强功能
- Kimi 专用 provider / schema / files API（按用户要求保留）
- tree-sitter-bash（bash 命令安全分析）
- SSH 远程执行（kaos）
- 视频输入支持（uploadVideo）
- thinking effort 控制（withThinking）
- 结构化输出（ResponseFormat json_object/json_schema）
- models.dev 模型发现
- undo 撤销文件操作
- migration-legacy（旧版 kimi-cli 迁移）
- node-sdk（Node.js SDK）
- kimi-inspect（调试工具）
- vscode（VSCode 插件）
- vis（可视化工具）
