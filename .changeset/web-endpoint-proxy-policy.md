---
"@kkagent/sdk": patch
---

Web search/fetch service endpoints: new `proxy = "auto" | "none" | "system"` policy (default `auto`). Local providers (e.g. SearXNG on `127.0.0.1`) no longer fail when system proxy env vars are set — `auto` bypasses the proxy for loopback / link-local / private endpoints, while direct public GETs keep following the system proxy.
