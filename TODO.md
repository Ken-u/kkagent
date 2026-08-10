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
