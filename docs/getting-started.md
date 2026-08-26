# 安装与快速开始

## 环境要求

- Rust 1.88 或更高版本；仓库的 `rust-toolchain.toml` 会选择所需工具链。
- macOS、Linux 或 Windows，支持 x86_64 与 arm64 原生构建。
- 至少一个可用的模型 Provider API，或 Kimi 托管账号。

## 构建和安装

### 预编译安装包

macOS / Linux：

```bash
curl -fsSLO https://raw.githubusercontent.com/Ken-u/kkagent/main/install.sh
sh install.sh
```

安装完成后，后续升级只需执行 `kkagent-update`。

Windows PowerShell：

```powershell
irm https://raw.githubusercontent.com/Ken-u/kkagent/main/install.ps1 -OutFile install.ps1
./install.ps1
```

安装完成后，后续升级只需执行 `kkagent-update.ps1`。

默认从 `Ken-u/kkagent` 的最新 GitHub Release 下载并校验 SHA-256。镜像、fork、固定版本和自定义安装目录见[发布与安装包](releases.md)。

### 从源码构建

```bash
git clone <repository-url> kkagent
cd kkagent
cargo build --release
./target/release/kkagent --help
```

也可以执行 `make release`。`make install` 会写入 `/usr/local/bin`，需要当前用户拥有对应权限。Windows 产物为 `target/release/kkagent.exe`。

## 配置模型

推荐用首次运行向导创建权限为 `0600` 的默认配置：

```bash
kkagent init
kkagent doctor
```

向导支持 OpenAI、Anthropic、Kimi、Google 和兼容 OpenAI Responses 的自定义端点。API key 输入不会回显。CI 中使用：

```bash
kkagent init --provider openai --model gpt-example --preset safe --non-interactive
```

`safe` 禁止工具进程联网；`default` 保留人工批准并允许联网；`full-auto` 使用自动批准。三种预设都保持系统隔离为 `auto`。

### 手工配置

创建默认配置：

```bash
mkdir -p ~/.kkagent
cp examples/config.example.toml ~/.kkagent/config.toml
```

最小配置如下：

```toml
default_model = "local/default"
default_permission_mode = "manual"

[providers.local]
type = "anthropic"
api_key = "sk-..."
base_url = "https://your-anthropic-compatible-endpoint.example"

[models."local/default"]
provider = "local"
model = "upstream-model-id"
max_context_size = 131072
max_output_size = 8192
capabilities = ["tool_use"]
# 推理强度（可选）：配置 default_effort 后无需全局 [thinking] 段即可启用思考。
# support_efforts = ["low", "medium", "high"]  # GPT-5.6: ["none", "low", "medium", "high", "xhigh", "max"]
# default_effort = "medium"
```

`default_model` 是 kkagent 内部别名，必须和 `[models."..."]` 的名称完全一致；`models.*.model` 才是发送给上游的真实模型 ID。

也可以使用 Kimi device-code 登录，让程序写入托管模型配置：

```bash
kkagent auth login
kkagent auth status
```

## 开始使用

进入需要修改的工程目录，然后运行：

```bash
cd /absolute/path/to/project
kkagent
```

建议首次使用保留 `manual` 权限模式。Agent 读取文件通常无需确认，写文件、运行命令等操作会先展示批准请求。确认能力和工作目录无误后，再按需使用 `-y`。

非交互模式适合脚本和冒烟测试：

```bash
kkagent -p "读取 Cargo.toml，总结 workspace 结构"
kkagent -y -p "运行测试并总结失败原因"
```

指定另一份配置：

```bash
kkagent --config /absolute/path/to/config.toml
```

检查配置和依赖：

```bash
kkagent doctor
kkagent doctor --json
kkagent doctor --live   # 额外请求 Provider 的 models 端点
```

## 会话恢复

TUI 中可使用 `/sessions` 和 `/resume <id>`。启动时也可以按完整 ID 或唯一前缀恢复：

```bash
kkagent --resume 2f34a1
```

会话按工作目录组织；恢复前应进入原工程目录。`/new` 创建新会话，`/compact` 压缩过长上下文；`/undo` 或空闲时连按两次 `Esc` 可选择历史提示，从该轮之前分叉并重新编辑，原会话与工作区文件不会回滚。`/undo N` 仍可执行传统的破坏性撤销。

## 下一步

- 配置所有 Provider 和运行参数：[配置参考](configuration.md)
- 熟悉交互界面：[CLI 与 TUI](cli-and-tui.md)
- 理解执行确认：[工具与权限](tools-and-permissions.md)
- 部署后台服务：[Agent Server API](server-api.md)
