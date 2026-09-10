# kkagent TodoList 工具设计（现状梳理）

> 基于 `main @ 73cc06c` 的代码通读整理。文中每条结论都带 `文件:行号`，可直接跳转核对。
> 注意：`ca8663c`（增量 op 版重构）与 `ecc1c62` 已被 `73cc06c` / `75f09ae` 完整 revert，当前代码是「全量替换」版本（见 §12）。

## 1. 一句话结论

TodoList 是一个**单工具、全量替换语义**的会话级任务清单：模型每次写入整份列表，工具实例持有「本回合可写快照」，`Session` 持有跨回合真值并落盘到 `state.json`，UI 侧通过 `AgentEvent::TodoUpdated` 得到同一份快照。跨回合状态一致靠**每回合重建工具注册表并 seed** 实现，而不是靠工具直接绑定 session service。

## 2. 组件与文件地图

| 层 | 位置 | 职责 |
| --- | --- | --- |
| 模型接口 | `crates/kkagent-tools/src/builtin/todo.rs` | `TodoListTool`：schema、读/写/清空、渲染、`Mutex<Vec<TodoItem>>` 实例态 |
| 会话真值 | `crates/kkagent-core/src/session/todo.rs` | `SessionTodoService`（`RwLock<Vec<TodoItem>>`）、`render_todo_list`、`parse_todo_items` |
| 服务挂载 | `crates/kkagent-core/src/session/services.rs:37,88` | `SessionServices.todos` |
| 提交/持久化 | `crates/kkagent-core/src/session/runtime.rs:556-590` | `todo_items()` / `set_todos_persisted()`，meta key `todos`（`runtime.rs:180`） |
| 回合编排 | `crates/kkagent-core/src/agent_loop.rs:2177-2227` | 写后提交、hook、事件发送 |
| 注册表 seed | `crates/kkagent/src/main.rs:4777-4833, 5110-5117, 7548-7569` | 每回合用 `session.todo_items()` 重建工具 |
| 协议 | `crates/kkagent-protocol/src/events.rs:113-117, 306-311` | `AgentEvent::TodoUpdated`、`TodoItemEvent` |
| 提醒 | `crates/kkagent-core/src/agent_loop.rs:589-599`、`system_reminder.rs:17-22` | 8 回合未更新提醒 |
| TUI | `crates/kkagent-tui/src/components.rs:1697-1964`、`app.rs:274-277, 13160-13172` | 粘性面板 |

## 3. 数据模型与状态词汇

三套并行类型，字段名互不相同：

- 核心内部：`TodoItem { title, status }`，`status: TodoStatus = pending | in_progress | done | cancelled`（`core/session/todo.rs:6-19`）。
- 工具内部：`TodoItem { title: String, status: String }`，字符串态，与核心类型无转换关系（`builtin/todo.rs:10-14`）。
- 协议/对外：`TodoItemEvent { id: String, content: String, status: String }`，status 为自由字符串（`protocol/events.rs:306-311`）。

词汇映射与归一化：

- **内部 `done` ↔ 对外 `completed`**：`builtin/todo.rs:48-55` 的 `display_status` 与 `runtime.rs:565-571` 各写了一份同义映射。
- **入站归一化**有三份副本：`builtin/todo.rs:39-46`、`core/session/todo.rs:75-80`、`runtime.rs:1523-1528`，都接受 `completed|done`、`in-progress|in_progress`、`canceled|cancelled`，其余落到 `pending`。
- **`id` 由下标推导**：`format!("{}", i + 1)`（`builtin/todo.rs:57-71`），`todo_items()` 同样按序号生成（`runtime.rs:556-574`）。因此 id 不是稳定标识，列表增删会导致 id 漂移，消费端不能把它当 key 用。
- 空/纯空白 title 在写入时被丢弃（`builtin/todo.rs:153-161`、`runtime.rs:1519-1522`）。

## 4. 工具契约（模型看到什么）

工具名 `TodoList`，描述「管理结构化 TODO 列表；传 `todos` 替换整表，省略则读，传空数组清空」（`builtin/todo.rs:101-107`）。

入参 schema（`builtin/todo.rs:108-130`）只有一个可选 `todos` 数组，item 要求 `title` + `status`：

| 调用形态 | 语义 | 返回 content |
| --- | --- | --- |
| 省略 `todos` | 读 | 渲染后的当前列表，或 `Todo list is empty.`（`145-149`、`73-90`） |
| `todos: []` | 清空 | `Todo list cleared.`（`172-174`） |
| `todos: [...]` | **整表替换**（无 patch/merge） | `Todo list updated.\n` + 全量渲染（`175-180`） |
| `todos` 非数组 | 错误 | `'todos' must be an array`（`182`） |

要点：

- `title` 之外还兼容 `content` 字段（`builtin/todo.rs:153-155`），这是为 ACP/legacy 输入留的口子。
- schema 的 status enum 只有 `pending|in_progress|done`，**`cancelled` 在模型侧不可达**，只能经 legacy 分支、seed 进来的历史数据或外部协议产生（`builtin/todo.rs:119-123` vs `39-46`）。
- `read_only() = true`（`builtin/todo.rs:131-133`），同时 `TodoList` 在权限默认放行集合 `READ_ONLY_TOOLS`（`core/permission.rs:48-59`）→ 写入不弹审批。
- 资源访问被判为 `tool_accesses::all()`（`tools/accesses.rs:289-291`）。
- 披露层级走 `Tool::disclosure()` 默认 `Inline`（`tools/lib.rs:76-78`），即 schema 常驻每次请求，不参与 `SelectTools` 延迟加载。
- 即使在 session 工具策略里被关闭也仍然可用：`tool_allowed()` 把 `TodoList` 与交互原语一起硬编码保留（`agent_loop.rs:3410-3443`）。这与 `WritePlan/ExitPlanMode` 的理由一致——避免中途改写 `tools[]` 前缀击穿 provider prompt cache。
- 子代理 profile：`GENERAL`、`CODER` 含 `TodoList`（`tools/lib.rs:365-397`），`EXPLORE` 不含（`399-400`）。
- **每次写入都把整份列表回灌进 tool result**。这是设计上的显性成本：模型上下文里每写一次就多一条全量渲染。§12 那次被回退的重构正是要干掉这点。

## 5. 一次写入的落地顺序

提交点在 `agent_loop.rs:2177-2227`，仅在 `!output.is_error && name == "TodoList"` 时触发：

1. `session.turns_since_todo = 0`（`2180`）——写成功即重置提醒计数。
2. `todo_items_from_output(&output)`（`2179`、`3369-3383`）从 `ToolOutput.data.items` **反解**出 `TodoItemEvent` 列表；没有 `data.items` 就返回 `None`，本回合不提交。
3. `session.set_todos_persisted(items)`（`2183`）→ 先 `services.todos.set_todos(...)` 更新内存真值，再把整份 JSON 写进 metadata `custom["todos"]` 并 `update(..., touch_updated_at=true)`；`update` 末尾固定 `self.persist()`（`session/metadata.rs:150-207`），即**每次写都同步落盘**。失败只 `tracing::warn!`，不阻断回合（`2184-2185`）。
4. 之后才 fire `PostToolCall` hook（`2192-2205`）、发 `ToolResult`（`2207-2217`）、发 `AgentEvent::TodoUpdated`（`2219-2227`）。

代码注释解释了 3 为什么要排在 hook/UI 之前：`Commit the latest TodoList snapshot before any hook or UI await so an app exit cannot strand a visibly completed update.`（`2177-2178`）。即：**先落盘，再让 UI 看到**，避免应用退出时出现「面板显示已更新但没持久化」的分裂态。

另注：`maybe_append_concurrent_write_reminder`（`runtime.rs:1193-1202`）只作用于 `Bash/Edit/Write`，TodoList 不参与并发写告警。

## 6. 核心设计：每回合重建注册表 + seed

这是 TodoList 状态同步真正的机制，也是最容易被误读的一点。

`build_turn_tool_registry()`（`main.rs:4777`）在**每个 turn 开始前**新建一个 `ToolRegistry`：

- 先 `register_builtin_tools` → 注册一个空的 `TodoListTool::new()`（`main.rs:4788`、`tools/lib.rs:285`）；
- 随后 `tools.register(Arc::new(TodoListTool::with_items(todos)))` 覆盖它（`main.rs:4831-4833`）；
- `ToolRegistry::register` 是 `BTreeMap::insert`，**后注册者胜**（`tools/registry.rs:24-26`），所以生效的是 seed 版。BTreeMap 的名字序遍历同时保证 `tools[]` 前缀字节稳定（`registry.rs:9-14`）。
- 注入源是 `session.todo_items()`（`main.rs:5113`），另一处同样模式在 `main.rs:7548-7569`。

于是形成这样的分工：

| 角色 | 生命周期 | 是否权威 |
| --- | --- | --- |
| `TodoListTool.todos: Mutex<...>` | 单个 turn | 否——本回合的可写快照，回合内多次调用共享同一实例（读己之写成立） |
| `SessionServices.todos: RwLock<...>` | 会话 | 是——跨回合真值，同时是 HTTP/ACP 的读取源 |
| `state.json` `custom["todos"]` | 磁盘 | 是——重启/resume 后的真值 |

推论（推断，非代码注释）：

- 工具实例态不会跨 turn 泄漏，也不会污染别的 session（每 session 每 turn 各一份注册表）。
- 若 §5 第 3 步持久化失败但内存 `set_todos` 已成功，两者会短暂不一致：内存真值仍被 `todo_items()` 读到，因此下一 turn seed 不丢；**但进程重启后按 state.json 恢复，会回到写入前的列表**。
- 全量替换 + 每回合 seed 意味着「模型不写 → 列表原样保留」，不存在自动过期。

## 7. 持久化与恢复

- Meta key：`const TODOS_META_KEY: &str = "todos"`，写在 session `state.json` 的 `custom` 里（`runtime.rs:180`、`582`）。
- Session 构造时统一回灌：`runtime.rs:390-393` 读 `todo_items_from_metadata` → `services.todos.set_todos(todo_service_items(...))`，覆盖 Resume / Fork / Startup 全部来源。
- 无 live `Session` 时读盘：`load_persisted_todos(session_id, working_dir)`（`runtime.rs:1578-1584`，经 `load_persisted_metadata` 直接 `state.json`），调用点 `main.rs:9099`。
- 对外读取：`"todos": session.todo_items()`（`main.rs:7104`、`9180`）；回归测试 `main.rs:13114-13189` 断言会话被 checkout 进 agent loop 期间 `get_session` 仍返回 live 列表。
- 回合进行中的 HTTP view：`TodoUpdated` 会把最新 items 覆写进 in-flight 快照对象的 `todos` 字段（`main.rs:6460-6471`）。
- `crates/kkagent-rpc/src/http.rs:807-811` 的 `list_tools` 是一份硬编码字符串清单（含 `TodoList`），与注册表无关，属占位实现。

## 8. 提醒机制

- 计数器 `Session::turns_since_todo: u32`（`runtime.rs:236-237`，初值 `445`）。
- 每个 turn 起点 `+1`，`>= 8` 时以 **user 消息**注入 `<system-reminder>` 并归零（`agent_loop.rs:589-599`）；写成功在 §5 第 1 步归零。
- 文案要求「Do not mention this reminder to the user」。
- 同一段文案有第二个来源 `system_reminder::todo_reminder()`（`system_reminder.rs:17-22`），但 `agent_loop` 用的是内联字面量，helper 目前无调用者 → 两处会漂移。
- **系统提示词里没有任何 TodoList 使用指引**：`default_system_prompt()`（`runtime.rs:1790`）正文不含 todo 字样。模型侧的引导只有工具 description + 8 回合提醒。

## 9. 子代理与 MCP serve 路径

- 子代理注册表只走 `register_core_tools`（`core/subagent_runtime.rs:110`、`core/internal_subagent.rs:115`）→ `TodoListTool::new()` 空 seed。**父列表不继承给子，子的列表也不回流父**；父看到的仍是自己 session 的列表。
- 子的注册表在其一次运行内只构建一次，不在子的回合边界重建，因此对子而言工具实例态近似其真值（`set_todos_persisted` 仍会写子 session 的 scratch 目录，而 `SessionCreateSource::Subagent` 不落 session store、退出即删，`runtime.rs:1592-1601`）。
- `kkagent mcp serve` 的 task 路径同样 `register_core_tools`（`src/mcp_serve.rs:3486-3487`），不 seed 既有会话列表。

## 10. TUI 呈现规则

状态：`AppState.todos: Vec<TodoItem>` + `todos_expanded: bool`（`app.rs:274-277`，`app.rs:985-996` 的 `TodoItem.is_finished()` = completed|done|cancelled）。

- 事件驱动刷新：`AgentEvent::TodoUpdated` → 全量覆写 `state.todos`；空或**全部完成**时自动收起（`app.rs:13160-13172`）。
- 快照 hydration 同理（`app.rs:9344-9357`）。
- 面板可见条件：非空 且（已展开 或 存在未完成项）（`components.rs:1832-1835`）→ 全完成后默认消失，除非手动展开。
- 上限 `TODO_MAX_VISIBLE = 5`（`1697`）。折叠态是 kimi 风格选择器：**先全部 in_progress，再 pending + 最新 done**，`cancelled` 永不计入可见项也不计入分类计数（`1887-1964`）。
- 溢出提示 `… +N more (x done · y in progress · z pending) · ctrl+t to expand`，展开后 `all N items · ctrl+t to collapse`（`1799-1823`）。
- 图标：`●` in_progress、`✓` completed（删除线）、`✗` cancelled、`○` pending（`1837-1870`）；`normalize_todo_status` 把 `done` 归一为 `completed`（`1872-1877`）。
- 窄终端按 Unicode 显示宽度逐行估算折行高度（`1746-1760`）。
- `ctrl+t` 切换展开/收起（`app.rs:4522-4528`），但**带门控**：仅当 `todos.len() > 5` 或全部完成时才响应——短且仍在进行的列表按 `ctrl+t` 无效果。换标签、新建会话等路径显式复位为 `false`（`8701`、`8708`、`8999`、`9356`、`11138`、`13170`）。
- 标签页运行时快照 `SessionRuntimeState::capture/restore` 携带 `todos` 但**不含** `todos_expanded`（`app.rs:476-540`）→ 展开态不随标签页恢复。
- 切换/新建/移除会话时会 `state.todos.clear()`（`app.rs:8995`、`10380`、`10448`、`11134`）。

## 11. 冗余与缺口（客观清单）

代码事实：

1. **双份状态镜像**（工具 `Mutex` + `SessionTodoService`），靠 seed / commit 两条边维持一致，是这套设计最大的复杂度来源。
2. `SessionTodoService::render()`、`clear()` 以及 `render_todo_list`、`parse_todo_items` 无生产调用者，只在 `session/mod.rs:91` 被 re-export（`core/session/todo.rs:39-66, 68-84`）。
3. 状态归一化 3 份副本、`done→completed` 映射 2 份、TUI 内还有第 4 份 `normalize_todo_status`（`components.rs:1872-1877`）。
4. `cancelled` 在工具 schema 不可达（§4）。
5. `UndoParticipant::Todos` 被声明并列入 `default_undo_participants`，但除 `context_memory.rs:113,121` 外无任何消费者 → 回合 undo 实际不回滚 todo 列表。
6. `full_compaction.rs` 不含 todo 逻辑 → 压缩后模型上下文中不再持有列表，只能靠再调一次 `TodoList`（无参读）恢复视图。
7. legacy 分支仍在：`action`/`items`/`merge` 路径（`builtin/todo.rs:136-139, 187-246`），其 `merge=true` 按 **title 全等**识别同一项（`228-232`），与新语义（整表替换）并存两套写入模型。
8. 插件可覆盖 `TodoList`（`docs/plugin-development.md:137` 列为高危需 opt-in；`core/plugin_overrides.rs:1-8` 说明覆盖在注册表构造最后一步 rebind）。此时 `agent_loop.rs:2179` 仍按 `name == "TodoList"` 提交，要求替换实现产出同构的 `data.items`，否则 §5 第 2 步拿到 `None`，列表静默不持久化。

推断（需实测确认）：

- 若一个 turn 内模型先写后读，读到的是实例态，与 session 真值同源；但若**同一 turn 内 `set_todos_persisted` 失败**，UI 与内存仍显示新值，重启后回退（§6）。

## 12. 已回退的设计方向

`ca8663c refactor(todo): make Todo an execution-state attention scheduler` 与 `ecc1c62 fix(todo): compact completed Todo write args...` 已被 `73cc06c` / `75f09ae` revert。从 revert 的 diffstat 可确认被移除的正是：`crates/kkagent-protocol/src/todo.rs`（429 行，把 service 上提到 protocol 以打破 core/tools 依赖环）、`core/context_projector.rs`（240 行，把窗口外的历史 Todo args/result 折叠成最小桩）以及 `tests/todo_projection.rs`。该方向的内容是：增量 transition op（add/update/complete/cancel/list/clear）、稳定 per-task id、单一权威 session service、写操作只回摘要、强制单一 in_progress 并自动晋升下一条、删除 8 回合提醒。

保留这一节的意义：当前设计里 §4 的全量回灌、§3 的下标 id、§8 的提醒计数，都是那次重构想一次性消解的对象；若重新推进，需要连 §11 的 1/2/3/5 一起处理。

## 13. 外部对照（参考，非本仓库事实）

主流 coding agent 的 todo 工具收敛于两点：一是**全量替换 + 每次回灌**（Claude Code `TodoWrite`、kimi 的 todo 系），kkagent 当前属于这一类；二是把清单升级为**带稳定 id 的任务系统**（Qoder 侧 `TaskCreate/TaskUpdate/TaskGet/TaskList` 的分立工具面）。kkagent 曾尝试走第二类（§12），已回退。
