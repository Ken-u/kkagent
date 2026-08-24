# ACP、MCP、Skills、Hooks 与插件

## ACP

`kkagent acp` 在 stdin/stdout 上运行 newline-delimited JSON-RPC（Agent Client Protocol v1），供编辑器和 IDE 启动为子进程：

```bash
kkagent --config ~/.kkagent/config.toml acp
```

客户端先调用 `initialize`，返回官方 `agentCapabilities`（`loadSession`、`promptCapabilities.tools`）与 `authMethods`。随后：

- `session/new`（支持 `cwd` 与可选 `initialMessage`）返回 `sessionId` 与 `modes`；
- `session/load` 按 transcript sessionId 恢复历史会话，并把历史按 `user_message_chunk` / `agent_message_chunk` 回放；
- `session/prompt` 接受官方 content block 数组（text/image 等），turn 期间通过 `session/update` 通知流式推送 `agent_message_chunk`、`agent_thought_chunk`、`tool_call`、`tool_call_update` 等官方变体，turn 结束返回 `{stopReason}`（`end_turn` / `cancelled` / `max_tokens` …）；
- 工具批准映射为 agent→client 请求 `session/request_permission`（options：`allow_once` / `allow_always` / `reject_once` / `reject_always`，kind 为 `edit` / `command` / `fetch`），客户端以 `{outcomeKind: "selected", optionId}` 或 `{outcomeKind: "cancelled"}` 响应；
- 用户提问映射为 `session/request_input`（text 或 select，多选遵循 `multiSelect`），响应为 `{content: [...]}` 或 `{canceled: true}`。

`model/list`、`session/set_mode`、`session/set_model`、`session/cancel`、`fs/*`、`terminal/*`、`mcp/list`、`commands/list` 与 slash commands 保持可用；旧 `approval/*` 接口保留为兼容层。

ACP 进程的 stdout 是协议通道，不能混入普通日志；诊断信息应读取 stderr。当前 [VS Code 扩展](../apps/vscode/README.md) 是一个实现了流式更新与权限/输入请求的 ACP 客户端示例。

## MCP Client

kkagent 可把外部 MCP Server 的工具动态注册给模型。支持：

- stdio 子进程；
- SSE；
- HTTP / Streamable HTTP；
- 远程 OAuth 配置。

配置见[配置参考](configuration.md)。启动后通过 `/mcp` 检查连接和工具。给不可信 MCP Server 的权限应按远程代码执行能力对待：它可以看到传入参数，也可能访问网络或本机文件。

## Skills

Skill 是一个目录中的 `SKILL.md`。发现顺序包括：

1. `~/.kkagent/skills/<name>/SKILL.md`；
2. `.kkagent/skills/<name>/SKILL.md`；
3. 兼容目录 `.agents/skills/` 和 `.kimi/skills/`；
4. 工程根目录 `AGENTS.md` 和 `.kkagent/AGENTS.md` 作为项目指令。

在工程目录运行 `kkagent --dump-system-prompt` 可确认 Skill 目录段和项目指令是否被注入。

最小示例：

```markdown
# rust-review

当用户要求审查 Rust 代码时，先运行 cargo fmt --check 和 cargo clippy，
再按正确性、安全性、跨平台和测试覆盖率输出问题。
```

Skill 可使用简单 frontmatter：

```markdown
---
name: rust-review
description: Review Rust changes before release
version: 1.0.0
triggers: [review, release]
---

按 references/checklist.md 执行检查。
```

Skill 名只能包含 ASCII 字母、数字、`-`、`_`。`SKILL.md` 最大 256 KiB；目录中的资源会被列给模型，`Skill` 工具可用 `resource` 参数读取最大 1 MiB 的 UTF-8 文本资源，并阻止绝对路径、`..` 和符号链接逃逸。每次列出或加载都会重新扫描，因此编辑无需重启。

同名优先级为项目 `.kkagent` > 项目 `.agents` > 项目 `.kimi` > `extra_skill_dirs` > 用户目录。独立 Server 会按每个 Session workspace 单独发现。Skill 不会绕过工具权限；`merge_all_available_skills = true` 会增加初始上下文占用。

## Hooks

除 TOML 配置外，还会发现：

- `~/.kkagent/hooks.json`；
- `<workspace>/.kkagent/hooks.json`。

JSON 示例：

```json
[
  {
    "event": "pre_tool_call",
    "command": "/absolute/path/to/policy-check",
    "args": ["--strict"],
    "timeout_ms": 5000
  }
]
```

Hook 进程工作目录是当前 workspace，并收到 `KKAGENT_HOOK_EVENT` 和 `KKAGENT_HOOK_CONTEXT`。stdout 可返回 JSON：

```json
{"block": true, "reason": "production deploy is disabled"}
```

或返回 `{"rewrite": {...}}` 改写上下文。TOML、用户 JSON 和项目 JSON 会合并；项目 Hook 按 Session workspace 动态读取。`matcher` 支持 `Bash`、`mcp_*` 等模式。`pre_tool_call` 启动失败、非零退出或超时会阻断工具；stdout/stderr 会持续排空并各限制为 64 KiB，timeout 最大 300 秒。Hook 本身是本机可执行代码，项目 Hook 只应在可信仓库中启用。

## 插件

> 面向插件作者的上手指南与 manifest 字段参考见[插件开发指南](plugin-development.md)；
> 本节是机制原理参考。

插件从 `~/.kkagent/plugins/<directory>/` 发现，也兼容
`~/.kkagent/plugins/managed/<id>/` 布局。Manifest 按以下优先级读取：

1. `kk.plugin.json`；
2. `.kk-plugin/plugin.json`；
3. 旧版 kkagent `plugin.json`。

KK plugin 使用 `mcpServers` 字段声明工具服务。最小示例：

```json
{
  "name": "code-search",
  "version": "0.1.0",
  "description": "Remote source search",
  "systemPrompt": "Use CodeSearch to locate remote source before reading local files.",
  "mcpServers": {
    "search": {
      "transport": "stdio",
      "command": "python3",
      "args": ["./scripts/mcp_server.py"],
      "cwd": "./"
    }
  }
}
```

stdio `command` 必须是 PATH 中的命令，或以 `./` 开头、位于插件根目录内的文件；
`cwd` 同样必须以 `./` 开头且不能通过 `..` 或符号链接逃逸插件目录。未填写
`cwd` 时默认使用插件根目录。运行时会注入 `KKAGENT_HOME` 和
`KKAGENT_PLUGIN_ROOT`。

Plugin MCP server 使用 `plugin-<plugin-id>:<server-name>` 作为运行时名称，避免与
`config.toml` 中的 MCP server 冲突。kkagent 启动时自动连接；每个 Agent turn 构建工具
注册表时都会读取当前 MCP 工具集，因此新 session 自动获得插件工具。修改或安装插件后
执行 `/plugins reload`，会重新扫描 manifest、重启 MCP 连接，并让后续 turn 使用新的工具。

暴露给模型的工具名使用缩短的命名空间：单 server 插件为 `mcp__<plugin-id>__<tool>`，
多 server 插件为 `mcp__<plugin-id>_<server-name>__<tool>`（例如
`mcp__rk-codesearch__CodeSearch`）。命名空间超过 32 字符或不同插件产生相同命名空间时，
会追加稳定的短哈希后缀消歧。运行时名称（`/mcp`、`disabled.toml`、OAuth 凭据存储）
不受影响，仍使用完整形式。

旧版 `prompt_append` 仍兼容，等价于 `systemPrompt`。插件 MCP 可通过 `/mcp` 以其运行时
名称启用或禁用，状态保存在 `~/.kkagent/disabled.toml`。插件进程是本机可执行代码，只应
安装可信插件；损坏的 MCP 声明会作为 `/plugins` diagnostics 展示，并且不会阻止其他插件加载。

### 插件 Override

插件可以覆盖内置工具和替换基础系统提示词，用于深度定制（如把 `Web` 换成 Tavily
MCP、发行版自定义 persona）：

```json
{
  "name": "kk-web-tavily",
  "version": "1.0.0",
  "description": "Replace built-in Web with Tavily MCP",
  "systemPrompt": "You are MyAgent...",
  "replaceSystemPrompt": true,
  "toolOverrides": {
    "Web": "tavily.tavily_search"
  },
  "mcpServers": {
    "tavily": { "command": "npx", "args": ["-y", "tavily-mcp"] }
  }
}
```

- **`toolOverrides`**：`内置工具名 → "<server>.<tool>"`（server 必须属于本插件的
  `mcpServers`）。应用时机是工具注册表构建的最后一步：桥接后的 MCP 工具会以内置
  工具的原始名字（如 `Web`）注册，内置实现被完全替换，对模型不可见。子代理 profile
  按名字过滤工具，因此替换自动对子代理生效。替换采用**身份继承**语义：wire 名、渐进
  披露（Inline/Deferred）、只读判定、审批规则与被覆盖工具完全一致，只有描述、参数
  schema 与执行逻辑来自替换者。
- **`services`**：服务后端覆盖（免 MCP）。`webSearch`/`webFetch` 字段与 `config.toml`
  的 `[services.web_search]`/`[services.web_fetch]` 同构，启用即整体替换用户配置。
  适合"只换搜索 provider"这类场景：不换工具实例，schema/权限/披露零变化，不拉起
  任何进程。多插件声明同一服务按插件名字典序取第一个。详见
  [插件开发指南](plugin-development.md#服务覆盖免-mcp)。
- **`replaceSystemPrompt`**：`systemPrompt` 从「追加」变为「替换基础 persona」。
  workspace 注入（AGENTS.md）、skills、其他插件的追加段仍然叠加在后面。主会话与
  子代理统一生效。多个插件声明替换时按插件名字典序取第一个，其余记入日志警告。

**策略边界**（`kkagent-config/src/plugin_policy.rs`）。内置工具共 19 个，全部列出：

**永不可覆盖（4 个）**——权限/计划模式守卫按工具名硬编码，替换会绕过安全检查，即使写入 `extra_overridable_tools` 也会被拒绝：

| 工具 | 原因 |
|---|---|
| `AskUserQuestion` | auto 模式强制 deny 守卫（防止无人值守会话卡在提问） |
| `EnterPlanMode` | 计划模式入口守卫 |
| `ExitPlanMode` | 计划模式审批出口守卫 |
| `Goal` | 目标生命周期与预算守卫 |

**默认可覆盖（5 个）**——低风险/纯数据面工具，manifest 声明即可替换：

| 工具 | 说明 |
|---|---|
| `Web` | web search/fetch（最常见的 override 目标） |
| `TaskOutput` | 后台任务/子代理结果查询 |
| `Skill` | skill 加载 |
| `Cron` | 定时任务管理 |
| `ReadMediaFile` | 媒体文件读取 |

**需配置显式开启（10 个）**——文件写入、命令执行、代理与内部机制类工具，须在 `config.toml` 中 opt-in：

```toml
[plugins]
extra_overridable_tools = ["Bash"]  # 可列多项
```

| 工具 | 风险点 |
|---|---|
| `Bash` | 绕过沙箱/shell AST 安全检查/path policy |
| `Edit` | 绕过路径守卫与文件冲突检测 |
| `Write` | 同上 |
| `Read` | 绕过敏感文件检查与工作区边界（只读，风险较低） |
| `Grep` / `Glob` | 绕过工作区路径边界 |
| `TodoList` | 低风险，但与 TUI 状态展示联动 |
| `Agent` | 子代理启动（权限模式随宿主，但描述/协议可能被替换） |
| `WritePlan` | 计划模式配套（覆盖后计划文件写入流程失效） |
| `SelectTools` | 延迟工具加载机制（覆盖可能破坏 MCP 工具发现） |

MCP 工具（`mcp__*` 命名空间）不属于内置工具：`toolOverrides` 的目标是内置工具名，对
`mcp__*` 名字默认同样被策略拒绝，如确需替换另一插件的 MCP 工具也要走
`extra_overridable_tools` 显式 opt-in。

被策略拒绝或源工具不可用（server 未连接/被禁用）的 override 会**自动回退**到内置实现，
并在日志中输出诊断，不会让工具消失。

### 插件 Slash 命令

`slashCommands` 支持完整命令定义，TUI 中以 `/plugin:<name>` 形式补全和执行：

```json
"slashCommands": [
  {
    "name": "search",
    "description": "Search the web",
    "argumentHint": "<query>",
    "promptTemplate": "Search the web for {{args}} and summarize the top results."
  }
]
```

执行时模板中的 `{{args}}`（全部参数）和 `{{arg0}}`、`{{arg1}}`…（逐词）展开为用户
输入，渲染结果作为普通用户消息提交给 agent。旧版纯命令名列表仍兼容，仅出现在补全中。


### 插件市场

顶层 `plugin_marketplace` 配置默认市场（本地路径、`file://` 或 HTTP(S) URL），
`KKAGENT_PLUGIN_MARKETPLACE_URL` 只覆盖这一项。多个市场用 `plugin_marketplaces`：

```toml
plugin_marketplace = "https://plugins.example.com/marketplace.json"
plugin_marketplaces = [
  "http://10.10.10.205:8091/bjc/kk-plugins",
  { name = "team", source = "/data/kk-plugins/marketplace.json" },
]
```

两者都未设置时，如果 `~/.kkagent/plugins/marketplace.json` 存在则自动使用。
通过 `/plugins` 弹窗添加的市场仍保存在 `~/.kkagent/plugins/marketplaces.json`。
Marketplace JSON 至少包含 `id` 和 `source`：

```json
{
  "version": "1",
  "plugins": [
    {
      "id": "code-search",
      "tier": "curated",
      "displayName": "Code Search",
      "version": "1.2.0",
      "description": "Search remote source indexes",
      "keywords": ["code-search"],
      "source": "./code-search"
    }
  ]
}
```

本地 marketplace 的 `source` 支持相对目录或 ZIP、绝对路径和 `file://`；远程
marketplace 的相对 `source` 应指向 ZIP。也支持普通 HTTP(S) ZIP，以及 GitHub /
GitBucket 等 GitHub 兼容 forge 的仓库、`tree/<ref>`、`tree/<ref>/<subdir>`、
release tag 和 commit URL。多插件单体仓库用 `tree/<branch>/<plugin-dir>` 指向子目录；
安装时下载 `{repo}/archive/<ref>.zip`（GitHub 走 `codeload.github.com`）并只取该子目录。
若 marketplace 配置的是仓库首页（HTML），会自动尝试
`raw/main/marketplace.json` 与 `raw/master/marketplace.json`。安装内容先进入临时目录，
验证 `kk.plugin.json` 后复制到 `~/.kkagent/plugins/managed/<id>/`，再原子更新
`~/.kkagent/plugins/installed.json`；失败时恢复原版本。ZIP 下载限制为 64 MiB、解压后
限制为 256 MiB/10000 个文件，并拒绝路径逃逸与符号链接。

直接执行 `/plugins` 会打开多级管理弹窗：首页可进入已安装插件、插件市场，也可添加
marketplace、从本地目录/ZIP/GitHub 来源安装或重新加载。选择 marketplace 后会先显示
插件列表，再进入插件详情执行安装或更新；已安装插件详情支持启用、禁用、更新和带确认
的移除。通过弹窗添加的 marketplace 会验证后保存到
`~/.kkagent/plugins/marketplaces.json`，下次启动仍然可用。配置文件中的
`plugin_marketplace` / `plugin_marketplaces` 以及环境变量指定的默认 marketplace
也会显示在同一列表中（配置来源不可从弹窗删除）。

```text
/plugins marketplace [source]
/plugins install <marketplace-id-or-source>
/plugins update <id>
/plugins enable <id>
/plugins disable <id>
/plugins remove <id>
/plugins info <id>
/plugins reload
```

这些子命令保留用于脚本和快速操作；不带参数的 `/plugins` 是推荐的交互入口。

`remove` 删除安装记录但保留托管副本，重新安装即可恢复。安装和更新会执行插件声明的
本机程序，因此只能使用可信 marketplace 和插件源。
