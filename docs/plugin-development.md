# 插件开发指南

面向插件作者的上手与参考文档。机制原理（发现顺序、命名空间、marketplace 安装流程）见
[extensions.md](extensions.md)；本文专注"怎么写、怎么测、怎么发"。

插件 = 一个包含 manifest 的目录 + 一个或多个 MCP server。kkagent 不执行插件代码本身，
只按 manifest 拉起声明的 MCP 进程/连接，把工具、提示词、slash 命令注入会话。因此：

- 插件可以用**任何语言**实现，只要能跑 MCP server（官方 SDK 覆盖 TypeScript/Python/Go/Rust 等）；
- 也可以**零代码**：直接复用社区现成 MCP server（`npx` 一行声明），或只发提示词/slash 命令。

> 本文假设你已了解 MCP server 的基本写法（tools/list、tools/call、JSON Schema 参数）。
> 从零开始请先读 [MCP 官方文档](https://modelcontextprotocol.io) 与对应语言的 SDK；
> 只做 override/slash 命令/提示词则完全不需要 MCP 知识。

## 快速上手（5 分钟）

建一个目录，放入 `kk.plugin.json`：

```json
{
  "name": "my-first-plugin",
  "version": "0.1.0",
  "description": "My first kkagent plugin",
  "mcpServers": {
    "fetch": {
      "command": "uvx",
      "args": ["mcp-server-fetch"]
    }
  }
}
```

在 TUI 里安装本地目录并重载：

```text
/plugins install /absolute/path/to/my-first-plugin
/plugins reload
```

对话里让模型抓取一个网页——它会使用 `mcp__my-first-plugin__fetch` 工具（单 server 插件
命名空间就是插件 id）。`/mcp` 可查看连接状态，`/plugins info my-first-plugin` 看详情。

## Manifest 字段参考

文件名优先级：`kk.plugin.json` > `.kk-plugin/plugin.json` > 旧版 `plugin.json`。

### 顶层字段

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | ✅ | 插件 id。须匹配 `[a-z0-9][a-z0-9_-]{0,63}`（小写字母/数字/`-`/`_`，小写或数字开头，≤64 字符），同时是单 server 插件的工具命名空间 |
| `version` | string | — | 语义化版本（进入 marketplace 时会校验 semver） |
| `description` | string | — | 一句话描述，`/plugins` 列表展示 |
| `systemPrompt` | string | — | 提示词文本。默认**追加**到系统提示词；配合 `replaceSystemPrompt` 变为**替换**。别名 `prompt_append` |
| `replaceSystemPrompt` | bool | — | `true` 时 `systemPrompt` 替换基础 persona（主会话与子代理统一生效）。别名 `replace_prompt` |
| `toolOverrides` | object | — | `{ "内置工具名": "<server>.<tool>" }`，见[覆盖内置工具](#覆盖内置工具) |
| `services` | object | — | 服务后端覆盖（`webSearch`/`webFetch`），见[服务覆盖（免 MCP）](#服务覆盖免-mcp) |
| `slashCommands` | array | — | 命令定义列表，见 [Slash 命令](#slash-命令) |
| `mcpServers` | object | — | 工具服务声明，键为 server 名，见下表 |
| `interface` | object | — | 展示元数据：`displayName`、`shortDescription`、`longDescription`、`developerName`、`websiteURL` |

### `mcpServers.<name>` 字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `type` | string | `stdio`（默认）/ `sse` / `http` / `streamable-http`。别名 `transport` |
| `command` | string | stdio 命令。必须是 PATH 中的命令，或以 `./` 开头且位于插件根目录内 |
| `args` | string[] | 命令参数 |
| `env` | object | 环境变量（如 API key） |
| `cwd` | string | 工作目录，必须 `./` 开头且不能 `..`/符号链接逃逸插件根目录；缺省为插件根目录 |
| `url` | string | 远程 transport 的 endpoint |
| `headers` | object | 远程 transport 的额外 HTTP 头 |
| `oauth` | object | 远程 OAuth 配置 |
| `timeout_ms` | number | 请求超时 |
| `enabled` | bool | `false` 时不连接 |

stdio 进程会收到注入的环境变量 `KKAGENT_HOME`（数据目录）和 `KKAGENT_PLUGIN_ROOT`
（插件根目录），可用于定位自带脚本与资源。

### 命名规则（写给模型看的名字）

- 暴露工具名：单 server 插件 `mcp__<plugin-id>__<tool>`；多 server 插件
  `mcp__<plugin-id>_<server>__<tool>`。命名空间超 32 字符或撞名时追加稳定短哈希。
- 运行时名称（`/mcp`、启停状态、OAuth 凭据）：`plugin-<plugin-id>:<server-name>`，
  与 `config.toml` 中的 MCP server 永不冲突。

## 提示词：追加与替换

- 只写 `systemPrompt`：文本**追加**到系统提示词尾部，多个插件按 id 排序拼接。适合
  "优先使用我的工具"这类行为引导。
- `systemPrompt` + `"replaceSystemPrompt": true`：**整段替换**内置基础 persona，用于
  发行版定制（改掉 "You are kkagent" 开头的整段基础提示词）。workspace 注入
  （AGENTS.md）、skills 段、其他插件的**追加**段仍会叠加在替换后的 persona 之后。
  多个插件同时声明替换时按 id 字典序取第一个，其余记入日志警告。

验证最终效果：在工程目录运行 `kkagent --dump-system-prompt`。

## 覆盖内置工具

把某个内置工具（如 `Web`）替换为本插件 MCP server 的某个工具：

```json
{
  "name": "kk-web-tavily",
  "toolOverrides": { "Web": "tavily.tavily_search" },
  "mcpServers": {
    "tavily": { "command": "npx", "args": ["-y", "tavily-mcp"] }
  }
}
```

行为要点：

- 值格式 `"<server>.<tool>"`，server 必须是本插件 `mcpServers` 的键；tool 取其最后一个
  `.` 后段（工具名本身可含点）。
- 桥接工具会以**内置工具原名**（`Web`）注册，内置实现完全消失（对模型不可见）。
  子代理 profile 按工具名过滤，替换自动对子代理生效。
- 覆盖失败（策略拒绝、server 未连接、被禁用）**自动回退**内置实现并输出日志诊断，
  工具不会消失。
- **权限继承**：权限链按工具名判定，覆盖后的 `Web` 继承内置 `Web` 的只读自动放行
  待遇；同理覆盖高危工具时其原有权限约束仍按原名生效。普通 `mcp__*` 工具（未被
  override 的）默认 fallback-ask——manual 模式每次询问，可用 `[[permission.rules]]`
  加 allow 规则或切 yolo 模式放行。
- **渐进披露与身份一致**：override 采用「身份继承」语义——替换后的工具在 wire 名、
  渐进披露（Inline/Deferred）、只读判定、审批规则上**与被覆盖的内置工具完全一致**，
  只有描述、参数 schema 和执行逻辑来自替换者。模型和权限系统看到的就是原来的工具。

**哪些工具能覆盖**——完整 19 个内置工具的三级清单见
[extensions.md 的策略边界表](extensions.md#插件-override)。摘要：

| 级别 | 工具 | 是否需要用户配置 |
|---|---|---|
| 永不可覆盖 | `AskUserQuestion`、`EnterPlanMode`、`ExitPlanMode`、`Goal` | 无论如何都不行 |
| 默认可覆盖 | `Web`、`TaskOutput`、`Skill`、`Cron`、`ReadMediaFile` | 不需要 |
| 高危需 opt-in | `Bash`、`Edit`、`Write`、`Read`、`Grep`、`Glob`、`TodoList`、`Agent`、`WritePlan`、`SelectTools` | 用户在 `config.toml` 的 `[plugins] extra_overridable_tools` 里显式列出 |

开发提示：如果插件覆盖的是第三级工具，README 里要明确告诉用户需要加哪行配置，否则
override 会静默回退。

## 服务覆盖（免 MCP）

`Web` 这类后端本来就是配置驱动的内置工具——不用 MCP 也能覆盖。manifest 的 `services`
段直接声明服务后端，字段名与 `config.toml` 的 `[services]` 一致（可复制粘贴），启用即
替换用户配置：

```json
{
  "name": "kk-web-brave",
  "version": "1.0.0",
  "description": "Point the built-in Web tool at Brave Search",
  "services": {
    "webSearch": {
      "provider": "brave",
      "base_url": "https://api.search.brave.com/res/v1/web/search",
      "api_key_env": "BRAVE_API_KEY"
    }
  }
}
```

行为要点：

- 覆盖的是**内置 `Web` 工具的后端实现**，不是工具本身——工具名、schema、权限、渐进
  披露全部不变，模型无感知，只换搜索/抓取走的 endpoint。
- 插件声明即**整体替换**用户 `[services.web_search]`/`[services.web_fetch]`（非字段级
  合并）；多插件声明同一服务时按插件 id 字典序取第一个，其余记日志警告。
- `/plugins reload`、enable/disable 后下一 turn 生效，无需重启。
- 不需要任何 MCP server、不拉起进程——相比 `toolOverrides` 走 MCP 的方式，适合
  "只是换个搜索 provider" 这类场景。

provider 取值与 wire 格式要求见 [configuration.md 的 Web 搜索](configuration.md#web-搜索)。

## Slash 命令

```json
"slashCommands": [
  {
    "name": "lookup",
    "description": "Look something up on the web",
    "argumentHint": "<query>",
    "promptTemplate": "Use the Web tool to look up: {{args}}. Summarize the key findings."
  }
]
```

- 用户以 `/plugin:lookup <query>` 触发（补全菜单自动出现）。
- 模板变量：`{{args}}` = 全部参数原文；`{{arg0}}`、`{{arg1}}`… = 按空白切分的第 N 个词。
- 渲染结果作为**普通用户消息**提交给 agent——命令本身不执行任何动作，一切能力来自
  模型 + 工具。
- 旧版纯字符串列表（`"slashCommands": ["foo"]`）仍可解析，但只会出现在补全里，
  执行时提示无模板，新插件应一律用完整定义。

## 本地测试与调试

1. **安装**：`/plugins install /absolute/path/to/plugin-dir`（也接受 ZIP / `file://` /
   GitHub URL）。开发期最方便的是**不放** marketplace 直接装本地目录。
2. **重载**：改完 manifest 或脚本后 `/plugins reload`——重新扫描 manifest、重启 MCP
   连接，后续 turn 生效（无需重启 kkagent）。
3. **排查工具**：
   - `/mcp`：server 连接状态、暴露的工具列表；
   - `/plugins info <id>`：manifest 解析结果、diagnostics、override 状态；
   - `kkagent --dump-system-prompt`：确认提示词追加/替换生效；
   - 日志（见 [operations.md](operations.md)）：override 被跳过时会输出
     `plugin tool override skipped` 与原因。
4. **常见问题**：

| 现象 | 原因 |
|---|---|
| 工具没出现 | stdio 命令不在 PATH；`command` 用了插件外路径；server 连接失败（看 `/mcp`） |
| override 没生效 | 目标在永不可覆盖清单；高危工具未 opt-in；源 server 未连接（日志有 skipped 诊断） |
| `services` 覆盖没生效 | 被字典序更小的插件抢先；插件被禁用；`/plugins reload` 后需下一 turn |
| 提示词没替换 | `replaceSystemPrompt` 忘写；`systemPrompt` 为空；被字典序更小的插件抢先 |
| slash 命令执行提示 unknown | 用了旧版纯名字形式（无模板）；插件被禁用 |
| MCP 名字带哈希后缀 | 命名空间超 32 字符或与其他插件撞名，属正常消歧 |

## 打包与发布

插件通过 marketplace 分发：

1. 打包插件目录为 ZIP（根或一级子目录含 `kk.plugin.json`）；
2. 在 marketplace JSON 中登记条目：

```json
{
  "id": "kk-web-tavily",
  "type": "plugin",
  "tier": "curated",
  "displayName": "Web via Tavily",
  "version": "1.0.0",
  "description": "Replace built-in Web with Tavily search",
  "keywords": ["web", "search"],
  "source": "./kk-web-tavily"
}
```

   本地 marketplace 的 `source` 支持相对目录/ZIP/绝对路径/`file://`；远程 marketplace
   的相对 source 应指向 ZIP，也支持任意 HTTP(S) ZIP 与 GitHub / GitBucket 等兼容 forge
   的仓库 URL（含 `tree/<ref>`、`tree/<ref>/<subdir>`、release tag、commit）。
3. 用户侧：`/plugins marketplace <source>` 添加后安装，或直接
   `/plugins install <github-url>`。

限制：ZIP ≤ 64 MiB、解压后 ≤ 256 MiB / 10000 个文件、拒绝路径逃逸与符号链接。
安装/更新会执行你声明的本机程序——**只安装可信来源**，发布方也应提供可校验的来源。

## 完整示例解剖

两个官方示例分别演示两条覆盖路径：

- [`plugins/official/kk-web-brave`](../plugins/official/kk-web-brave/kk.plugin.json)——
  **免 MCP 的 services 覆盖**：仅 `services.webSearch` 一段，把内置 `Web` 的搜索后端
  指到 Brave，无进程、无工具替换。
- [`plugins/official/kk-web-override`](../plugins/official/kk-web-override/kk.plugin.json)——
  **MCP 工具 override + 全部四种能力**，逐段解释如下：

```json
{
  "name": "kk-web-override",
  "version": "1.0.0",
  "description": "Example override plugin: replaces the built-in Web tool ...",
  "systemPrompt": "When answering questions that need up-to-date information, use the overridden Web tool ...",
  "toolOverrides": { "Web": "web.everything_search" },
  "slashCommands": [
    {
      "name": "lookup",
      "description": "Look something up on the web",
      "argumentHint": "<query>",
      "promptTemplate": "Use the Web tool to look up: {{args}}. Summarize the key findings."
    }
  ],
  "mcpServers": {
    "web": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-everything"] }
  }
}
```

- `systemPrompt`（无 `replaceSystemPrompt`）→ 追加行为引导；
- `toolOverrides` 把 `Web` 绑到本插件 `web` server 的 `everything_search` 工具
  （`Web` 属默认可覆盖清单，用户无需改配置）；
- `slashCommands` 定义 `/plugin:lookup`；
- `mcpServers.web` 用 npx 拉起社区 server——零自带代码。

安装后用 `/plugins info kk-web-override` 可看到 Tool overrides 与 System prompt 状态。
