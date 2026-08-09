# 安全说明

kkagent 能读取和修改文件、运行 Shell、访问网络和调用第三方工具，安全等级等同于运行它的操作系统用户。

## 推荐基线

- 首次进入仓库使用 `manual`，检查 `AGENTS.md`、`.kkagent/`、Hooks 和 MCP 配置。
- 只在可信工作区启用项目 Skill/Hook；审查来自外部仓库的指令文件。
- 使用专用低权限账户、容器或虚拟机处理不可信代码。
- API 密钥放在用户配置或环境变量中，不写入仓库。
- Server 默认只绑定 `127.0.0.1`，token 使用高熵随机值并定期轮换。
- 对生产目录、SSH key、云凭据和包发布凭据使用操作系统权限隔离。

## 权限与沙箱是两层控制

`manual`、`yolo`、`auto` 决定工具是否需要交互批准；路径策略和危险命令检测阻止一部分常见误操作。Bash 默认再启用 `[sandbox] mode = "auto"`：Linux/macOS 使用工作区级 OS sandbox，Windows 使用 Job Object 进程隔离。该层只覆盖 Bash 工具进程，不自动包裹 MCP、Hook 或显式开启的 HTTP terminal；合法编译器和脚本仍可能利用允许的工作区或网络能力。处理多租户或恶意代码时仍应叠加低权限账户、容器或 VM。

## Server 暴露

主 HTTP token 是 admin。部署时优先给调用方发 read/write/terminal scoped token，并保持直接 fs write 和 terminal API 关闭。若监听非 loopback 地址：

- 在前面部署 TLS 反向代理；
- 限制来源 IP 或使用私有网络；
- 不把 token 放进 URL；
- 禁止代理缓存 API 响应；
- 保留并轮转 `~/.kkagent/http-audit.jsonl`；
- 把 `trusted_workspaces` 限定为必要的绝对路径。

WebSocket query token 是兼容机制，可能被访问日志记录；普通 HTTP 和 Node SDK 使用 Bearer Header。terminal API 即使有 token 也必须通过 `--allow-terminal-api` 显式打开。

## 凭据和敏感文件

Kimi OAuth 凭据原子写入 `~/.kkagent/credentials/kimi-code.json`，Unix 权限设为 `0600`。文件工具会限制常见 `.env`、私钥和凭据路径，但不要依赖名称检测保护秘密。最可靠方式是让 Agent 用户无法读取不需要的凭据。

配置中的 `custom_headers`、MCP `headers` 和 API key 都属于秘密。分享日志、配置或 bug report 前应人工脱敏。

## 网络和扩展

`FetchURL` 有 SSRF 防护，仍应在网络层阻止访问云 metadata 和内网管理接口。MCP Server、Hook 和插件提示均跨越信任边界：stdio MCP 与 Hook 是本机进程，远程 MCP 能接收工具参数，Skill 和插件能影响模型行为。

## 遥测

云遥测默认关闭。启用前检查 `KKAGENT_TELEMETRY_ENDPOINT` 和组织政策。运行时会清洗部分属性并把失败事件 spill 到 `~/.kkagent/telemetry/`，但不应在 prompt、标题或自定义字段中主动放入秘密。

## 漏洞报告

安全问题不应附带真实 token、私钥、用户会话数据库或含敏感源码的完整日志。报告时提供最小复现、版本、平台、配置结构（脱敏）和预期/实际权限决策。
