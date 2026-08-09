# kkagent 文档

这套文档以当前仓库实现为准。根目录 `README.md` 负责项目概览，本目录负责可操作的完整说明。

## 使用者

- [安装与快速开始](getting-started.md)：编译、配置、首轮对话、恢复会话。
- [配置参考](configuration.md)：全部 TOML 配置项、环境变量、Provider、Model、MCP、Hooks。
- [CLI 与 TUI](cli-and-tui.md)：命令行参数、快捷键、斜杠命令、非交互模式。
- [工具与权限](tools-and-permissions.md)：内置工具、三种权限模式、Plan 模式和安全边界。
- [故障排查](troubleshooting.md)：配置、模型、工具、Server、MCP 和日志问题。

## 集成与部署

- [Agent Server API](server-api.md)：独立 Server、HTTP、WebSocket、RPC、认证和示例。
- [ACP、MCP、Skills、Hooks 与插件](extensions.md)：编辑器协议和扩展机制。
- [安全说明](security.md)：信任边界、网络暴露、敏感文件、Shell、凭据和遥测。
- [运维指南](operations.md)：数据目录、日志、备份、升级、健康检查和资源限制。
- [Node.js SDK](../sdk/node/README.md)：从 Node.js 控制 Agent Server。
- [VS Code 扩展](../apps/vscode/README.md)：当前实验性编辑器桥接能力。

## 贡献者

- [架构](architecture.md)：crate 分层、请求链路、会话和持久化模型。
- [开发与测试](development.md)：本地开发、测试矩阵、跨平台、提交约定和发布检查。

## 文档约定

- 配置默认位于 `~/.kkagent/config.toml`，也可用全局参数 `--config` 指定。
- 示例中的 `/absolute/path/to/project` 必须换成真实绝对路径。
- HTTP API 示例默认使用 `127.0.0.1:8787`，并通过 `KKAGENT_HTTP_TOKEN` 认证。
- API 仍处于 `v1` 演进期；自动化集成应容忍响应中新增字段。
