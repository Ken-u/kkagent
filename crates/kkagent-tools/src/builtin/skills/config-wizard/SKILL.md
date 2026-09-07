---
name: config-wizard
description: 配置向导：引导用户理解并安全修改 kkagent 配置（先问清目标，再给出最小、可验证的修改）。
---

# config-wizard

用户往往说不清要改哪个字段，只描述现象或期望（"模型太慢"、"老弹确认"、"想接个 MCP"）。你的任务是：问清目标 → 看现状 → 给最小修改 → 验证生效。不要一次性大改配置，也不要凭猜测改字段。

本 skill 自带配置速查，用户机器上没有 kkagent 源码仓库，一切以这里的内容和 `kkagent config show` 的实际输出为准。

## 第一步：弄清目标

先确认用户想达成什么，含糊时追问一句。常见诉求对应字段见下方速查表。

## 第二步：看清现状

- 配置文件位置：默认 `~/.kkagent/config.toml`；也可能由启动参数 `--config <path>` 指定。项目级差异只能写在受信任 workspace 的 `<workspace>/.kk/config.toml`（仅白名单段落，见下）。
- 用 Bash 运行 `kkagent config show` 查看有效配置（已应用环境变量覆盖，密钥自动隐藏）；只关心一项时用 `kkagent config get <path>`，例如 `kkagent config get default_model`。
- 环境变量 `KKAGENT_*` 会覆盖部分字段（如 `KKAGENT_DEFAULT_MODEL`、`KKAGENT_PERMISSION_MODE`）；workspace 下的 `.env` 只导入 `KKAGENT_*` 和 `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `KIMI_API_KEY` / `GOOGLE_API_KEY`。

## 第三步：给出最小修改

一次只改与目标直接相关的字段，逐项说明默认值、可选值、影响和回退方式。

## 第四步：落地修改

- 优先 `kkagent config set <path> <value>`（临时文件原子替换，Unix 下 0600 权限）。值按 TOML 语法解析：字符串必须带引号，例如 `kkagent config set default_permission_mode '"manual"'`；布尔写 `true` / `false`。
- 或直接编辑配置文件：改前先读原文件，改后只保留必要 diff，不重排无关内容。
- 首次配置可用 `kkagent init`；想收紧安全基线可用 `kkagent config preset safe`。

## 第五步：验证

改完用 `kkagent config get <path>` 确认生效。启动校验失败时按报错逐条解释并修复，直到通过。常见校验错误：

- `default_model` 或某模型引用不存在：别名拼写错误，或漏配了 `[models."<alias>"]`。
- `provider = "..."` 引用不存在：`[models.*]` 里的 provider 必须已在 `[providers.*]` 声明。
- URL 非法：`base_url` 必须是 `http://` / `https://`。
- 模型别名用了保留符号：`quality` / `balance` / `current` / `fast` 不能作为 `[models]` 别名（含大小写变体）。
- `[subagent.default_models]` 键重复（大小写折叠、`agent` 视为 `general` 后）。

## 安全红线

- 绝不把 api_key / token / secret 回显到对话，也不写入会提交到 Git 的文件；密钥优先用 `api_key_env` 引用环境变量。
- 安全敏感字段要主动说明风险、建议保守默认：`default_permission_mode`（默认 `manual`；`yolo` 跳过全部确认）、`[sandbox]`（`mode = "disabled"` 会关闭全部隔离，仅限已有外层容器/VM 时）、`[plugins].extra_overridable_tools`。
- 不要替用户"顺手"修改与目标无关的配置。

## 配置速查

### 最小可用配置

```toml
default_model = "main"

[providers.main]
type = "anthropic"        # anthropic | kimi | openai | openai-chat | openai-responses | google
api_key_env = "ANTHROPIC_API_KEY"
base_url = "https://api.anthropic.com"

[models.main]
provider = "main"
model = "claude-sonnet-4-5"
max_context_size = 200000
max_output_size = 16384
capabilities = ["tool_use", "thinking", "image_in"]
```

### 常见诉求 → 字段

- 新会话默认模型：`default_model`（必须存在于 `[models]`）。
- 模型档位（子 Agent / 压缩摘要用）：顶层 `quality_model` / `balance_model` / `fast_model` / `fallback_model` / `compaction_model`，都是 `[models]` 里的别名；不配则逐级回退到 `default_model`。
- 单个模型参数：`[models."<alias>"]` 下的 `max_context_size` / `max_output_size` / `capabilities`（`tool_use`、`thinking`、`image_in`、`video_in`、`audio_in`）/ `support_efforts` / `default_effort` / `first_token_timeout_ms`。
- 超时与重试：provider 或 model 级 `first_token_timeout_ms`（默认 60000，`0` 禁用）、`read_timeout_ms`（默认 180s）、`request_timeout_ms`（默认禁用）；`[loop_control]` 的 `max_attempts_per_step`（默认 10）、`rate_limit_retry_base_seconds`（429 退避，默认 5）。
- 老弹确认框：`default_permission_mode` = `manual` / `yolo` / `auto`；细粒度用 `[[permission.rules]]`（`decision` = allow/deny/ask，`pattern` 如 `"Bash(rm *)"`）。
- 减少后台命令超时被杀：`[background]` 的 `bash_task_timeout_s`（默认 120）、`max_running_tasks`。
- 隔离与文件访问：`[sandbox]` 的 `mode`（auto/workspace/process/disabled）、`network`、`extra_read_paths` / `extra_write_paths`（路径必须已存在）；`[tools]` 的 `additional_dirs`、`heavy_dirs` / `extra_heavy_dirs`（Glob/Grep 跳过的重型目录，默认 node_modules/target/.git/out/.repo）。
- 接 MCP：`[mcp_servers.<name>]`，stdio 型配 `type = "stdio"` + `command` + `args`，远程型配 `type = "streamable-http"`（或 `sse` / `http`）+ `url` + `headers`。
- 联网搜索：`[services.web_search]`，`provider` = searxng / brave / custom，配 `base_url`（完整 endpoint）和 `api_key` / `api_key_env`；本地 SearXNG 走 `127.0.0.1` 时默认自动绕过系统代理。
- 生图（启用 `GenerateImage` 工具）：`[services.image_gen]`，配 `base_url` + `api_key_env` + `model`；两者都不配则该工具不注册。
- 子 Agent：`[subagent]` 的 `max_depth`（默认 2）、`max_concurrent`（默认 4）、`[subagent.default_models]`（键 explore/coder/general，值可用真实别名或符号 `quality`/`balance`/`fast`/`current`）。
- 上下文压缩：`[loop_control]` 的 `auto_compact`（默认 true）、`compact_keep_last`、`reserved_context_size`；压缩模型用 `compaction_model`。
- TUI：`[ui]` 的 `check_updates`（默认 true）、主题 `[ui.theme]`；任务完成提醒用环境变量 `KKAGENT_NOTIFY=0` 关闭。
- Hooks：`[[hooks]]`，event 支持 `pre_tool_call` / `post_tool_call` / `session_start` / `session_end` / `turn_start` / `turn_end` / `notification`，配 `matcher`（工具名或 `*`）、`command`、`timeout`。

### 项目级覆盖（`<workspace>/.kk/config.toml`）

仅受信任 workspace 生效，只允许白名单段落：`default_model`（只能引用全局 `[models]` 已声明的别名）和 `[services.image_gen]`。`[providers.*]`、permission、sandbox 不可项目化；出现未知顶层段落会直接报错（防止把安全配置写进项目文件）。文件不存在 = 完全跟随全局。

## 常用命令

- `kkagent config show` / `config get <path>` / `config set <path> <value>`
- `kkagent init`（初始化）、`kkagent config preset safe`（安全预设）
- `kkagent doctor`（排障：环境、工具链、缓存等体检）
