# 发布与安装包

## 支持的产物

修改 `[workspace.package].version` 并推送到 `main` 后，Release workflow 会等待同一提交的 CI 成功，再在对应系统 runner 上执行锁定依赖的 release 构建。若 `v<version>` Release 已存在，自动任务会安全跳过，不重复发布；CI 失败不会发版：

| 系统 | 架构 | Rust target | 资产 |
|---|---|---|---|
| Linux | x86_64 | `x86_64-unknown-linux-gnu` | `.tar.gz` |
| Linux | arm64 | `aarch64-unknown-linux-gnu` | `.tar.gz` |
| Linux (musl, 静态) | x86_64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux (musl, 静态) | arm64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| macOS | x86_64 | `x86_64-apple-darwin` | `.tar.gz` |
| macOS | arm64 | `aarch64-apple-darwin` | `.tar.gz` |
| Windows | x86_64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows | arm64 | `aarch64-pc-windows-msvc` | `.zip` |

每个包包含可执行文件、README 和 MIT License。发布任务等待八个构建全部成功，随后生成 `SHA256SUMS`，使用 GitHub Actions OIDC 对清单执行 Cosign keyless 签名，并上传 `SHA256SUMS.sigstore.json`。

## 安装

macOS / Linux：

```bash
sh install.sh
```

`/usr/local/bin` 可写时默认写入该目录，否则自动使用 `~/.local/bin`。也可选择用户目录或固定版本，并确保目录在 PATH 中：

```bash
KKAGENT_INSTALL_DIR=/absolute/writable/bin sh install.sh
KKAGENT_VERSION=0.2.0 sh install.sh
KKAGENT_TARGET=x86_64-unknown-linux-gnu sh install.sh
```

Windows PowerShell 默认写入 `%LOCALAPPDATA%\Programs\kkagent`，并在缺失时加入用户 PATH：

```powershell
./install.ps1
./install.ps1 -InstallDir 'D:\Tools\kkagent'
./install.ps1 -Version 0.2.0
```

fork 或镜像设置：

```bash
KKAGENT_REPOSITORY=owner/repository sh install.sh
KKAGENT_RELEASE_BASE_URL=https://mirror.example/kkagent/latest sh install.sh
```

PowerShell 使用同名环境变量。两个安装器都会先下载 `SHA256SUMS`，校验目标包，然后通过临时文件替换可执行文件；校验失败不会安装。重复执行安装器即升级到最新版本，或升级到 `KKAGENT_VERSION` / `-Version` 指定的版本。

## 独立验证签名

安装 Cosign 后，在下载资产的目录执行：

```bash
cosign verify-blob \
  --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp 'https://github.com/.*/kkagent/.github/workflows/release.yml@.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  SHA256SUMS
```

签名验证证明清单来自该 GitHub Actions workflow；随后仍应对下载的压缩包执行清单里的 SHA-256 校验。安装脚本默认完成后一步，离线或高信任场景可先手工完成两步验证。

## 发布操作

1. 确保本地测试、真实 Provider 冒烟和文档检查通过。
2. 提升根 `Cargo.toml` 的 `[workspace.package].version` 并推送到 `main`；例如 `0.1.0` 升为 `0.1.1`。
3. 等待同一提交的 CI，以及 `Release` workflow 的版本解析、八个 build job 和 publish job 全部成功。
4. 在 GitHub Release 中确认八个包、校验清单、Sigstore bundle 和自动 release notes。

发布任务会在最后创建 `v<version>` tag 和 Release。需要重新构建已有版本资产时，可手动运行 workflow 并填写现有 tag；上传使用覆盖模式。正式版本不应移动已经对外发布的 tag。

## 版本历史

### v0.1.7

新增：

- **CLI 沙箱临时禁用**：`kkagent --disable-sandbox` 仅在当前进程关闭 Bash OS 沙箱与资源限制，不写回配置，不能与 `--connect` 同时使用。推荐只在受控容器或排障场景使用。
- **TUI 编辑对话历史**：连按两次 `Esc` 可进入当前对话的编辑/重发模式。
- **工作区 Git 信任授权**：启动 Bash 时检测仓库 Git 配置与全局 Git 配置，首次访问会询问用户授权；选择结果写入配置同目录的 `<config>.trust.toml`。未授权时 Bash 注入隔离 Git 配置，跳过 global/system config 与全局 ignore/attributes。
- **无厂商绑定的 Web 搜索**：`WebSearch`/`FetchURL` 工具改为 provider-agnostic 实现，支持更通用的网络抓取配置。
- **会话恢复增强**：恢复会话时保持原工作目录不变。
- **TUI 体验修复**：后台刷新时保留会话选择器光标；`!shell` 类命令在本地直接执行，不进入 Agent loop；thinking 转圈动画渲染简化。

### v0.1.6

首个对外发布版本。提供 TUI、CLI prompt、独立 Server、ACP、MCP、Hooks、Skills、会话管理与 Bash OS 沙箱。
