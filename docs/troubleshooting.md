# 故障排查

## 先收集信息

```bash
kkagent --help
kkagent doctor
kkagent --dump-system-prompt
rustc --version
RUST_LOG=kkagent_core=debug,kkagent_llm=debug \
  kkagent --config ~/.kkagent/config.toml -p "只回复 ok" 2>kkagent-debug.log
```

TUI 问题同时查看 `~/.kkagent/kkagent.log`。分享日志前删除 token、API key、Header、prompt 中的秘密和私有路径。

## 配置与模型

| 现象 | 原因与处理 |
|---|---|
| `Model '...' not found` | `default_model` 与 `[models."..."]` 别名不一致。 |
| Provider 不存在 | `models.*.provider` 没有对应 `[providers.<name>]`。 |
| 配置没生效 | 运行 `kkagent config show` 查看有效值；标准 dotenv 会自动加载，旧 TOML `.env` 需传 `--config .env`。 |
| URL 校验失败 | `base_url` 必须是合法 `http://` 或 `https://` URL。 |
| 401/403 | 检查 API key、OAuth 状态、自定义 Header 和上游 base URL。 |
| 400 / max tokens | 减小 `max_output_size`，校准 context，必要时关闭 thinking。 |
| `first token timeout` | 上游迟迟不吐首字；调大模型/Provider 的 `first_token_timeout_ms`，或设 `0` 禁用；也可配置 `fallback_model`。 |
| 模型不调用工具 | 加入 `capabilities = ["tool_use"]`，并确认上游支持。 |
| 上下文过长 | `/compact`，开启 `auto_compact`，或降低 `compact_keep_last`。 |
| 项目指令/Skill 没生效 | 在工程目录运行 `kkagent --dump-system-prompt` 查看实际合成的系统提示词，确认 `AGENTS.md`、Skill 目录段是否注入。 |

## 工具

| 现象 | 原因与处理 |
|---|---|
| 写文件一直询问 | 当前是 `manual`；批准、添加精确规则或切换 `yolo`。 |
| Write/Edit 被拒绝 | 检查 Plan 模式、工作区边界和敏感文件策略。 |
| Bash 被硬阻断 | 命令被判为破坏性；改成可恢复的明确操作，不能靠 `auto` 绕过。 |
| Bash 超时但还在跑 | 可能已转后台；查看 `/tasks` 并按 ID stop。 |
| Grep 没有结果 | 检查 pattern、glob、工作目录和敏感/忽略路径。 |
| WebSearch 不可用 | 配置 `[services.web_search]`（provider / base_url / api_key_env）。 |
| FetchURL 拒绝地址 | 地址命中 SSRF/私网保护；改用允许的公开端点。 |

## Server 与 SDK

| 现象 | 原因与处理 |
|---|---|
| HTTP `401` | Bearer token 与 Server 启动 token 不一致。 |
| HTTP `403` | token scope 不足，或 fs write / terminal API 未通过启动参数开启。 |
| HTTP `429` | 当前 token 达到每分钟请求限制；降低频率或调整 `--http-rate-limit`。 |
| 连接旧会话 | 检查 `--resume`、`--connect` 和仍运行的 Server。 |
| `server.sock` 连不上 | 确认无进程监听后再删除残留 endpoint 并重启。 |
| POST 后没有最终文本 | POST 只提交任务；连接 WS，并按 `session_id` 等事件。 |
| WS 断线后缺事件 | 事件流不保证重放；重新读取 session/snapshot。 |
| fs/terminal 返回 400 | 路径不可信、cwd 不存在或命令创建失败。 |
| terminal `429` | 删除已完成 terminal，降低并发。 |
| `/ready` 返回 503 | 检查 transcript DB；显式内存降级模式始终不进入 ready。 |

## ACP、MCP、Hooks

| 现象 | 原因与处理 |
|---|---|
| ACP JSON 解析失败 | stdout 被污染；日志只能写 stderr，并确保一行一个 frame。 |
| MCP stdio 启动失败 | 检查 PATH、args、env 和 `timeout_ms`。 |
| 远程 MCP 失败 | 检查 URL、TLS、headers、OAuth scope 和代理。 |
| 没有 MCP 工具 | 查看 `/mcp`，确认初始化和 tools/list 成功。 |
| Hook 没触发 | 检查事件名、JSON、可执行权限和发现路径。 |
| pre-tool 被阻断 | Hook 非零退出或返回 `block: true`；查看日志。 |

仍无法定位时，准备不含密钥的最小配置、最小仓库、完整命令、退出码、平台/架构、相关日志和稳定复现步骤，再判断问题属于配置、Provider、流解析、权限、工具还是前端。
