# Plan: Plan 文件按 Session 隔离

## 问题

1. `Session::new()` 中 plan 文件路径硬编码为 `working_dir/.kkagent/plan.md`，与 session ID 无关。
2. 同一工作目录下多个 session 进入 plan 模式时，会互相覆盖 plan 文件。
3. 文件名 `plan.md` 写死，无法区分不同 session 的计划。

## 方案

将 plan 文件路径改为按 session ID 派生：

```
working_dir / .kkagent / plans / {session_id}.md
```

这是最小改动方案——现有代码已经全部通过 `session.plan_file_path` 动态引用路径，唯一的问题就是 `Session::new()` 里的硬编码。

参考 kimi-code 的做法：`planFilePathFor(id)` → `{plansDir}/{planId}.md`，每个 plan 有独立 ID 和文件。

## 改动清单

### 1. `crates/kkagent-core/src/session.rs` — 核心改动

`Session::new()` 第 59 行：

```rust
// 旧
let plan_file_path = working_dir.join(".kkagent").join("plan.md");

// 新
let plan_file_path = working_dir
    .join(".kkagent")
    .join("plans")
    .join(format!("{}.md", id));
```

`id` 是 `Session::new()` 的第一个参数（session UUID），已经是现成的。

### 2. `crates/kkagent-tui/src/app.rs` — 去掉硬编码路径提示

两处 "Plan mode ON" 系统消息（约第 664 行和第 1144 行），去掉 `.kkagent/plan.md` 硬编码：

```rust
// 旧
"Plan mode ON — explore & write plan only (.kkagent/plan.md). \
 Source edits are denied until you ExitPlanMode."

// 新
"Plan mode ON — explore & write plan only. \
 Source edits are denied until you ExitPlanMode."
```

plan 文件的具体路径已经通过 `PlanFileUpdated` 事件中的 `path` 字段传给 TUI，并在 plan 面板中展示，不需要在系统消息里写死。

### 3. `crates/kkagent-core/src/permission.rs` — 测试路径更新（保持一致性）

测试中使用的 plan 路径从 `.kkagent/plan.md` 改为 `.kkagent/plans/test.md`（约第 348、360、376 行）。这些是测试 fixture，不影响功能，但保持一致避免混淆。

## 为什么不需要改其他地方

以下代码已经动态使用 `session.plan_file_path`，改了 `Session::new()` 后自动生效：

| 位置 | 用法 |
|------|------|
| `session.rs` `plan_mode_reminder()` | 把 `plan_file` 路径写入 LLM system-reminder |
| `session.rs` `messages_for_llm()` | 传 `&session.plan_file_path` 给 reminder |
| `agent_loop.rs` 权限检查 | `perm.evaluate(..., Some(&session.plan_file_path))` |
| `agent_loop.rs` `read_plan_file_if_matched()` | 对比 `session.plan_file_path` |
| `agent_loop.rs` `PlanFileUpdated` 事件 | 用 `session.plan_file_path.display()` 作为 path |
| `main.rs` `session.set_plan_mode` | `create_dir_all(session.plan_file_path.parent())` — 自动创建 `.kkagent/plans/` |
| `main.rs` `sessions.create` | 调 `Session::new(session_id, ...)` — 自动派生路径 |
| `main.rs` `session.resume` | 调 `Session::new(session_id, ...)` — 用原 session_id 恢复同一 plan 文件 |

## 风险 / 边界情况

- **旧 plan 文件孤立**：已有的 `.kkagent/plan.md` 不会被自动清理，也不会被使用。影响极小，用户可手动删除。
- **plan 文件积累**：`.kkagent/plans/` 下会按 session 积累 `.md` 文件。后续可考虑加清理逻辑（如 session 归档时删 plan），本次不做。
- **session resume 一致性**：resume 时 `Session::new()` 用的是原 session_id，所以 plan 文件路径不变——同一 session 恢复后继续用同一个 plan 文件，符合预期。
- **UUID 文件名较长**：`{uuid}.md` 文件名比较长（36 字符）。如果觉得不好看，可以用前 8 位，但前缀有极小碰撞风险。建议用完整 UUID 保证唯一性。

## 验证方式

1. **多 session 隔离**：在同一目录启动两个 TUI session，都进入 plan 模式，确认各自写入不同的 `.kkagent/plans/{session_id}.md`。
2. **Resume 一致性**：创建 session → 进 plan 模式 → 写 plan → 退出 → resume 该 session → 进 plan 模式 → 确认 plan 文件路径与之前一致。
3. **现有测试**：`cargo test -p kkagent-core` 通过（更新测试路径后）。
4. **编译**：`cargo build` 通过。
