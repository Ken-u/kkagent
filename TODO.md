# kkagent 核心功能与生产就绪清单

> 核对更新（2026-08-09）：以下是当前 Rust coding-agent 核心范围的实现状态；不把示例、占位后端或未经测试的平台写成“已验证”。

## 一、工具集（Tools）

| 工具 | 状态 |
|------|------|
| Read / Write / Edit / Grep / Glob / Bash | **已实现**（轻量 Bash AST + 启发式安全、进程树取消、后台上限） |
| TodoList / Plan / Goal* / SetGoalBudget | **已实现** |
| Task* / Agent* / AskUserQuestion | **已实现** |
| Skill / WebSearch / FetchURL / ReadMediaFile / Cron* / SelectTools / MCP | **已实现** |

## 二、AgentCore

| 能力 | 状态 |
|------|------|
| contextMemory / toolPolicy / swarm / usage / undo / media / scope | **已实现** |
| tokenCounting / compact / hooks / dedupe / blob / plugin / … | **已实现** |
| **Session 子系统**（store/index/workdir-key、metadata、lifecycle、interaction、todo/cron、tool policy、usage、terminal/process、instructions、export；RPC：fork/archive/rename/export） | **已实现并覆盖持久化一致性** |

## 三、TUI

| 能力 | 状态 |
|------|------|
| pi-tui 编辑器原语 + chrome + reverse-rpc + controllers | **已实现** |
| **terminal-image**（Kitty / iTerm2 协议编码） | **已实现** |
| Welcome / footer git 徽章 / 流式光标 / 滚动提示 | **已实现** |
| Ctrl-F 转录搜索 overlay + `/search` | **已实现** |
| Ctrl-G btw 侧栏 + 输入框状态标题 | **已实现** |

## 四、REST / WS / ACP

| 能力 | 状态 |
|------|------|
| HTTP 全路由矩阵（tools/tasks/skills/fs/files/workspaces/config/modelCatalog/search/snapshot/prompts/questions/terminals/connections/export + WS） | **已实现** |
| HTTP ↔ **AgentLoop / ServerState 共享绑定**（`AgentHttpBackend`） | **已实现** |
| ACP：terminal / model-catalog / modes / slash / approval / 事件映射 | **已实现** |

## 五、平台包

| 包 | 状态 |
|------|------|
| **OpenAI Responses API**（`/v1/responses` + catalog 自动选择） | **已实现** |
| **kkagent-oauth**（PKCE / device code / Kimi managed identity / token storage） | **已实现** |
| **kkagent-kaos**（local + SSH via system ssh/scp） | **已实现** |
| **sdk/node**（`@kkagent/sdk` HTTP + JSON-RPC） | **已实现** |
| **apps/vscode**（ACP/HTTP 最小扩展） | **已实现** |
| Bash AST（`bash_ast`，用于安全分析的轻量子集） | **已实现；不宣称完整 shell parser** |

## 六、生产加固

- [x] Provider 严格流终止、并行 tool call 边界与可观测重试
- [x] HTTP 强制认证、受信工作区文件边界、SSRF 防护与终端输出上限
- [x] Shell/Grep 超时、取消、进程树清理、后台任务上限
- [x] Transcript 批量事务、压缩重写、fork/archive/rename 一致性
- [x] Kimi 视频 Files API / 托管账号 OAuth identity（自动刷新、私有原子存储、模型目录配置）
- [x] Linux/macOS/Windows x86_64/arm64 CI 检查矩阵与 Rust 1.88 MSRV
- [x] RPC/HTTP/ACP 共用完整 Turn 工具装配
- [x] per-workspace Skill 热加载、frontmatter、额外目录与受限资源读取
- [x] Hook 配置合并、matcher、workspace 隔离、超时清理与输出上限
- [x] 事件序号、回放窗口、Session 过滤与 turn 状态查询
- [x] HTTP scoped token、限流、审计和危险 API 显式开关
- [x] Transcript fail-closed、health/readiness、Prometheus 指标和 request ID

## 七、验证

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo +1.88.0 check --workspace --all-targets --locked
cargo build --release -p kkagent
kkagent server --http 127.0.0.1:8787
kkagent acp
```
