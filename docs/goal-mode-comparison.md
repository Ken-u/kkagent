# Goal 模式对比：当前代码 vs ref/kimi-code

> 分析日期：2026-08-19
> 对比对象：当前仓库 `kkagent`（Rust） vs `ref/kimi-code`（Kimi Code CLI 原版，TypeScript）

## 摘要

当前 `kkagent` 已实现 goal 模式的**核心闭环**：goal 的创建/查询/更新/预算设定、状态机（active→paused/blocked/complete）、wall-clock 累计、budget 耗尽阻断、turn 边界自动续跑、active 提醒注入。功能可用，能跑通 goal 自主循环。

但与原版 `ref/kimi-code` 相比，差距集中在**工程健壮性与边界处理**层面：原版有一套庞大而精密的 turn 级状态机（stale-goal 拦截、budget grace turn、outcome continuation、wall-clock 死钟调度器、error-driven pause、fork 处理、goal 队列），当前实现几乎全部缺失或大幅简化。

---

## 1. 文件分布

| 层 | ref/kimi-code (TS) | 当前 kkagent (Rust) |
|---|---|---|
| 协议/模型 | `goalOps.ts`（wire Model + Ops）、`types.ts` | `crates/kkagent-protocol/src/goal.rs` |
| 服务层 | `goalService.ts`（~1330 行 DI 服务） | `goal.rs` 中 `GoalManager`（内存+文件持久化） |
| 工具 | `tools/goal/` 4 个子目录（create/get/update/set-goal-budget） | `crates/kkagent-tools/src/builtin/goal.rs`（单文件 4 个 Tool） |
| 注入 | `injection/goalInjection.ts` + 3 个 `.md` 模板 | `goal.rs::active_reminder()` 内联字符串 |
| 死钟 | `goalDeadlineScheduler.ts`（独立 DI 服务） | 无 |
| 错误码 | `errors.ts`（8 个 goal.* 错误码） | 无独立错误码 |
| outcome | `outcome-prompts.ts` | 无（UpdateGoal 直接 stop_turn） |
| 文档 | `docs/en/guides/goals.md`（含队列、web UI） | 无 goal 专门文档 |

---

## 2. 数据模型对比

### GoalStatus

两边一致：`active / paused / blocked / complete`。当前额外支持 legacy `failed` → `Blocked` 反序列化兼容。

### GoalState 字段

| 字段 | ref | 当前 | 差异 |
|---|---|---|---|
| 标识 | `goalId` | `goal_id` | 一致 |
| 目标文本 | `objective` | `description`（+ `objective` 别名） | 命名不同，当前做了兼容 |
| 完成标准 | `completionCriterion` | `completion_criterion`（+ 别名） | 一致 |
| 状态 | `status` | `status` | 一致 |
| turn 计数 | `turnsUsed` | `turns_used` | 一致 |
| token 计数 | `tokensUsed` | `tokens_used` | 一致 |
| wall-clock 累计 | `wallClockMs` | `wall_clock_ms` | 一致 |
| 活动锚点 | `wallClockResumedAt`（epoch-ms，**持久化**） | `Instant`（内存，不持久化） | ⚠️ 原版崩溃后可恢复累计；当前重启后丢失进行中区间 |
| 预算 | `budgetLimits` | `budget` | 一致 |
| 终止原因 | `terminalReason` | `terminal_reason` | 一致 |
| 时间戳 | — | `created_at` / `updated_at` | 当前额外有 |
| actor 追踪 | `actor`（user/model/runtime/system） | 无 | ⚠️ 原版每个生命周期变更记录发起者 |

### 预算报告

| 维度 | ref | 当前 |
|---|---|---|
| 字段 | token/turn/wallClock 三维，各有 `remaining` + `reached` + `overBudget` 聚合 | 三维各有 `remaining` + 单一 `budget_reached` 布尔 |
| 块原因 | `goalBudgetBlockReason()` 拼出具体哪项预算耗尽 | 统一 "Blocked after goal budget reached" |

---

## 3. 工具对比

四个工具名称一致：`CreateGoal` / `GetGoal` / `UpdateGoal` / `SetGoalBudget`。

### 共同点

- 都返回 goal snapshot + budget report
- `UpdateGoal` 在 complete/blocked 时 stop_turn
- `SetGoalBudget` 支持 unit+value（kimi-aligned）和 legacy 多字段

### 差异

| 能力 | ref | 当前 |
|---|---|---|
| **子 agent 拦截** | ✅ 仅 main agent 注册（`when: agentId === 'main'`），子 agent 调用返回 `goal.unsupported_agent` | ❌ 无此拦截 |
| **CreateGoal 长度校验** | ✅ `MAX_GOAL_OBJECTIVE_LENGTH` / `MAX_GOAL_COMPLETION_CRITERION_LENGTH`，空目标报 `goal.objective_empty` | ❌ 无校验 |
| **CreateGoal goal_start 展示** | ✅ 非 auto 模式生成 `goal_start` 卡片 + 权限模式审批 | ❌ 无 |
| **CreateGoal replace 语义** | 通过独立 `replaceGoal()` | ✅ 有 `replace` 参数 → 内部 `replace_goal()` |
| **UpdateGoal 终态 outcome prompt** | ✅ complete → `buildGoalCompletionSummaryPrompt`；blocked → `buildGoalBlockedReasonPrompt`（带统计 + "Write a concise final message"） | ❌ 直接 stop_turn，无总结提示 |
| **UpdateGoal 状态限制** | 仅 active/complete/blocked | active/complete/blocked/paused + legacy failed |
| **SetGoalBudget 合理性校验** | ✅ 时间 1s–24h 范围校验，拒绝不合理值 | ❌ 无 |
| **SetGoalBudget 已超预算即停** | ✅ 如果新预算已被耗尽 → stop batch | ❌ 无 |
| **Stale-goal 拦截** | ✅ turn 开始记录 goalId，执行 mutation 工具时校验未被替换，否则合成错误结果 | ❌ 无 |

---

## 4. Agent Loop 集成对比

### 4.1 续跑机制

| 维度 | ref | 当前 |
|---|---|---|
| 续跑触发 | `pendingContinuation` + `resumeContinuation`，有 `goalDrivenTurns` 计数 | `should_continue()` + `goal_continuations` 计数（上限 64） |
| 续跑提示 | `GOAL_CONTINUATION_PROMPT` | `GOAL_CONTINUATION_PROMPT`（一致） |
| Step-cap 续跑 | 独立 `GOAL_STEP_CAP_CONTINUATION_PROMPT` | 复用 `GOAL_CONTINUATION_PROMPT`（前缀拼接 step-cap 说明） |
| **outcome continuation** | ✅ 终态 UpdateGoal 后排队一个总结 turn（`goalOutcomeContinuationTurns`） | ❌ 无，终态后直接停 |
| **budget grace turn** | ✅ budget 耗尽但本 turn 有工具调用 → 给一个 grace step 注入 `GOAL_BUDGET_STOP_REMINDER`，下一 step 再停 | ❌ 无 grace，直接 block + finish |

### 4.2 预算执行

| 维度 | ref | 当前 |
|---|---|---|
| turn 边界检查 | ✅ `blockIfBudgetReached()` | ✅ `run_turn_step` 开头检查 `is_budget_exhausted()` |
| **wall-clock 死钟调度器** | ✅ 独立 `IGoalDeadlineScheduler`，active 时 arm 定时器，到期后 cancel 活跃 turn + block | ❌ 无异步死钟，只在 turn 边界检查（长 turn 中 wall-clock 超时无法中断） |
| block 原因粒度 | ✅ 拼出具体 "turn budget X, token budget Y" | ❌ 统一 "Blocked after goal budget reached" |

### 4.3 提醒注入

| 维度 | ref | 当前 |
|---|---|---|
| active 提醒 | ✅ `goal-active-reminder.md`（含 budget guidance、nearing-budget ≥75% 警告、untrusted 转义） | ✅ `active_reminder()` 内联（含 untrusted 标签，**无 nearing-budget guidance**） |
| **paused 提醒** | ✅ `goal-paused-reminder.md`（"不要自主推进，除非用户要求 resume"） | ❌ 无 |
| **blocked 提醒** | ✅ `goal-blocked-reminder.md`（"用 /goal resume 恢复"） | ❌ 无 |
| 重复注入防护 | ✅ | ✅ `session_has_goal_reminder()` 检查 |

### 4.4 错误驱动 pause

| 维度 | ref | 当前 |
|---|---|---|
| rate limit → pause | ✅ `GOAL_RATE_LIMIT_PAUSE_REASON` | ❌ |
| connection error → pause | ✅ | ❌ |
| auth error → pause | ✅ | ❌ |
| API error → pause | ✅ | ❌ |
| model config error → pause | ✅ | ❌ |
| runtime error → pause | ✅ | ❌ |
| **效果** | provider 错误时 goal 自动 pause 而非 block，用户可 resume | provider 错误时 turn 失败，goal 状态不变（可能卡在 active） |

---

## 5. 仅 ref 有的特性

| 特性 | 说明 | 影响 |
|---|---|---|
| **Goal 队列（`/goal next`）** | 可排队多个 goal，当前完成后自动启动下一个；`/goal next manage` 交互式管理 | 当前完全无队列，一次只能一个 goal |
| **fork 处理** | `forkGoal` Op 在 fork session 时清除 goal + `GOAL_FORK_CLEARED_REMINDER` | 当前 fork 时不处理 goal |
| **resume continuation** | `continueIfPaused()` / `continueIfBlocked()` 可启动续跑 turn | 当前 resume 依赖用户输入触发 |
| **telemetry** | goal_created / goal_budget_set / goal_continued / goal_status_changed / goal_cleared | 当前无 goal 专项遥测 |
| **actor 追踪** | 每次生命周期变更记录 user/model/runtime/system | 当前无 |
| **wall-clock 持久化锚点** | `wallClockResumedAt` 写入 wire，崩溃恢复后可补算进行中区间 | 当前用内存 `Instant`，重启丢失 |
| **goal_start 权限审批** | 非 auto 模式下 CreateGoal 展示卡片 + 可切换权限模式 | 当前无 |
| **terminal 状态自动 clear** | complete 时 `clearInternal`（goal 被清除而非保留） | 当前 complete 后 goal 保留在 state 中 |
| **stale-goal 拦截** | turn 内 goal 被替换后，旧的 mutation 工具调用被合成错误拒绝 | 当前无，可能操作到已被替换的 goal |

---

## 6. 当前代码有但 ref 没有的

| 特性 | 说明 |
|---|---|
| `created_at` / `updated_at` 时间戳 | goal 记录创建和更新时间 |
| `GoalOp` journal 枚举 + `AccountUsage` | 显式的 usage 记账 Op（ref 通过 turn 事件隐式记账） |
| JSON 文件原子持久化 | `tmp + rename` 独立文件持久化（ref 通过 wire transcript 持久化） |
| UpdateGoal 支持 `paused` | ref 的 UpdateGoal 只接受 active/complete/blocked |

---

## 7. 优先级建议

按"影响 × 实现成本"排序的改进建议：

### P0 — 健壮性关键缺口

1. **wall-clock 死钟调度器**：长 turn 中 wall-clock 超时无法中断，可能导致 goal 无限运行。建议用 tokio task + timeout 实现。
2. **stale-goal 拦截**：turn 内 goal 被替换（如用户 `/goal replace`）后，旧工具调用可能操作错误 goal。建议 turn 开始记录 goal_id，mutation 工具执行前校验。
3. **error-driven pause**：provider 错误时 goal 应 pause 而非卡在 active，否则 `should_continue()` 会无限重试。

### P1 — 体验改善

4. **budget grace turn**：budget 耗尽时给模型一个 grace step 写总结，而非硬停。
5. **outcome continuation / outcome prompt**：终态后让模型写一段完成/阻塞总结。
6. **block 原因粒度**：拼出具体哪项预算耗尽，便于用户判断。
7. **paused / blocked 提醒**：补齐两种非 active 状态的提醒注入。
8. **nearing-budget guidance**：active 提醒中 ≥75% 时提示模型注意剩余预算。

### P2 — 功能补齐

9. **子 agent 拦截**：禁止子 agent 调用 goal 工具。
10. **CreateGoal 长度校验**：防止超长目标。
11. **SetGoalBudget 合理性校验**：时间范围 1s–24h。
12. **goal 队列（`/goal next`）**：支持排队多个 goal。
13. **fork 处理**：fork session 时清除/迁移 goal。
14. **terminal 状态自动 clear**：complete 后清除 goal。
15. **telemetry**：goal 生命周期事件埋点。

---

## 附：核心文件索引

### 当前代码
- `crates/kkagent-protocol/src/goal.rs` — 模型、GoalManager、提醒常量、GoalOp journal
- `crates/kkagent-tools/src/builtin/goal.rs` — 4 个 Tool 实现
- `crates/kkagent-core/src/agent_loop.rs` — 续跑、budget gate、提醒注入
- `crates/kkagent-tui/src/slash.rs` — `/goal` 命令定义
- `crates/kkagent-tui/src/app.rs` — `/goal` 命令处理

### ref/kimi-code
- `packages/agent-core-v2/src/agent/goal/types.ts` — 类型定义
- `packages/agent-core-v2/src/agent/goal/goalOps.ts` — wire Model + Ops
- `packages/agent-core-v2/src/agent/goal/goalService.ts` — 服务实现（~1330 行）
- `packages/agent-core-v2/src/agent/goal/goalDeadlineScheduler.ts` — 死钟调度器
- `packages/agent-core-v2/src/agent/goal/injection/` — 注入 + 3 个 md 模板
- `packages/agent-core-v2/src/agent/goal/tools/outcome-prompts.ts` — 终态提示
- `packages/agent-core-v2/src/agent/goal/errors.ts` — 错误码
- `packages/agent-core-v2/src/agent/tools/goal/` — 4 个 Tool 实现
- `docs/en/guides/goals.md` — 用户文档
