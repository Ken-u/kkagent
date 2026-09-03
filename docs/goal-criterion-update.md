# Goal 裁判对话与验收口径设计(criterion conversation)

状态:设计稿(未实现,v3 — 对话优先 + 裁决证据账本) · 2026-09

> v3 修订:裁决轮证据机制与裁判会话隔离原则吸收了外部评审(Codex,2026-09)对
> 成本模型、注入面与 overclaim 洗白风险的结论,见"详细设计 §6"与"备选方案 4"。

## 背景与动机

Goal 裁判模式(`[goal] judge_enabled = true`)的裁判目前是**一次性短命智能体**:
模型申报 complete 时临时拉起,裁决完即丢弃(`crates/kkagent-core/src/goal_judge.rs`,
`run_goal_judge` 每次新建 `Session::for_subagent`)。用户与裁判之间没有任何交互通道,
验收口径(`completion_criterion`)也只能在创建 goal 时一次性写入。

实际使用中,验收口径往往要**讨论**才会形成:用户最初只给一句 objective,裁判 reject
之后双方对"什么算完成"有来有回(要跑哪些测试、是否要求 clippy 干净、要不要截图验证)。
因此本设计的目标形态是:**goal 存续期间,用户可以随时和裁判对话讨论验收标准,
讨论的结论沉淀为 criterion,并在后续裁决中由同一裁判人格执行**。

## 现状梳理(实现时以此为准)

| 环节 | 位置 | 现状 |
| --- | --- | --- |
| 裁判生命周期 | `crates/kkagent-core/src/goal_judge.rs:164-233` | 每次 verdict 临时创建 AgentLoop + Session,裁决后丢弃,无历史 |
| 裁判工具 | `goal_judge.rs:174-176` | 仅 `GoalJudge`(标记 approve/reject)+ `Read` |
| 裁判 prompt | `goal_judge.rs:76-138` | objective + criterion(可选拼接,`unwrap_or_default`)+ 最近 40 条证据窗口 |
| fail-open | `goal_judge.rs:9-11, 201-206` | 超时/无 toolcall/报错一律接受自报,裁判永不卡死 goal |
| criterion 模型 | `crates/kkagent-protocol/src/goal.rs:110` | `Option<String>`,`#[serde(default)]`,快照兼容 |
| criterion 写入 | `goal.rs:527-534` | `set_completion_criterion`,仅 `Goal` 工具 create 调用;不写 journal |
| criterion 消费 | `goal.rs:178-204`(worker reminder)、`goal_judge.rs:107-115`(裁判) | 均为运行时现读,`untrusted_` 包裹 + 转义 |
| RPC | `crates/kkagent/src/main.rs:8434-8563` | `session.goal`:status/pause/resume/budget/cancel/replace/create |
| TUI | `crates/kkagent-tui/src/app.rs:10968-11086` | `/goal` 子命令与 RPC action 一一对应 |
| 裁决记录 UI | `crates/kkagent-tui/src/goal_judge_view.rs` | footer goal chip 点击查看历史裁决 |

关键结论:消费端(criterion → 裁判/worker)已经是活的,**讨论结论只要写入
criterion 就立即生效**;缺的是 ① 对话通道本身,② 持久化的裁判会话。

## 目标

1. goal 存续期间,用户可通过 `/goal discuss <message>` 与裁判对话,讨论验收标准;
2. 对话历史持久化,跨 turn / 跨进程重启延续(随 goal 生命周期销毁);
3. 裁判在讨论中可将被双方认可的口径**落盘**(写 `completion_criterion`),立即影响
   后续裁决与 worker reminder;
4. 完成裁决(complete 申报审查)与讨论共用同一裁判**人格**与 criterion 口径——但
   裁决上下文物理隔离(不装载讨论历史全文),避免锚定与角色混淆(见 §1、§6);
5. 裁决的 fail-open 语义、worker 会话隔离不回退;基础设施故障放行时显式记录
   `accepted_unreviewed`,不与裁判明示的 `judge_approved` 混淆。

## 非目标(本期不做)

- 裁判与 worker 的直接对话(worker 仍只通过 reject gaps 文本接收反馈);
- 讨论内容的流式渲染(首版以事件消息整段到达,后续可流式);
- 无 goal 时的裁判对话(必须有活动 goal 才能开聊);
- 结构化逐条口径(`Vec<{text, met}>`)。

## 方案总览

两个组件,共用一个裁判会话:

```
用户 ──/goal discuss <msg>──▶ RPC session.goal{action:"discuss"}
                                   │
                                   ▼
                        裁判会话(持久,goal 生命周期)
                        ├─ 讨论轮:prompt=讨论模式,工具 = Read + GoalCriterion
                        │    └─ GoalCriterion toolcall → set_completion_criterion
                        └─ 裁决轮(worker 申报 complete 时触发,复用现有 gate)
                             ├─ 上下文 = 固定政策/rubric + criterion snapshot + 本轮证据 digest(§6)
                             │    └─ digest = 运行时证据账本 + 受约束摘录(替代 40 条裸窗口)
                             └─ GoalJudge toolcall → approve / reject(+gaps)
```

- 讨论轮由用户消息驱动;裁决轮由现有 `run_goal_judge_gate`(`agent_loop.rs:2348`)驱动,
  **不再每次新建会话**,改为解析/创建会话级持久裁判会话。
- 讨论的产出以 `GoalCriterion` toolcall 落盘;即使裁判忘了落盘,用户仍可用
  `/goal criterion <text>` 手动覆盖(见"手动通道")。

## 详细设计

### 1. 持久裁判会话:`crates/kkagent-core/src/goal_judge.rs`

- 新增 `JudgeConversation`:`Session`(for_subagent)+ 生命周期管理:
  - 创建:首次讨论消息或首次裁决时惰性创建;system prompt 为裁判人格总纲
    ("你是目标完成度裁判,与用户讨论验收标准;讨论轮输出口径修订,裁决轮输出 verdict");
  - 销毁:goal complete/cancel/replace(`goal_id` 变更)时清除——口径与讨论随 goal 消失;
  - 轮次区分原则"**逻辑持久、物理隔离**":讨论历史与人格持久,但每次裁决使用
    **新鲜的 adjudication context**——只装载固定裁判政策、objective、criterion
    snapshot 和本轮 digest,不装载讨论历史与上轮 digest,防止锚定、旧证据误用
    (如"测试通过后又改代码"场景)与讨论中的说服直接进入裁决。实现上每轮新建
    AgentLoop(与现状同构),messages 按轮类型构造;裁决结果(不带原文的
    approve/reject 摘要)回注讨论历史供用户查看。
- 工具:
  - 讨论轮:`ReadTool` + 新 `GoalCriterionTool`(复用 `GoalJudgeTool` 的
    slot+toolcall 模式):入参 `{criterion: String, note: String}` → 调
    `set_completion_criterion`,note 作为给用户的变更说明;
  - 裁决轮:`GoalJudge` + `Read`(现状不变);裁决轮 prompt 中禁止调用 GoalCriterion。
- 上下文膨胀控制:裁决轮不再向持久会话追加原始证据消息(证据经 §6 digest 进入
  独立的裁决上下文,裁决后即弃,只留裁决摘要);讨论超过 N 轮(建议 20)时对最旧
  讨论做一次摘要压缩(复用 full_compaction 的模型解析基础设施,或首版简单截断
  保留最近 N 轮 + 当前 criterion 全文)。
- fail-open 拆分语义:裁决基础设施故障(超时/无 toolcall/报错)仍放行 worker 自报,
  但裁决记录标记 `accepted_unreviewed`(含失败原因),区别于裁判明示的
  `judge_approved`;讨论轮失败仅向用户回报错误,不影响 goal 状态。

### 2. 会话存储

- 裁判会话消息持久化到 goal 快照旁的同级文件(如 `<goal-file>.judge-chat.json`,
  原子写复用 `write_goal_file` 模式);不塞进 `GoalFile`,避免版本迁移;
  `#[serde(default)]` 解析,旧文件缺省为空。
- `completion_criterion` 仍存在 `Goal` 内,随现有快照持久化,无 schema 变更。

### 3. RPC:`crates/kkagent/src/main.rs` `session.goal`

- 新增 `discuss` action:`{session_id, action:"discuss", text}` →
  校验活动 goal → 裁判讨论轮(独立于 worker turn lock 的 judge 锁,防止讨论/裁决并发写同一会话;
  裁决触发时若讨论进行中则等待,超时走 fail-open)→ 回复以
  `AgentEvent::GoalJudgeChat { session_id, text, criterion_updated: bool }` 事件推给 TUI;
- 新增 `criterion` action(手动通道,读/写语义,详见下节);
- headless `/goal discuss ...`:一次性发送 → 等待回复 → 打印 → 退出。

### 4. TUI:快捷键 + 裁判对话窗口(主入口)+ `slash.rs`

**快捷键呼出对话窗口是主交互形态**,`/goal discuss` 降级为兼容入口:

- 默认键 `Ctrl+J`(judge;编辑器 map 与既有应用级 chord 均未占用,`app.rs:3647` 的
  `Ctrl+G` BTW、`app.rs:3376` 的 F5 重绘先例一致,硬编码默认值,暂不接入
  `[ui.keybindings]`——该 override 机制目前只覆盖编辑器动作);
- 窗口为覆盖层,复用 BTW view 的栈式模式(`enter/exit_*_view`):输入焦点交给
  窗口内 composer,`Esc` 关闭并还原焦点;审批/plan-review 弹窗可见时也可呼出
  (与 BTW 同理,裁判对话不触碰 server 侧审批状态,`enter` 隐藏 modal、`exit` 还原);
- 窗口内容三段:顶部当前 goal + 当前口径(最新 `GoalCriterion` note 高亮)、中部
  对话历史(含历史裁决 approve/reject 摘要,即现有 `goal_judge_view` 记录面板内容
  并入此处)、底部 composer;发送即走 `discuss` RPC,回复经 `GoalJudgeChat`
  事件流式追加到中部(等待期 composer 显示 pending 态);
- footer goal chip 点击改为打开同一窗口(原"只读裁决记录弹窗"成为窗口的历史区块;
  `goal_judge_panel_open` 状态路径迁移);
- 无活动 goal 时快捷键呼出显示空态引导(提示先 `/goal <objective>` 或开启
  `judge_enabled`);goal 销毁时窗口自动关闭;
- `/goal discuss <message>` 保留:效果等同在窗口输入框发送(回复仍以 `[judge]`
  前缀消息落入主流;窗口打开时则直接渲染进窗口);
- headless `/goal discuss ...`:一次性发送 → 等待回复 → 打印 → 退出。

### 5. 手动通道(原语,保留)

`set_completion_criterion` 直写通道是讨论机制的地基,独立可用:

- RPC `criterion` action + `/goal criterion` 命令(替换语义;空参读,带参写);
- `Goal` 工具 `update` action 增加可选 `completionCriterion`(模型代写);
- 可选:`set_completion_criterion` 补 `GoalOp::Update` journal 记录(`reason: "criterion_updated"`),
  使口径变更可追溯。

### 6. 裁决轮证据机制:确定性证据账本 + 受约束摘录(替代 40 条裸窗口)

现状(`goal_judge.rs:76-138`)把最近 40 条原始消息(每条截 2000 字符)裸拼进裁判
prompt,三个结构性问题:**成本**——裁判前缀每个 run 独特,跨 run 缓存永不命中,
单次全价 fresh prefill 最多 ~8 万字符;**信噪比**——尾部多为工具调用糊墙,但
tool_result 里的测试输出/退出码恰恰是戳穿 overclaim 的唯一硬证据;**注入**——
evidence 未转义、仅 `<evidence>` 标签包裹,worker 读过的恶意文件内容可直达裁判,
甚至可伪造闭合标签。曾评估"fork 主会话 → 叙事 compact → 裁判只读概要"(已否决,
见备选方案 4)。本节方案:

**(a) 确定性证据账本(事实根,Rust runtime 直出,零 LLM)**
- 从 tool 事件直接生成:命令清单(`argv` 结构化,不重解析 shell 字符串)、执行
  状态(exited / timed_out / cancelled / denied / unknown)、退出码、cwd、时序
  (seq)、输出是否截断、工具失败清单;
- 文件改动列表来自运行时写事件与 VCS delta(标明来源),不从对话抽取;
- 账本天然规避:超时/取消/权限拒绝被误计成功、`cargo test || true` 外层退出码
  为 0、"验证后再次修改文件"导致旧成功失效(记录最后相关变更 seq,供裁判判断
  验证时序是否覆盖最终状态)。

**(b) 受约束摘录 pass(LLM 只选引文,不产生事实)**
- 机制:fork worker 会话渲染消息,**以追加消息形式**挂抽取指令(不换 system
  prompt、不动 tools 定义与顺序——渲染前缀一致才有前缀缓存机会,见 (d));
- 输出仅为对账本所引原始日志的摘录引用,每条引文带 `event_id + 范围`,运行时
  **回验引文确为原始输出的子串**;schema 不合法或回验失败重试一次;
- 逐字引用,禁止转述;预算 1-2k token 为软目标,超预算时优先保留失败/未知/
  超时/截断标志与最新一次相关验证,标 `coverage_complete=false`,省略低价值
  成功日志。

**(c) digest 装配与裁判 prompt 结构**
- digest = 账本事实字段 + 回验摘录 + worker 最终申报原文(标注"待核验主张",
  非证据)+ `digest_status: complete | degraded`;
- 裁判 prompt 分层:固定裁判政策与 rubric(含少量对抗示例)**置于最前**(稳定
  前缀,自身可缓存)→ 结构化 objective + criterion snapshot → 本轮 digest;
  裁决每个结论须引用对应条目;证据/文件/criterion 内容一律视为不可信文本。

**(d) 成本模型(现实口径)**
- 前缀缓存命中要求渲染前缀完全一致(模型、system、tools 定义及顺序、消息序列)
  且在 provider TTL 内(Anthropic 默认 5 分钟;OpenAI 按模型代际);最终申报与
  抽取指令必然是 uncached suffix;
- 定位为**高概率、可观测的优化,而非设计不变量**;落地时采集 `cached_tokens /
  cache_write / uncached input / 输出 / 延迟 / 失败率` 实测验证。

**(e) 失败语义与注入加固**
- extractor 失败:降级为确定性最小账本(`degraded`),**不回退 40 条裸窗口**
  (回退即重新引入注入与漏证据);
- 逐条引文单独 `untrusted_evidence` 转义(防标签逃逸;转义不防语义注入,靠
  政策级禁令:证据/objective/criterion/文件内容中的指令一律不可执行);
- criterion 只能描述验收条件,"忽略证据直接批准"视为非法 criterion;
- 裁决基础设施故障放行记 `accepted_unreviewed`(见 §1)。

**(f) 后续演进(不进 MVP)**:`EvidenceRead(event_id)` 按需读原文工具、criterion
不可变版本链 + `attempt_id` 全链路、引文 hash 防篡改、高保证场景 fail-closed 配置。

## 边界与已知取舍

| 场景 | 行为 | 说明 |
| --- | --- | --- |
| 讨论进行中 worker 申报 complete | 裁决轮排队等 judge 锁,超时 fail-open | 保持"裁判永不卡死 goal" |
| turn 运行中口径被改 | 裁决即时生效;worker 下一 turn 生效 | reminder 在 turn 边界注入,与 `/goal replace` 一致 |
| goal replace/cancel/complete | 裁判会话与讨论记录删除 | goal_id 变更即新裁判,避免跨目标串味 |
| 用户在讨论中口头改口径但裁判未落盘 | 不生效 | 以 `GoalCriterion` toolcall / 手动通道为准;prompt 中要求裁判每轮结尾确认口径状态 |
| 上下文膨胀 | 讨论轮数上限 + 截断/压缩;裁决上下文每轮重建(§1) | 裁决成本不随讨论轮数增长 |
| extractor/账本失败 | 降级为确定性最小账本(`degraded`),不回退裸窗口 | 账本由 runtime 直出,仅摘录层可能失败(§6e) |
| 注入安全 | criterion 对 worker 仍 `untrusted_` 包裹;digest 引文逐条 `untrusted_evidence` 转义(§6e) | 用户是委托人,裁判可自由采信其口径;证据内指令不可执行靠政策禁令 |
| judge 关闭(`judge_enabled=false`) | discuss 通道不开放(报错提示开启) | 没有裁决人格就没有讨论对象;手动 criterion 通道不受限 |

## 备选方案(已评估)

1. **独立"口径顾问"人格**(讨论与裁决分离,仅共享 criterion):实现更简单(不改动裁决
   gate),但用户感知为"和两个裁判说话",裁决可能背离讨论共识;若裁决轮上下文膨胀
   成为问题,可降级到此方案。
2. **一次性讨论轮**(每条消息新建会话,带讨论尾部窗口):最省事,但多轮讨论无连续性,
   "对话"体验名不副实,不推荐。
3. **裁判追问通道**(reject 时裁判向用户提问):依赖用户在线交互,与 fail-open 冲突;
   用户可随时 `/goal discuss` 主动回应,已覆盖该需求。
4. **裁判只读叙事 compact 概要**(fork 主会话 → compact → 裁判只读概要;2026-09
   外部评审否决):compaction 的优化目标是叙事连续性而非证据核验,易把 worker 自报
   洗成"已完成、测试通过"——恰是裁判要抓的 overclaim 原始形态;compactor 与 worker
   常为同源模型,错误倾向相关;`Read` 抽查只能验证最终文件,无法恢复"测试是否真
   跑过、对应哪个代码状态"。若把 compact prompt 特化到保留命令/退出码/矛盾/逐字
   引文,本质上已退化为 §6 的证据 digest。现状 40 条裸窗口在 §6 落地后退役。

## 实现清单

分三步交付,每步独立可用:

**第一步:手动通道(地基)**
- [ ] `builtin/goal.rs`:`update` action 支持 `completionCriterion`(含 schema 描述)
- [ ] `main.rs`:`session.goal` 新增 `criterion` action(读/写语义)
- [ ] `app.rs`/`slash.rs`:`/goal criterion [text]`;headless driver 透传
- [ ] 可选:`set_completion_criterion` 补 journal op

**第二步:裁判对话**
- [ ] `protocol/goal.rs`:`GoalJudgeChat` 事件(judge 回复 + criterion_updated 标记)
- [ ] `core/goal_judge.rs`:`JudgeConversation` 持久会话 + 讨论轮 prompt/工具 +
      `GoalCriterionTool` + 裁决轮改造(复用会话;裁决上下文物理隔离,§1)
- [ ] 会话持久化文件与生命周期(随 goal 销毁)
- [ ] `main.rs`:`discuss` action + judge 轮锁 + 事件发布
- [ ] `app.rs`:`Ctrl+J` 呼出裁判对话窗口(覆盖层,复用 BTW 栈式模式)+
      key handler + `GoalJudgeChat` 事件渲染;goal chip 点击改开同一窗口,
      迁移 `goal_judge_panel_open` 状态路径
- [ ] `slash.rs`:`/goal discuss [message]`;headless 一次性模式
- [ ] 测试:讨论轮落盘口径、锁排队、goal replace 后会话重建、裁决上下文不含
      讨论历史(隔离);TUI 键处理(呼出/关闭/焦点)与空态

**第三步:裁决证据链(§6,独立于前两步可先行/后补)**
- [ ] `core/goal_judge.rs`:runtime 确定性证据账本(命令/状态/cwd/时序/截断/
      写事件文件列表)
- [ ] 摘录 pass:fork worker 渲染消息 + 追加抽取指令(不动 system/tools 前缀)、
      引文 `event_id + 范围` 回验子串、失败重试一次
- [ ] digest 装配(`digest_status`、coverage 标志)+ 裁判 prompt 重排
      (固定 rubric 前置)+ `accepted_unreviewed` 语义拆分;退役 40 条裸窗口
- [ ] 缓存收益遥测(cached/write/uncached/输出/延迟/失败率)
- [ ] 测试:声称测试通过但从未执行、`|| true` 伪成功、验证通过后再改文件、
      超时/取消/权限拒绝、输出截断且失败行在截断区、超 40 条外的早期关键失败、
      恶意闭合标签/伪 toolcall/源码注释注入、extractor 幻觉引文(回验拦截+
      重试)、degraded 账本路径、fail-open 记 `accepted_unreviewed`

## 验证计划

按 LOCAL.md 最小验证约定:每步完成后仅对受影响 crate 执行 `cargo check -p <crate>`
与 targeted test(讨论落盘、会话隔离、fail-open / `accepted_unreviewed`、证据账本
与引文回验);功能经用户确认后统一执行 fmt/clippy/test 全量检查,每步创建一个
逻辑完整的本地提交。
