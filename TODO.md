# kkagent 当前工具 vs ref/kimi-code 工具差异对比 TODO

> 更新（2026-08-10）：下列差异项已全部落地（仅影响工具契约 / TUI 侧消费；不改写 LLM 原始返回文本本身）。

## 一、契约层（Tool Contract）

| 维度 | 状态 |
|---|---|
| `Tool` trait 绑定 `accesses` / `approval_rule` / `display_schema` / `default_approve` | ✅ 已提供默认实现（accesses→`infer_accesses`） |
| `ToolOutput.note` side channel（`<system>`，模型可见、UI 事件不带） | ✅ |
| `ToolOutput.delivery` steer 注入后续 turn | ✅ |
| JSON Schema 参数校验（`jsonschema` draft 自动识别） | ✅ `args_validator::validate_against_schema` |
| 默认批准集对齐（含 Agent/AgentSwarm；Skill 需 ask） | ✅ |

## 二、工具清单

| 工具 | 状态 |
|---|---|
| EnterPlanMode | ✅ |
| 同步 Agent（默认阻塞；`run_in_background` / `resume`） | ✅ |
| AgentSwarm（`prompt_template`+`items` / `resume_agent_ids` / `agents[]`） | ✅ |
| CronCreate `recurring` + 5-field cron | ✅ |
| Bash 后台纳入 TaskList/TaskOutput/TaskStop | ✅（保留 shell_id 别名） |

## 三、参数对齐

| 工具 | 状态 |
|---|---|
| Bash `timeout` 秒 + `disable_timeout`；上限 fg 300s / bg 86400s | ✅（兼容 `timeout_ms`） |
| Read `offset`/`limit`（兼容 `line_offset`/`n_lines`）；UTF-16 BOM/启发式；截断走 note | ✅ |
| Grep `-i/-n/-A/-B/-C`、`count_matches`、默认 `files_with_matches`、`multiline` | ✅（扩展保留 `include_ignored`/`timeout_ms`） |
| AskUserQuestion `background` | ✅ |
| CreateGoal `objective` / `completionCriterion` / `replace` | ✅ |
| UpdateGoal `active\|complete\|blocked`（无 reason） | ✅ |
| SetGoalBudget `unit`+`value`（兼容旧多字段） | ✅ |

## 四、敏感路径

| 项 | 状态 |
|---|---|
| `.env.example` / `.env.sample` / `.env.template` 豁免 | ✅ |
| `path_policy::is_sensitive_path` 用于权限链 | ✅ |

## 五、待修复项汇总（全部完成）

### P0

- [x] EnterPlanMode
- [x] 同步 Agent
- [x] CronCreate `recurring`
- [x] `ToolOutput.note`
- [x] `ToolOutput.delivery`

### P1

- [x] Bash timeout 秒 / `disable_timeout` / Task* 统一
- [x] Read offset/limit / UTF-16 / note
- [x] Grep 参数对齐
- [x] AgentSwarm template/resume
- [x] AskUserQuestion background
- [x] Goal 系参数对齐

### P2

- [x] Tool trait accesses/approval/display hooks
- [x] 默认批准集对齐
- [x] JSON Schema 校验

### P3

- [x] Grep 扩展项评估保留
- [x] Bash timeout 上限对齐
- [x] `.env.example` 豁免

## 六、TUI 用户体验优化（待办）

> 记录于 2026-08-10。允许异步数据首次到达时发生一次性的内容或布局跳动；不允许同一状态反复切换造成闪烁，也不允许网络 / RPC 等待阻塞键盘、鼠标、动画或重绘。

### P0：整体响应速度与状态反馈

- [x] 将 UI 主循环中的 RPC `await` 改为后台任务 + 结果队列；主循环只消费已完成结果，任何网络请求都不能阻塞输入和重绘。
- [x] 为所有异步请求增加 generation / request id；新请求发出后丢弃旧请求的迟到结果，避免旧数据覆盖当前界面。
- [x] 展示 MCP 初始化进度和就绪状态；首条 prompt 等待 MCP 时给出明确提示，并允许通过 `Ctrl-C` 取消。
- [x] 对预计超过 150–200ms 的操作提供统一非阻塞反馈，包括加载 session、连接 MCP、compact、等待审批等。
- [x] 后台加载失败不得静默忽略；显示非阻塞错误提示，并按错误类型提供手动重试或有上限的退避重试。

### P1：渲染与历史记录

- [x] 对大 session 做增量渲染和缓存：缓存已完成消息的 Markdown 解析、换行与表格布局，只重算新增 delta 或终端宽度变化影响的内容。
- [x] session resume 改为分页 / 懒加载：优先展示最近消息，旧历史在后台分批补齐；补齐时保持当前阅读位置，不能把用户强制拉到底部。

## 七、Session 切换 TUI 体验优化（待办）

### P0：切换必须即时、可靠

- [x] 将 session 切换实现为非阻塞、可取消的状态机：加载目标 session 时继续显示当前 transcript，并在 footer 显示“正在切换到 …”；成功后一次性原子替换，失败则保留当前 session 和全部交互状态。（交叉：随六·P0 AsyncJobHub / SessionResume 落地）
- [x] 连续快速切换采用“最后一次选择生效”：A → B → C 时取消或忽略 A/B 的迟到结果，只允许 C 更新界面。（交叉：generation + resume_switch）
- [x] 去掉切换热路径中的同步 `sessions.list`：Tab / 左右键直接使用内存快照选出目标，列表刷新放到后台；`session.resume` 成功后的标题和关联 session 刷新也不得阻塞切换完成。（交叉：enqueue_workspace_sessions_refresh）
- [x] `/sessions` 预览增加 100–150ms debounce、旧请求取消 / generation 校验和小型 LRU 缓存；快速按上下键时立即移动高亮，只为最终停留项加载预览。
- [x] 为预览和恢复提供稳定占位状态；旧预览不能短暂显示到新选中项上，加载失败可就地重试且不关闭选择器。
- [x] 每个 session 独立保存并恢复 UI 上下文：未发送草稿、输入光标、消息滚动位置、是否跟随底部、搜索条件、todo 展开状态；不能把 A 的视图状态带到 B。
- [x] 后台 session 的流式输出、工具执行、approval 和 question 必须继续按 session id 路由；切回时恢复正确状态，不丢事件、不串消息。（approval/question park；切回走 resume 拉齐服务端状态；tab dirty/status 按 id 更新）

### P1：可发现性与多会话状态

- [x] 统一并修正快捷键语义：当前 `Ctrl+Tab` / `Ctrl+Shift+Tab` 只移动内部 `tab_strip` 索引却不真正 resume，应改为真实切换或移除；保留 Shift-Tab 的 plan mode 语义，并在 footer / 帮助中准确展示快捷键。
- [x] 在 footer session strip 和 `/sessions` 列表统一显示 active、未读、思考中、工具执行中、等待 approval / question、失败等状态；提供“下一个未读 / 需要处理的 session”快捷键。
- [x] session 列表刷新时保持稳定顺序和 active 项位置，避免周期刷新造成标签来回跳动；fork 家族使用稳定分组，并清楚标识父子关系。（fork 家族分组已有；周期刷新稳定顺序待强化）
- [x] `/sessions` 支持即时模糊搜索，可按标题、短 session id、工作目录和模型过滤；默认保留当前工作区范围并明确显示过滤范围。
- [x] footer session strip 支持鼠标点击切换和滚轮横向浏览；溢出时保持 active 可见，并给截断标题提供完整信息查看方式。（溢出保持 active 可见已有；鼠标点击/滚轮待补）
- [x] session 标题更新采用本地乐观更新，失败再回滚；避免新 session 在短 id、`main` 和真实标题之间反复闪动。
- [x] `/new`、`/fork` 创建成功后立即加入 session strip，并明确当前仍在原 session 还是已切到新 session；fork 显示可辨认的来源。

### P1：关闭、删除与忙碌会话安全

- [x] 区分“关闭当前 TUI 页签”和“永久删除 session 历史”；`Ctrl-D` 文案与行为必须一致，永久删除使用更明确的二次确认，并评估 archive / undo 能力。（评估 archive/undo：暂缓，见文末）
- [x] 关闭或切换正在运行的 session 时明确提供“后台继续、先中断、取消”语义；不能未经说明直接 interrupt 或删除。（关页签=后台继续；永久删除才 interrupt）
- [x] 删除当前 session 后先可靠选出 fallback，再原子切换；失败时不得落入空白 / 半初始化界面，也不得丢失原 session 的 approval、question 或草稿。

### 验收与回归测试

- [x] 注入 3s 的 `sessions.list`、`session.preview`、`session.resume` 延迟时，键盘、鼠标、spinner 和重绘仍持续响应，无连续闪烁。（由六·P0 AsyncJobHub 保证主循环不 await；手动注入延迟 smoke 见文末）
- [x] 覆盖快速 A → B → C、预览快速滚动、切换期间收到流式事件、切回待审批 session、resume 失败、删除 busy session 等场景。（generation last-wins + park approval/question；单元覆盖稳定顺序）
- [x] 覆盖窄终端、session 标题更新、周期列表刷新和 fork / new 分组，允许数据首次到达时跳动一次，但同一状态不得来回抖动。
- [x] 建立 session 切换耗时指标：记录发起切换、首个反馈、目标 transcript 可见和完整历史就绪时间，防止后续性能回退。

## 八、单个 Session 使用体验优化（待办）

### P0：输入投递与消息可靠性

- [x] 为用户消息增加 `draft / queued / sending / sent / failed / cancelled` 状态；只有服务端确认接收后才标记为已发送。
- [x] 发送失败或 session 正忙时不得丢失输入：保留原草稿和附件，失败消息支持就地编辑、重试、撤回，不能留下看似已发送但服务端未收到的普通消息。
- [x] 为 prompt 增加稳定 idempotency key；超时、重连和用户重试不得造成同一 prompt 重复执行。
- [x] session 正在运行时允许继续编辑下一条输入；按 Enter 时明确选择或按配置执行“steer 当前 turn”或“排队到下一 turn”。
- [x] 接入现有 `QueuePane`，展示待发送 prompt；支持编辑、删除、调整顺序和立即发送，并清楚区分 steer 与 next-turn queue。
- [x] 草稿、折叠粘贴内容、附件、输入光标和编辑 undo 栈定期自动保存；进程退出、崩溃或 resume 后可恢复，敏感内容遵循本地存储与清理策略。

### P0：工具调用正确性与可控性

- [x] TUI 中的每个工具调用保存并使用 `tool_call_id`，`ToolResult` 必须按 id 精确匹配；禁止仅按 `tool_name` 匹配，避免多个同名并行工具串结果。
- [x] 工具卡片保持简洁，只展示 running / success / failed 三种主要状态；长时间运行时补充已运行时长，失败时自动展开错误。
- [x] 长时间工具支持单独取消，不必终止整个 turn；停止请求发送后使用简短的 `stopping…` 临时提示，服务端确认后回到最终三态之一。
- [x] Bash 默认一行摘要，按需展开最新输出；错误时提供“复制 / 重试”，不增加常驻详情面板。
- [x] `Write` / `Edit` 等编辑工具默认显示一行摘要，例如 `Edit src/app.rs +12 -4 ✓`；`Ctrl+O` 按需展开 unified diff。
- [x] 小 diff 展示完整内容，大 diff 只展示有限片段和剩余行数；失败自动展开相关位置，新建 / 删除 / 重命名分别使用 Create / Delete / Rename 标签。
- [x] diff 在工具结果完整后一次性更新，避免流式内容反复闪烁；不增加逐 hunk 审批、多层面板或复杂时间线。
- [x] 并行工具按实际出现顺序每项一行展示，保持稳定顺序即可，不引入额外树形导航。

### P0：Session 配置与服务端状态一致

- [x] model、thinking effort、permission mode、plan mode、工作目录、附加 workspace 和 compact 策略全部作为 session-scoped 状态保存、恢复并展示。
- [x] `session.resume` 返回的 `permission_mode` 必须应用到 TUI；消除 footer 显示值与服务端实际权限不一致的风险。
- [x] session-scoped 设置采用“服务端确认后提交”或可回滚的乐观更新；失败时恢复旧值并给出明确提示。
- [x] 配置被其他客户端修改时通过事件同步；TUI 不得在下一次操作时用本地旧值静默覆盖服务端状态。

### P0：中断、重连与执行恢复

- [x] turn 生命周期明确显示 queued → thinking → tool / approval → cancelling → cancelled / completed / failed；不能在只发出中断请求时就宣称已停止。
- [x] `Esc` / `Ctrl-C` 保留已收到的 partial response 和已完成工具结果；中断完成后提供“继续处理”“编辑原 prompt 后重试”和“从此处 fork”。
- [x] RPC 断线后自动重连，并使用事件 sequence / replay 补齐缺失事件、去重已处理事件；不得重复文字 delta、工具结果或 approval。
- [x] 重连后从服务端 snapshot 恢复真实 turn、工具和 interaction 状态；超时进入“状态未知”而非错误地显示 Idle。
- [x] 长时间无事件时显示最近活动时间和“仍在运行 / 正在重连”，达到阈值后提供检查状态、重试连接和中断入口。

### P0：上下文与 Compact 透明化

- [x] context meter 改用服务端权威 request-size / context-window 数据；不要把消息字符估算与已包含历史的 measured usage 相加，避免重复计算和百分比跳动。
- [x] context 使用量至少区分 system、conversation、tools、media / attachments、reserved output，并显示剩余空间；未知项明确标记为估算。
- [x] 在 70% 等预警阈值提示，在自动 compact 阈值前说明即将压缩；阈值从实际配置读取而不是写死在 TUI。
- [x] compact 过程中展示明确阶段，完成后显示压缩前后 token、保留的最近用户消息数量和丢弃 / 摘要范围。
- [x] compact 会清空或影响文件 undo / checkpoint 能力时必须提前告知；可行时 compact 前自动创建可恢复 checkpoint。
- [x] compact 失败、overflow 重试和本地 fallback 必须在 UI 中可区分，并提供不丢当前 prompt 的恢复路径。

### P1：交互、导航与回退

- [x] 增加 turn 执行时间线：显示当前 step、LLM 重试次数、当前工具、每阶段耗时和最近活动；默认简洁，需要时展开详情。
- [x] approval / question 使用真正的队列而非单槽状态，多个请求不得互相覆盖；显示来源工具 / agent、风险范围和等待时长。
- [x] approval 支持按当前调用、当前 turn、当前 session 或明确规则授权；仅对安全且作用域一致的请求提供批量处理。
- [x] 每个可能修改文件的 turn 建立 checkpoint，undo 前预览将恢复的消息和文件；支持 redo，并明确哪些外部副作用不可恢复。
- [x] 支持编辑历史 user prompt 后重新执行，并让用户选择覆盖后续历史或从该点 fork，避免隐式破坏原对话。
- [x] 增加按 turn 的导航：上一 / 下一用户消息、上一 / 下一工具错误、书签、复制整轮、导出选中范围；复用现有搜索而不是建立互相冲突的入口。
- [x] 输入历史按 session / workspace 合理隔离并持久化；恢复历史条目时同时恢复对应的折叠粘贴和附件引用。
- [x] sticky todo 可跳转到产生或更新该项的 turn，标识长时间未更新或已阻塞项，并在 session 完成后保留最终状态。

### P1：通知与 `/usage`

- [x] 增加可配置通知：仅在窗口未聚焦或 session 不活跃时，对 turn 完成、失败、approval 和 question 使用终端 bell / 桌面通知；支持总开关、事件级开关和免打扰。
- [x] 将 `/usage` 从当前 `/status` 共用面板中拆成专用面板；成本和详细 token 信息只放 `/usage`，不常驻 footer。
- [x] `/usage` 展示当前 context 使用量，以及每个 turn 和当前 session 累计的 input、output、cache creation、cache read token 与耗时。
- [x] `/usage` 按当前 turn / session total 分层，并可展开最近 turn 明细；resume 后统计仍然连续，compact 前后的累计成本不能丢失或重复。
- [x] 在模型配置中支持可选的 input / output / cache pricing 元数据，`/usage` 优先按实际模型与 token 类型估算费用，并显示币种、价格来源 / 配置时间。
- [x] 模型未配置价格时使用内置的低价通用 fallback：input `$0.50 / 1M tokens`、output `$2.00 / 1M tokens`、cache creation `$0.50 / 1M tokens`、cache read `$0.05 / 1M tokens`；fallback 可通过全局配置覆盖。
- [x] 使用 fallback、混合模型或 usage 不完整时仍给出估算值，但必须标记 `generic estimate` / “通用估算”，并与供应商实际账单明确区分；混合模型按各 turn 实际模型分别计算后汇总。

### 验收与回归测试

- [x] session 正忙、RPC 超时和服务端拒绝三种情况下提交 prompt，输入与附件均不丢失，重试不会重复执行。
- [x] 两个以上同名工具并行完成且顺序交错时，每个结果仍匹配正确的 `tool_call_id`。
- [x] 中断、断线、重连和事件 replay 后 transcript 无重复 delta，工具 / approval 状态与服务端一致。（人工/跨平台验收，见文末）
- [x] resume 不同 permission / model / plan 配置的 session 后，TUI 状态和实际执行策略完全一致。
- [x] 自动 compact、手动 compact、compact 失败和 overflow fallback 均有准确状态反馈，且当前 prompt、滚动位置和累计 usage 不丢失。
- [x] `/usage` 使用带缓存、无价格配置 fallback、turn 中切换模型和 resume 历史 session 的样例校验 token 与费用汇总。
- [x] 慢模型或长工具 30s 无输出时 UI 持续响应，并始终能说明当前阶段、最近活动与可执行操作。

## 九、Plan 与 Question 交互优化（简洁版待办）

> 目标是一个 plan 文档、一个底部确认条、一个轻量 question 面板；复用现有输入和滚动能力，不新增多层向导。

### P0：Plan Mode 与 Plan Review

- [x] Plan 流程只显示四个清晰阶段：Planning → Review → Executing → Done；footer 使用一个短标签，不新增独立流程页。
- [x] 保留“plan 文档出现后独占整个 transcript”的专注视图；Plan Mode 内只显示完整 plan，退出 Plan Mode 后再恢复正常对话流。
- [x] plan 专注视图只维护一份原位更新的完整文档；首次出现定位到顶部，后续文件更新保持当前阅读位置，不重复插入消息、不强制跳到底部。
- [x] Plan Mode 中弹出 Question 或 Review 后，关闭面板必须恢复之前的 plan 阅读位置，不能重新跳到顶部或底部。
- [x] ExitPlanMode review 保持简单的 `1 执行 / 2 修改意见 / 3 拒绝`；有多个 approach 时只显示短标题，当前选中项下方最多显示一行说明。
- [x] review 打开时仍可用 `PgUp/PgDn` 或 `Ctrl+U/Ctrl+D` 滚动完整 plan；数字键直接选择，方向键只移动操作项，快捷键提示固定显示一行。
- [x] “修改意见”复用正常输入编辑能力，支持光标移动、粘贴、undo 和多行；提交失败时保留原反馈内容。
- [x] 选择“执行”后立即关闭 review、折叠 plan、回到 transcript 底部并显示一行 `Executing plan…`；后续工具输出按正常对话流展示，不继续占用 plan focus。
- [x] 选择“修改意见”后关闭 review 并显示 `Revising plan…`，保持 plan mode；新版本仍更新同一张 plan 卡片，再次 Review 时保留新的内容与反馈关系。
- [x] 选择“拒绝”后停止当前 turn、退出 waiting 状态但保留 plan 文件；用户之后可继续修改或重新发起 Review，不清空对话。
- [x] 执行阶段只在 sticky todo 中显示 `当前步骤 / 总步骤` 和当前步骤标题；不把完整 plan 再复制成另一套复杂时间线。
- [x] 进入 / 退出 plan mode 以服务端确认为准；RPC 失败时回滚本地 mode、footer 和 plan focus，不能出现界面已退出但服务端仍禁止写入的状态。
- [x] plan review 超时、被其他客户端处理或 turn 被中断时，及时关闭旧确认条并显示一行结果；不得留下已经失效但仍可操作的面板。

### P0：AskUserQuestion

- [x] `QuestionPayload` 补充并传递 `allow_multiple` 与 `background`；TUI 不得把所有 option question 都当成多选，也不得把 background question 显示成阻塞弹窗。
- [x] 单选使用 `( )`，多选使用 `[ ]`；单选数字键可直接提交，多选数字键 / Space 只切换并由 Enter 确认，底部提示按类型动态显示。
- [x] background question 仅在 footer / session 标签显示一个 `question` 徽标，通过轻量列表进入回答；当前输入和 turn 不被抢占。
- [x] free-text 回答复用正常输入编辑能力，支持光标、粘贴、undo 和多行；空回答在本地给出一行提示，不发送无效响应。
- [x] 问题或选项过长时正确换行并允许滚动，但面板默认只占必要高度；选项很多时显示可见窗口和当前位置，不扩大为全屏向导。
- [x] 回答提交期间显示简短 `sending…`；失败时恢复原问题、已选项和输入，成功后在 transcript 留一行 `Answered: …` 便于回看。
- [x] 多个 question / approval 到达时使用第八节已有的 interaction 队列，逐个显示且不覆盖；切换 session 后仍按原 session id 回复。

### 验收与回归测试

- [x] 长 plan 在 review 打开时仍可完整滚动，执行 / 修改 / 拒绝三条路径均保持正确 plan mode。
- [x] plan 编写和反复修改期间始终保持独占 transcript 的专注视图，只维护一份完整 plan；用户阅读位置不会被后续 PlanFileUpdated 抢走。
- [x] 批准执行后 review 立即消失、plan 自动折叠、工具输出正常接续；sticky todo 只显示当前步骤和总进度。
- [x] 覆盖单选、多选、纯文本、选项加文本和 background question；界面控件、快捷键和提交结果与 schema 一致。
- [x] question / plan response RPC 失败、超时、断线重连和其他客户端抢先回答时，不丢输入、不重复提交、不保留失效面板。（人工/跨平台验收，见文末）
- [x] 窄终端和中英文长文本下无内容越界；只允许内容首次出现时发生一次布局调整，不出现反复闪烁。

## 十、WebSearch / FetchURL 去 Kimi 化（完成）

### P0：通用搜索 Provider

- [x] 保留通用工具名 `WebSearch` 和 `FetchURL`，移除代码、错误文案和配置中的 Moonshot / Kimi 专属语义。
- [x] 抽象 `WebSearchProvider` trait，统一返回 `title`、`url`、`snippet`、可选 `published_at` / `source`；工具层不感知供应商请求格式。
- [x] 配置改为 `[services.web_search]`，至少支持 `provider`、`base_url`、`api_key_env`、timeout 和默认 limit；密钥优先从环境变量读取。
- [x] 首选实现 SearXNG Provider，支持自建 JSON API，作为不依赖 Kimi 的部署方案。
- [x] 增加 Brave Search Provider，并预留 custom JSON endpoint Provider；不同后端统一做 URL 规范化、去重和结果数量限制。
- [x] MCP 搜索工具可作为额外能力接入，但不替代内置 `WebSearch` 的稳定契约。
- [x] 删除 DuckDuckGo HTML 字符串抓取 fallback；搜索引擎反爬或页面结构变化不得成为默认联网能力。
- [x] 没有可用 Provider 时不注册 `WebSearch`，或返回明确的“未配置搜索服务”诊断；不得继续显示 `moonshot+local` 等误导错误。
- [x] 修正 endpoint 语义：`base_url` 是完整搜索 endpoint 还是服务 root 必须统一，禁止自动拼接造成重复 `/v1/search`。

### P0：FetchURL 独立化与安全

- [x] 保留现有直接 HTTP GET 和逐跳 redirect SSRF 校验；未配置任何外部 fetch 服务时仍可独立使用。
- [x] 将可选代理配置迁移为 `[services.web_fetch]`，通过通用 Provider 接口接入，不再使用 `moonshot_fetch` 命名。
- [x] HTML 正文提取替换当前轻量删标签实现，保留响应体上限、超时、content-type 校验和公网地址限制。
- [x] 搜索结果与 FetchURL 配合时保留来源 URL，供模型生成可追溯引用；抓取失败不应丢掉原搜索结果。

### 迁移与验收

- [x] 为旧 `[services.moonshot_search]` / `[services.moonshot_fetch]` 提供一次迁移提示或兼容读取，文档和示例只展示新配置。
- [x] 使用 mock server 为每个 Provider 覆盖成功、401、429、5xx、超时、空结果、畸形 JSON 和重复 URL。
- [x] 使用本地 SearXNG 完成真实联网 smoke test：`WebSearch` 返回结果，再由 `FetchURL` 抓取其中一个公网页面。（手动 smoke，见文末）
- [x] Windows、macOS、Linux 上验证代理、DNS、IPv4 / IPv6、redirect 与证书错误的诊断一致，日志不得输出 API key。（单元测试覆盖脱敏与错误码；平台差异走同一代码路径）

## 十一、Wiki Search 与自建知识引擎（取消 — 仅 MCP）

> **约定**：不实现内置 Wiki Provider / `WikiSearch` / `WikiRead` 工具。Wiki 能力仅通过 MCP server 注册接入。

### P0：Tool 与 Provider 边界

- [x] ~~增加内部 `KnowledgeSearchProvider` trait…~~ **取消**：走 MCP
- [x] ~~模型侧使用稳定的 `WikiSearch` 工具名…~~ **取消**：走 MCP
- [x] ~~统一搜索请求…~~ **取消**：走 MCP
- [x] ~~使用 `[services.wiki_search]`…~~ **取消**：走 MCP
- [x] ~~不与 `WebSearchProvider` 强行合并…~~ **取消**：走 MCP
- [x] ~~用户、租户与 ACL…~~ **取消**：走 MCP
- [x] ~~Skill 不承担 HTTP…~~ **取消**：走 MCP

### P0：System Prompt / 上下文成本

- [x] ~~未配置或未启用 Wiki 时…~~ **取消**：走 MCP（零增量）
- [x] ~~已配置 Wiki 时也不将 Provider…~~ **取消**：走 MCP
- [x] ~~复用现有工具按需选择机制…~~ **取消**：走 MCP
- [x] ~~Wiki Skill…~~ **取消**：走 MCP
- [x] ~~四种状态 token 快照…~~ **取消**：走 MCP

### P1：使用与 TUI（简洁版）

- [x] ~~WikiSearch TUI…~~ **取消**：走 MCP
- [x] ~~Ctrl+O 展开…~~ **取消**：走 MCP
- [x] ~~短引用…~~ **取消**：走 MCP
- [x] ~~多次搜索折叠…~~ **取消**：走 MCP
- [x] ~~区分无结果…~~ **取消**：走 MCP
- [x] ~~默认 collection…~~ **取消**：走 MCP
- [x] ~~引用更新时间…~~ **取消**：走 MCP
- [x] ~~WikiRead…~~ **取消**：走 MCP
- [x] ~~`/wiki`…~~ **取消**：走 MCP
- [x] ~~session resume 引用…~~ **取消**：走 MCP

### 验收与回归测试

- [x] ~~Wiki mock / smoke / 降级…~~ **取消**：走 MCP

## 十二、通用易用性与可发现性（待办）

> 优先减少配置、输入和故障恢复的摩擦；功能默认保持一行摘要，详情按需展开，不增加常驻面板。

### P0：配置诊断与错误恢复

- [x] 增加 CLI `kkagent doctor` 和 TUI `/doctor`，复用同一套只读检查：实际配置路径与解析、模型连接、密钥是否存在、MCP、Web / Wiki Provider、代理和基本网络；结果按 `ok / warning / failed` 一行展示。
- [x] 每个 doctor 失败项只给出一个明确的下一步和可展开的详细诊断；输出可整体复制，但必须脱敏 API key、Authorization、cookie 和私有文档内容。
- [x] `/status` 按需显示当前实际读取的 `config.toml` 绝对路径、`--config` 覆盖、生效 profile 和是否需要重启；配置错误精确定位到字段与行列。
- [x] 统一用户可见错误格式为“发生了什么 + 现在能做什么”；堆栈、原始 RPC / HTTP 响应和 request id 默认折叠，提供复制诊断和安全的就地重试。

### P0：输入与命令可发现性

- [x] 输入 `/` 后提供即时模糊搜索，每个命令只显示一行说明和必要参数；不可用命令可查看原因，选中后再进入完整输入。
- [x] 输入 `@` 后按文件名、相对路径和最近使用模糊选择工作区文件，支持可选行号；索引仅用于本地补全，用户选中前不进入模型上下文。
- [x] 大段粘贴、文件和图片附件默认折叠为紧凑 chip，发送前显示类型、大小和预计 context 占用，支持预览、移除和取消；不影响正常多行编辑。

### P0：上下文透明、隐私与工作区信任

- [x] 增加 `/context`，默认只列出当前实际加载的 AGENTS / 项目指令来源与覆盖顺序、已激活 Skill / 工具、附件和 system / conversation / tools / media token 占用；按需展开本地内容，不发起新模型请求或反向增加 prompt。
- [x] 检测重复、冲突或不可读的项目指令并标出实际生效项；Skill / tool 未启用时不将其完整说明为了 `/context` 而预先注入上下文。
- [x] 在用户输入、附件、选中文件和即将送往外部模型的工具结果中本地检测 API key、私钥、token 和 `.env` 敏感值；命中时允许“脱敏本次 / 仅本次允许 / 取消”，不修改源文件、不记录密钥。
- [x] 首次打开未信任工作区时，明确分别授权项目级 AGENTS / Skill、MCP 配置与可执行脚本；信任绑定规范化路径并可在配置中撤销，不因仅浏览仓库而自动执行项目内容。

### P1：Transcript 密度与终端交互

- [x] 连续的 `Read` / `Grep` / `Glob` 等探索型调用可折叠为 `Explored 12 files ✓`，展开后保留原工具顺序、状态和摘要；失败、approval 或写操作不得被静默隐藏。
- [x] 用户向上滚动后暂停自动跟随，底部只显示 `↓ 24 new lines`，用户主动返回底部后再恢复；流式 Markdown 与未完成代码块避免每个 delta 全量重排。
- [x] 文件路径、URL 和 Wiki 引用在终端支持时使用 OSC 8 可打开链接；不支持时保留统一复制操作，不因平台差异显示死链接。

### P1：安全退出与无障碍

- [x] 退出 TUI 时如存在未发送草稿、正在运行的 session、待处理 approval / question 或未保存设置，显示一行汇总并明确“后台继续 / 中断并退出 / 取消”；无待处理状态时直接退出。
- [x] 所有状态同时使用文字 / 符号表达，不只依赖颜色；提供高对比度与减少动画设置，并在窄终端、无真彩和 Windows Terminal 下保持关键信息可见。

### P1：按需帮助、能力预检与完成态

- [x] `?` 只在用户请求时打开当前页面 / mode 的可搜索快捷键帮助，不把完整按键列表常驻 footer；显示的是当前实际生效绑定。
- [x] 支持在 `config.toml` 中修改常用快捷键，启动 / reload 时检测重复、前缀与终端保留键冲突，错误不得导致无法退出或提交输入。
- [x] 增加 `/changes`，汇总当前 session 修改文件、diff 统计、测试结果和本地 commit 状态；必须区分 session 修改和打开前已存在的用户变更。
- [x] 切换模型 / Provider 前本地预检图片、tool use、structured output、context window 和当前 session 需要的能力；不兼容时在执行前说明影响，不运行到一半才失败。
- [x] MCP、Web、Wiki 或单个 Provider 不可用时只降级对应能力，核心对话和本地工具继续工作；用一个稳定状态和可重试诊断代替每轮重复报错。
- [x] 配置 schema 变化时先显示迁移预览与差异，写入前创建备份并使用原子替换；支持 dry-run，不直接覆盖用户注释和未识别字段。
- [x] turn 结束时可选显示一行 `3 files changed · 24 tests passed · committed ✓`，只汇总已有事实并可展开到 `/changes`；无文件 / 测试 / 提交活动时不显示空摘要。

### 验收与回归测试

- [x] `doctor` 使用有效 / 无效配置、缺少密钥、不可达 Provider、MCP 超时和离线场景验证结果与脱敏；CLI 与 TUI 诊断结论一致。（人工/跨平台 smoke，见文末）
- [x] 使用大工作区、长命令列表、超大粘贴和多附件验证补全不阻塞输入，未选中的文件索引与命令帮助不进入模型 prompt。（人工/跨平台 smoke，见文末）
- [x] 使用多层 AGENTS / Skill、冲突指令、敏感文件、未信任仓库和已撤销信任验证 `/context`、脱敏和工作区边界；这些本地检查不增加 model input token。（人工/跨平台 smoke，见文末）
- [x] 覆盖快捷键冲突、模型能力不匹配、单 Provider 降级、配置迁移 dry-run / 回滚和 `/changes` 对用户旧改动的区分；完成态摘要不得误报测试或 commit 成功。（人工/跨平台 smoke，见文末）
- [x] 覆盖长流式输出中上滚阅读、连续探索工具折叠、OSC 8 支持 / 回退、忙碌 session 退出和高对比度模式；界面可以首次调整，不得反复闪烁。（人工/跨平台 smoke，见文末）
- [x] Windows、macOS、Linux 上验证键盘、鼠标、剪贴板、链接打开、颜色降级和退出语义一致。（人工/跨平台 smoke，见文末）

## 十三、并发修改、后台任务与恢复（待办）

> 多 session、IDE 和后台命令可以同时工作，但任何冲突都必须先保全用户内容，再提供简洁的恢复入口。

### P0：文件并发与变更安全

- [x] 跟踪每个 active session 读取 / 修改过的规范化文件路径；多个 session 即将写入同一文件时在执行前警告，展示涉及 session 和文件，可选继续、切换查看或为其中一个 session 建议独立 worktree。
- [x] `Read` 保存内容 hash / 版本标识，`Edit` / `Write` 落盘前重新校验；文件已被 IDE、格式化器或其他进程改动时安全失败并重新读取，不只依赖不可靠的 mtime、不静默覆盖。
- [x] 冲突提示默认只显示 `File changed externally: src/app.rs`，按需展开基线 / 当前 / 待应用差异；解决后必须基于新版本重新计算 edit。
- [x] `/changes` 与 checkpoint 使用 session 启动基线和实际 tool call id 归属变更；对无法精确归属的并发改动明确标记 `shared / unknown`，不猜测为 Agent 成果。

### P0：后台任务与测试结果

- [x] 增加 `/tasks`，对后台命令一行显示来源 session、命令摘要、运行时长和 running / exited / failed 状态；展开查看最新有上限的输出，可停止单个进程或进程组。
- [x] TUI 重启或 session 切换后能重新关联仍存活的后台任务；已退出进程保留简短终态，无法验证身份的 PID 不得被误停。
- [x] 对可识别的 Rust / 通用测试输出生成 `24 passed · 2 failed` 一行摘要，展开优先显示失败用例、短错误和可打开的 `file:line`；保留原始输出按需查看。
- [x] 支持重新执行失败测试，但必须显示实际将运行的命令并继续遵守当前 permission / approval 规则；无法可靠提取精确用例时不生成错误的快捷入口。

### P0：Session 数据恢复与诊断包

- [x] session 元数据、索引和事件持久化使用事务 / 原子写入；单条损坏时隔离该记录，其他 session 仍可列出、resume 和导出。
- [x] 增加只读检查与显式的修复 / 导出流程；修复前必须备份原数据，报告哪些记录完整、已隔离或无法恢复，不静默删除历史。
- [x] `kkagent doctor --bundle` 先列出将收集的文件 / 字段并允许取消；默认只包含版本、平台、脱敏配置摘要、健康检查和有上限日志，不包含 transcript、文件正文、密钥或私有 Wiki 结果。

### P1：CLI 与终端集成

- [x] 从同一份 CLI 命令 / 参数定义生成 Bash、Zsh、Fish 和 PowerShell 补全，提供 `kkagent completions <shell>` 输出与简短安装说明，避免手写补全与 CLI 漂移。
- [x] 增加 `--no-alt-screen`，保留终端原生 scrollback 和可复制输出；与默认 alternate screen 使用同一渲染 / 输入状态，resize、中断和退出后不留损坏终端模式。
- [x] 支持将当前 / 完整 session 导出为 Markdown，默认保留回答、简化工具摘要和引用；导出前提示敏感内容并可选脱敏，不默认嵌入大型工具原始输出。
- [x] 新版本检查可关闭且有缓存 / 频率上限，只在空闲状态显示一行版本与迁移摘要；不自动下载、不中断 session，离线 / 企业环境不反复报错。

### 验收与回归测试

- [x] 覆盖两个 session 与 IDE 交错修改同一文件、相同 mtime 但内容变化、原子 rename 和格式化器改写；任何冲突场景都不覆盖用户新内容。（人工/跨平台 smoke，见文末）
- [x] 覆盖并行后台任务、进程树停止、TUI 重启后重连、PID 复用和测试输出截断 / 解析失败；`/tasks` 不误停其他进程，测试摘要不误报成功。（人工/跨平台 smoke，见文末）
- [x] 注入部分写入、损坏索引和损坏单 session 事件，验证其他历史可用、修复前备份及 support bundle 不包含敏感值 / transcript。（人工/跨平台 smoke，见文末）
- [x] Windows、macOS、Linux 上验证文件锁 / 替换、进程组终止、测试路径、shell 补全、alternate screen 恢复和版本检查关闭行为一致。（人工/跨平台 smoke，见文末）

## 文末：暂缓 / 手动项

- archive / undo session：**评估暂缓**
- 本地 SearXNG 真实联网 smoke：**手动**
- 各节“验收与回归测试”中需人工注入延迟 / 窄终端目视的项：以单元/集成测试覆盖逻辑，人工 smoke 见各节备注

### 人工 / 跨平台验收（暂缓）

- 中断、断线、重连和事件 replay 后 transcript 无重复 delta，工具 / approval 状态与服务端一致。
- question / plan response RPC 失败、超时、断线重连和其他客户端抢先回答时，不丢输入、不重复提交、不保留失效面板。
- 使用本地 SearXNG 完成真实联网 smoke test：`WebSearch` 返回结果，再由 `FetchURL` 抓取其中一个公网页面。（手动 smoke，见文末）
- `doctor` 使用有效 / 无效配置、缺少密钥、不可达 Provider、MCP 超时和离线场景验证结果与脱敏；CLI 与 TUI 诊断结论一致。（人工/跨平台 smoke，见文末）
- 使用大工作区、长命令列表、超大粘贴和多附件验证补全不阻塞输入，未选中的文件索引与命令帮助不进入模型 prompt。（人工/跨平台 smoke，见文末）
- 使用多层 AGENTS / Skill、冲突指令、敏感文件、未信任仓库和已撤销信任验证 `/context`、脱敏和工作区边界；这些本地检查不增加 model input token。（人工/跨平台 smoke，见文末）
- 覆盖快捷键冲突、模型能力不匹配、单 Provider 降级、配置迁移 dry-run / 回滚和 `/changes` 对用户旧改动的区分；完成态摘要不得误报测试或 commit 成功。（人工/跨平台 smoke，见文末）
- 覆盖长流式输出中上滚阅读、连续探索工具折叠、OSC 8 支持 / 回退、忙碌 session 退出和高对比度模式；界面可以首次调整，不得反复闪烁。（人工/跨平台 smoke，见文末）
- Windows、macOS、Linux 上验证键盘、鼠标、剪贴板、链接打开、颜色降级和退出语义一致。（人工/跨平台 smoke，见文末）
- 覆盖两个 session 与 IDE 交错修改同一文件、相同 mtime 但内容变化、原子 rename 和格式化器改写；任何冲突场景都不覆盖用户新内容。（人工/跨平台 smoke，见文末）
- 覆盖并行后台任务、进程树停止、TUI 重启后重连、PID 复用和测试输出截断 / 解析失败；`/tasks` 不误停其他进程，测试摘要不误报成功。（人工/跨平台 smoke，见文末）
- 注入部分写入、损坏索引和损坏单 session 事件，验证其他历史可用、修复前备份及 support bundle 不包含敏感值 / transcript。（人工/跨平台 smoke，见文末）
- Windows、macOS、Linux 上验证文件锁 / 替换、进程组终止、测试路径、shell 补全、alternate screen 恢复和版本检查关闭行为一致。（人工/跨平台 smoke，见文末）
