---
"kkagent": patch
---

Support per-profile subagent default models via `[subagent.default_models]`, including symbolic aliases `current` (parent session model), `default` (`default_model`), `secondary` (`secondary_model`), and the new `fast` (top-level `fast_model`, falling back to `secondary_model` then `default_model`). Tool `model` parameters are now static enums (`default` / `fast` / `current`) so the model never needs to know real model aliases.
