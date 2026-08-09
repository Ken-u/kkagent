# ACP、MCP、Skills、Hooks 与插件

## ACP

`kkagent acp` 在 stdin/stdout 上运行 newline-delimited JSON-RPC，供编辑器和 IDE 启动为子进程：

```bash
kkagent --config ~/.kkagent/config.toml acp
```

客户端应先调用 `initialize`，再创建会话和发送 prompt。实现暴露会话、模型目录、模式、工具、批准、用户问题、文件系统、终端、MCP 和 slash command 等 capability，并把 Agent 状态映射成 `session/update`、`session/request_permission`、`session/request_input` 等通知。

ACP 进程的 stdout 是协议通道，不能混入普通日志；诊断信息应读取 stderr。当前 [VS Code 扩展](../apps/vscode/README.md) 是最小 ACP 客户端示例。

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
3. 兼容目录 `.kimi/skills/`；
4. 工程根目录 `AGENTS.md` 和 `.kkagent/AGENTS.md` 作为项目指令。

最小示例：

```markdown
# rust-review

当用户要求审查 Rust 代码时，先运行 cargo fmt --check 和 cargo clippy，
再按正确性、安全性、跨平台和测试覆盖率输出问题。
```

Skill 只提供指令和资源，不会自动绕过工具权限。当前 Agent 按需要选择和加载，以减少上下文占用；配置 schema 中的 `extra_skill_dirs` 和 `merge_all_available_skills` 暂未接入发现器。

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

或返回 `{"rewrite": {...}}` 改写上下文。`pre_tool_call` 非零退出可阻断工具；其他失败和超时主要记录警告。TOML Hook 的 `matcher` 当前尚未用于过滤。Hook 本身是本机可执行代码，项目 Hook 只应在可信仓库中启用。

## 插件

插件从 `~/.kkagent/plugins/<directory>/plugin.json` 发现。当前 manifest 是轻量能力声明：

```json
{
  "name": "team-conventions",
  "version": "0.1.0",
  "description": "Team prompt additions",
  "prompt_append": "Always run the repository verification command before completion.",
  "slash_commands": ["team-status"]
}
```

当前插件面主要用于附加 prompt、列出 slash command 声明和展示元数据，不是通用动态 Rust 模块加载器。需要真正调用外部工具时优先使用 MCP，需要流程说明时优先使用 Skill。
