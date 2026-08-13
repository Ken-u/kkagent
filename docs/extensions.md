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
3. 兼容目录 `.agents/skills/` 和 `.kimi/skills/`；
4. 工程根目录 `AGENTS.md` 和 `.kkagent/AGENTS.md` 作为项目指令。

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
