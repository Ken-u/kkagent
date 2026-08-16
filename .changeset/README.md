# Changesets

Workspace release notes for kkagent. Add a markdown file under `.changeset/` before cutting a release, then bump `Cargo.toml` / npm package versions together with `upload.sh`.

Example:

```md
---
"kkagent": patch
"@kkagent/sdk": patch
---

Document the user-facing change here.
```
