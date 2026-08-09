# 开发与测试

## 本地检查

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
```

对应 Make target 为 `make build`、`make fmt`、`make lint` 和 `make test`。修改局部 crate 时可先执行 `cargo test -p kkagent-tools`，最终提交前仍应跑 workspace 全量检查。测试默认不应依赖外网或真实密钥。

## 实网测试

当前目录的 `.env` 是旧格式本机测试配置，内容是 TOML，因此不会被当成 dotenv 变量加载；可显式作为配置使用：

```bash
cargo run -p kkagent -- --config .env -p "只回复 ok"
cargo run -p kkagent -- --config .env -y -p "读取 Cargo.toml"
```

不要提交 `.env` 或在测试输出中打印密钥。实网测试属于补充验证，不能替代 deterministic 单元/集成测试。

## 修改边界

- 协议字段优先在 `kkagent-protocol` 定义，再更新 RPC/ACP/HTTP 和前端。
- 新配置项需要 schema、默认值、loader、validate、示例和 `configuration.md`。
- 新工具需要参数 schema、路径/权限声明、取消和输出上限测试。
- 新 HTTP 路由必须考虑认证、trusted workspace、body 上限和错误码。
- Provider 流解析必须覆盖正常结束、工具调用、上游错误、截断和 malformed frame。
- 平台相关代码使用 `cfg` 隔离，并考虑 Windows 路径、进程树和 loopback endpoint。

## 跨平台构建

主要目标包括：

```text
x86_64-apple-darwin
aarch64-apple-darwin
x86_64-unknown-linux-gnu / musl
aarch64-unknown-linux-gnu / musl
x86_64-pc-windows-msvc / gnu
aarch64-pc-windows-msvc
```

本机原生构建最可靠。交叉编译需先 `rustup target add <target>`，并安装对应 linker、SDK 或 musl 工具链。GitHub `Release` workflow 在六种 runner/target 组合上构建并打包；本地 `make dist` 只适合已有对应 linker 的开发机，不能代替完整 CI 矩阵。

## Node SDK

```bash
cd sdk/node
npm test
node --check src/index.js
```

JavaScript 和 TypeScript 入口应保持一致。仓库目前未固定 TypeScript compiler 开发依赖；需要类型检查时使用项目外已固定版本的 `tsc --noEmit`。HTTP/WS 行为变化要同步 [SDK README](../sdk/node/README.md) 和 [Server API](server-api.md)。

## 提交约定

仓库要求每次独立修复形成一个本地提交，提交者邮箱必须为 `604498913@qq.com`。提交前检查：

```bash
git config user.email 604498913@qq.com
git diff --check
git status --short
```

不要覆盖用户已有的未提交改动。提交信息建议使用 `type(scope): summary`。

## 发布检查

- 全 workspace format、clippy、test 通过。
- Node SDK 测试、JavaScript 语法检查通过；发布流程使用固定 TypeScript 版本检查类型入口。
- 至少一个真实 Provider 完成对话、Read、Bash 和 Write 冒烟测试。
- HTTP token、WS、ACP、恢复会话、MCP 失败降级通过。
- macOS、Linux、Windows 的 x86_64/arm64 CI 矩阵通过。
- 文档、示例配置、版本和 release notes 已同步。
- Release 资产包含六个平台压缩包、`SHA256SUMS` 和 GitHub OIDC 生成的 Sigstore bundle。
