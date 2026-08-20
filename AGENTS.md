## 开发要求

* 一次性完成用户请求，不要在开发过程中因普通实现细节反复等待确认。
* 禁止使用子 Agent 开发。
* 代码使用 Rust。
* 支持 Windows / macOS / Linux，x86_64 / arm64。
* 读取开发者自定义规范 @LOCAL.md。
* 可以优先复用成熟的开源组件。
* LLM 测试可使用当前目录 .env 中的配置。
* kkagent 读取 ~/.kkagent/config.toml，或通过 --config 指定。

## 修改与验证

* 开发过程中只执行与当前修改直接相关的最小验证，例如受影响 crate 的 cargo check、相关测试或 targeted test。
* 不要在每次中间修改后执行全 workspace 验证。
* 功能实现并完成必要的最小验证后，将结果交给用户确认。
* 用户确认功能完成后，再统一执行：

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets
```

* 全部通过后创建最终本地提交。
* 如果完整检查发现问题，继续修复并重新执行必要检查，直到全部通过。
* 纯文档修改或仅修改 .gitignore 不要求执行完整验证，也不要求提交。

## Git

* 用户确认功能完成并通过最终检查后，创建一个逻辑完整的本地 commit。
* 不要为同一任务的中间修改反复提交。
* 不要覆盖、回退或提交用户以及其他并发会话已有的无关修改。
* 推送或发布版本时使用当前目录的 upload.sh。
* upload.sh 推送完成后确认 CI 结果；CI 失败则继续修复直到通过。
