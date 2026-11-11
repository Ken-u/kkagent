# 运维指南

## 数据目录

| 路径 | 内容 |
|---|---|
| `~/.kkagent/config.toml` | 默认配置。 |
| `~/.kkagent/transcripts.db` | 会话、事件、持久 turn 队列和后台 Agent 任务 SQLite。 |
| `~/.kkagent/session_index.jsonl` | 会话索引兼容记录。 |
| `~/.kkagent/sessions/` | 按 workspace 分桶的 session journal 和元数据。 |
| `~/.kkagent/credentials/kimi-code.json` | Kimi OAuth 凭据。 |
| `~/.kkagent/kkagent.log` | TUI 文件日志。 |
| `~/.kkagent/server.sock` | Unix socket，或 Windows 本地 endpoint 文件。 |
| `~/.kkagent/active-sessions/` | 每个工作目录一份 TUI 后台化时保存的 session id，供在该目录下次自动 resume；删除/丢弃 session 时对应标记同步清除。 |
| `~/.kkagent/active-session` | 最近一次 TUI 后台化时保存的 session id（全局兼容标记，无按目录标记时作为回退）。 |
| `~/.kkagent/config.toml.trust.toml` | TUI 管理的 workspace、外部 Git 元数据和只读全局 Git 配置授权；自定义 `--config` 使用同目录同名 sidecar。 |
| `~/.kkagent/skills/`、`plugins/`、`hooks.json` | 用户扩展。 |
| `~/.kkagent/telemetry/events.jsonl` | 本地遥测事件。 |
| `~/.kkagent/http-audit.jsonl` | HTTP 请求审计日志，不记录原始 token。 |
| `~/.kkagent/sessions/<workDirKey>/<sessionId>/agents/main/plans/` | Session 计划，文件名为 `YYYY-MM-DD_<plan-name>.md`。 |
| `~/.kkagent/tool-results/<session_id>/` | 被外置保存的超大工具结果，按 session 分桶；文件名含 tool 名、tool call id 和 UUID，并在 `transcripts.db` 记录映射。subagent 的外置结果写入父 session 桶（无 DB 行）。 |
| `~/.kkagent/trash/<session_id>.jsonl` | 删除 session 时的回收站归档：完整导出 session 行、全部消息和每个超大工具结果内容，仅供离线分析；运行时不读取。 |

## 日志

TUI 为避免破坏界面，将日志写到 `~/.kkagent/kkagent.log`。非交互和 Server 模式主要写 stderr。通过 `RUST_LOG` 调整：

```bash
RUST_LOG=kkagent_core=info,kkagent_llm=debug kkagent -p "health check"
```

debug 日志可能包含上游错误和工具参数，收集后应脱敏。长期运行 Server 应由 systemd、launchd、Windows Service wrapper 或容器平台接管 stderr、重启和日志轮转。

## 健康检查

```bash
curl --fail --silent \
  -H "Authorization: Bearer $KKAGENT_HTTP_TOKEN" \
  http://127.0.0.1:8787/api/v1/ready
```

`health` 是存活探针，`ready` 会检查持久化并在显式内存降级时返回 503。`metrics` 输出 Prometheus 文本。完整端到端检查还应创建临时 Session、向低成本模型发送短 prompt，并确认事件序号连续；避免高频执行实网 LLM 检查。

Transcript DB 默认打不开时进程启动失败，防止静默丢会话，错误信息会列出处理办法。数据库位置可用 `[storage] transcripts_path` 或 `KKAGENT_TRANSCRIPTS_DB` 指定：SQLite 打开前会读取路径所有上级目录的元数据，严格的 `sandbox-exec` 配置不放行这些目录时会报 `unable to open database file`，把数据库放到进程可到达的位置即可。只有明确接受非持久化运行时才设置 `[storage] allow_in_memory = true`（或 `KKAGENT_ALLOW_IN_MEMORY_TRANSCRIPTS=1`）；此时 health 会标记 `durable=false`，ready 保持 503。

独立 Server 重启后会把未完成 turn 和后台 Agent 标记为恢复态并重新领取；每项最多执行三次。客户端提交消息时应带 `Idempotency-Key`，网络超时后使用同一 key 和相同 body 重试，避免创建第二项任务。审批默认 15 分钟超时（`[background] approval_timeout_s`，`0` = 不限时），重启时处于待审批状态的 turn 会从已持久化 transcript 重新运行，而不会自动沿用旧批准；但审批 ID 在同一持久化任务内是稳定的，恢复后同一任务再次请求同一个审批时 ID 不变，宿主已经给出的答复仍然可用。创建会话也支持 `Idempotency-Key`，`GET /api/v1/meta` 的 `instance_id` / `boot_id` 用来区分“同一个 server 重启”和“换了 server”。HTTP 任务的最终状态以 `task_finished` 事件为准，`turn_start` / `turn_end` 只是模型步骤边界（见 [Server API](server-api.md)）。

## 备份与恢复

停止正在写入的 kkagent 进程后，备份 `transcripts.db`、`sessions/` 和需要保留的配置/扩展。不要把 credentials 一并放入普通备份；如必须备份，应加密并限制权限。

恢复时保持目录相对结构，并让文件归目标用户所有。SQLite 正在运行时优先使用 SQLite 在线备份机制，避免只复制主文件而漏掉 WAL 状态。

Linux 生产主机需安装 Bubblewrap 并允许非特权 user namespace；否则 `sandbox.mode = "workspace"` 会 fail-closed。macOS 使用 `/usr/bin/sandbox-exec`；Windows 默认使用 Job Object 的 `process` 模式，若需要文件系统级多租户隔离，应在 Windows Sandbox、WDAG 或专用 VM 中运行 kkagent。

## 升级

1. 备份会话数据和配置。
2. 阅读 release notes 和配置变化。
3. 构建新二进制并先执行 `cargo test --workspace --all-targets`。
4. 停止旧 Server，替换二进制，再启动新进程。
5. 检查 `/api/v1/meta`、模型列表、MCP 和一次只读 prompt。

不要让新旧版本同时写同一个 endpoint 或同一 Session。残留 `server.sock` 只应在确认没有 Server 进程持有后移除。

## Standalone Server 生命周期

默认 `kk` 会自动启动或连接独立 server（`~/.kkagent/server.sock`）。TUI 退出（含 `Ctrl+B` 后台化）不会停止 server；正在执行的 agent turn 继续跑，结果留在 server 内存与 transcript 中。

| 命令 | 作用 |
|---|---|
| `kkagent server` | 前台运行独立 server（也可由 TUI 自动后台拉起）。 |
| `kkagent server status` | 人类可读状态；退出码 0=存活，1=未运行。 |
| `kkagent server status --json` | JSON：`alive`、`active_turns`、`client_count`、`uptime_secs`、`pid`。 |
| `kkagent server stop` | 优雅停止；有 active turn 时拒绝。 |

空闲策略见配置 `[server].idle_timeout_secs`（默认 1800 秒；`0` 表示永不自动退出）。进行中的 turn、待决审批/提问、运行中的后台任务默认都会阻止空闲退出（`idle_hold_for_turns` / `idle_hold_for_approvals` / `idle_hold_for_background_tasks`），连接中的本地客户端、打开的 HTTP 事件流与每个 HTTP 请求也都算活动。`./upload.sh` 在本地 CI 前会检测并停止无 turn 的 server；有 turn 时会报错退出。

## 容量与清理

Session journal、SQLite transcript、日志、telemetry 和 tool-results 会随使用增长。当前没有统一保留策略；应按组织要求定期归档或删除旧会话。删除 session 会把数据完整导出到 `~/.kkagent/trash/<session_id>.jsonl`（含超大工具结果全文）后从 DB 与磁盘清除，之后不再出现在任何列表里；trash 目录可按组织要求定期清理或归档。避免在进程活跃时手工编辑数据库。

HTTP terminal 最大 64 个，单个 stdout/stderr 各约 1 MiB；Agent 后台任务默认并发 4，Bash 内部还有运行数量和最长存活时间限制。资源不足时先检查 `/api/v1/tasks`、`/api/v1/terminals` 和进程树。
