# KK Wiki Agent

官方示例插件：**internal 型**子 Agent——使用 kk 内部模型与工具，只开放 allowlist
里的工具，并挂载插件私有的 wiki MCP server。

## 为什么这样做

wiki 相关 MCP 工具数量多，直接挂到主会话会常驻占用上下文。本插件把工具放进子
agent 的私有 MCP：**只在委派发生时懒加载启动**，用完即弃，主会话零上下文成本。

## 工作方式

1. 模型调用 `Agent(subagent_type: "kk-wiki-agent.search", prompt: "...")`；
2. kkagent 在进程内起一个标准 agent loop（kk 模型、`PermissionMode::Auto`）；
3. 工具集 = core 工具 ∩ allowlist（`Read`、`Grep`、`wiki_search`）+ 私有 wiki MCP；
4. MCP server 运行时名 `plugin-kk-wiki-agent:wiki`，工具命名空间压缩为
   `wiki_search`（与主会话插件 MCP 规则一致）；
5. 全程事件镜像到父 TUI（Spawned/Started/流式/Completed），与内建子 agent 观感一致。

## 自定义

- `systemPrompt`：子 agent 人设；
- `model`：声明即绑定的模型别名（`default` / `fast` / `current` / `secondary`），
  固定该子 agent 的模型档位，优先于委派时的 `model` 参数；别名以外的值会被忽略
  并记入插件诊断；
- `tools`：allowlist，未列出的工具（含 core/MCP）全部不可用；
- `mcpServers`：换成你自己的 wiki MCP 实现；
- `allowDelegation: true` 可以让它再委派其它子 agent（默认禁止）。
