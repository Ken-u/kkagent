---
name: toolchain-sandbox
description: Diagnose sandbox / toolchain failures (blocked installs, cache paths, missing mounts).
---

# toolchain-sandbox

Diagnose sandbox / toolchain failures (blocked installs, cache paths, missing mounts).

Use when:
- A Bash command is rejected with a `Blocked toolchain mutation` message (e.g. `npm install -g`): the deny list protects host toolchains; use a workspace-local install instead (`npm_config_cache`/`CARGO_HOME` etc. are redirected to `~/.kkagent/toolchains` when enabled).
- A build picks up the wrong cache/registry path, or seems to re-download everything: caches are profile-scoped and env-redirected only under workspace sandbox mode.
- You need the current toolchain posture (profiles, env keys, cache sizes, deny patterns): run `kkagent doctor` (add `--json` for machine-readable output) via Bash — the `toolchain` check embeds the full report.

Persisting extra paths: grants are no longer a runtime tool; declare them statically in `~/.kkagent/config.toml` under `[toolchain.profiles.<name>]` (`runtime_read_only` / `agent_cache_read_write` / `env`).
