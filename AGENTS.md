## 一次性开发完成直到所有功能测试通过
## 禁止开子 Agent
## 完整复刻 kimi code
## 代码使用 Rust
## 支持 win/mac/linux x86/arm64 
## 所有提交者都必须是 604498913@qq.com 这个用户
## 你可以复用用开源项目组件，先找一些可能会用到的组件
## LLM 大模型你可以用当前目录的 .env 文件获取测试
## 每次修复问题都做好本地提交
## 每次功能完成后必须本地执行：cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace --all-targets，全部通过后再提交
## 整个 kkagent 读取 ~/.kkagent/config.toml 或通过参数 --config 指定
## 推送或发布版本时，使用当前目录的 upload.sh, 该脚本会预检查，推上去之后需要确认 CI 的结果，失败了需要修复
