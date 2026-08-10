//! Web search / fetch providers — provider-agnostic, no Kimi/Moonshot coupling.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub published_at: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub limit: usize,
}

#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, req: &SearchRequest) -> anyhow::Result<Vec<SearchHit>>;
}

#[derive(Debug, Clone)]
pub struct WebSearchServiceConfig {
    pub provider: String,
    /// Full search endpoint URL (not auto-suffixed).
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_ms: u64,
    pub default_limit: usize,
}

#[derive(Debug, Clone, Default)]
pub struct WebFetchServiceConfig {
    /// Optional proxy fetch endpoint (full URL). When unset, FetchURL uses direct GET.
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub timeout_ms: u64,
}

#[derive(Clone)]
pub struct WebServicesConfig {
    pub search: Option<WebSearchServiceConfig>,
    pub fetch: WebFetchServiceConfig,
    pub migration_hint: Option<String>,
}

impl WebServicesConfig {
    pub fn from_app(config: &kkagent_config::AppConfig) -> Self {
        let services = config.services.as_ref();
        let mut migration_hint = None;

        let search = if let Some(ws) = services.and_then(|s| s.web_search.as_ref()) {
            let api_key = resolve_api_key(ws.api_key.as_deref(), ws.api_key_env.as_deref());
            Some(WebSearchServiceConfig {
                provider: ws
                    .provider
                    .clone()
                    .unwrap_or_else(|| "searxng".into())
                    .to_ascii_lowercase(),
                base_url: ws.base_url.clone(),
                api_key,
                timeout_ms: ws.timeout_ms.unwrap_or(15_000),
                default_limit: ws.default_limit.unwrap_or(5).clamp(1, 20),
            })
        } else if let Some(old) = services.and_then(|s| s.moonshot_search.as_ref()) {
            migration_hint = Some(
                "Deprecated [services.moonshot_search] detected — migrate to [services.web_search]"
                    .into(),
            );
            // Compat: treat legacy base_url as service root and append /v1/search once.
            let base = normalize_legacy_moonshot_search_endpoint(&old.base_url);
            Some(WebSearchServiceConfig {
                provider: "custom".into(),
                base_url: base,
                api_key: old.api_key.clone().or_else(|| {
                    services
                        .and_then(|s| s.moonshot_fetch.as_ref())
                        .and_then(|e| e.api_key.clone())
                }),
                timeout_ms: 15_000,
                default_limit: 5,
            })
        } else {
            None
        };

        let fetch = if let Some(wf) = services.and_then(|s| s.web_fetch.as_ref()) {
            WebFetchServiceConfig {
                base_url: Some(wf.base_url.clone()),
                api_key: resolve_api_key(wf.api_key.as_deref(), wf.api_key_env.as_deref()),
                timeout_ms: wf.timeout_ms.unwrap_or(30_000),
            }
        } else if let Some(old) = services.and_then(|s| s.moonshot_fetch.as_ref()) {
            if migration_hint.is_none() {
                migration_hint = Some(
                    "Deprecated [services.moonshot_fetch] detected — migrate to [services.web_fetch]"
                        .into(),
                );
            }
            WebFetchServiceConfig {
                base_url: Some(normalize_legacy_moonshot_fetch_endpoint(&old.base_url)),
                api_key: old.api_key.clone(),
                timeout_ms: 30_000,
            }
        } else {
            WebFetchServiceConfig {
                base_url: None,
                api_key: None,
                timeout_ms: 30_000,
            }
        };

        Self {
            search,
            fetch,
            migration_hint,
        }
    }

    pub fn search_configured(&self) -> bool {
        self.search
            .as_ref()
            .is_some_and(|s| !s.base_url.trim().is_empty())
    }

    pub fn build_search_provider(&self) -> Option<std::sync::Arc<dyn WebSearchProvider>> {
        let cfg = self.search.as_ref()?;
        if cfg.base_url.trim().is_empty() {
            return None;
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.timeout_ms.max(1_000)))
            .build()
            .ok()?;
        let provider: std::sync::Arc<dyn WebSearchProvider> = match cfg.provider.as_str() {
            "brave" => std::sync::Arc::new(BraveProvider {
                endpoint: cfg.base_url.clone(),
                api_key: cfg.api_key.clone(),
                client,
            }),
            "searxng" => std::sync::Arc::new(SearxngProvider {
                endpoint: cfg.base_url.clone(),
                api_key: cfg.api_key.clone(),
                client,
            }),
            _ => std::sync::Arc::new(CustomJsonProvider {
                endpoint: cfg.base_url.clone(),
                api_key: cfg.api_key.clone(),
                client,
                legacy_moonshot_shape: cfg.provider == "custom"
                    && cfg.base_url.contains("/v1/search"),
            }),
        };
        Some(provider)
    }
}

fn resolve_api_key(inline: Option<&str>, env_name: Option<&str>) -> Option<String> {
    if let Some(name) = env_name {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    inline
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn normalize_legacy_moonshot_search_endpoint(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1/search") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/search")
    }
}

fn normalize_legacy_moonshot_fetch_endpoint(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1/fetch") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/fetch")
    }
}

pub fn normalize_url(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let parsed = reqwest::Url::parse(t).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    Some(parsed.to_string())
}

pub fn dedupe_limit(hits: Vec<SearchHit>, limit: usize) -> Vec<SearchHit> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for hit in hits {
        let Some(url) = normalize_url(&hit.url) else {
            continue;
        };
        if !seen.insert(url.clone()) {
            continue;
        }
        out.push(SearchHit {
            url,
            title: hit.title,
            snippet: hit.snippet,
            published_at: hit.published_at,
            source: hit.source,
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

struct SearxngProvider {
    endpoint: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

#[async_trait]
impl WebSearchProvider for SearxngProvider {
    fn name(&self) -> &str {
        "searxng"
    }

    async fn search(&self, req: &SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let mut url = reqwest::Url::parse(&self.endpoint)
            .map_err(|e| anyhow::anyhow!("invalid web_search.base_url: {e}"))?;
        // SearXNG JSON API: endpoint is the search path (often .../search).
        url.query_pairs_mut()
            .append_pair("q", &req.query)
            .append_pair("format", "json");
        let mut builder = self.client.get(url);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        let resp = builder.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "SearXNG HTTP {}: {}",
                status,
                text.chars().take(200).collect::<String>()
            );
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("SearXNG returned malformed JSON: {e}"))?;
        let results = parsed
            .get("results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut hits = Vec::new();
        for r in results {
            let title = r
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)")
                .to_string();
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let snippet = r
                .get("content")
                .or_else(|| r.get("snippet"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let published_at = r
                .get("publishedDate")
                .or_else(|| r.get("published_at"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let source = r
                .get("engine")
                .or_else(|| r.get("source"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            hits.push(SearchHit {
                title,
                url,
                snippet,
                published_at,
                source,
            });
        }
        Ok(dedupe_limit(hits, req.limit))
    }
}

struct BraveProvider {
    endpoint: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

#[async_trait]
impl WebSearchProvider for BraveProvider {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(&self, req: &SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let mut url = reqwest::Url::parse(&self.endpoint)
            .map_err(|e| anyhow::anyhow!("invalid web_search.base_url: {e}"))?;
        url.query_pairs_mut()
            .append_pair("q", &req.query)
            .append_pair("count", &req.limit.to_string());
        let mut builder = self.client.get(url);
        if let Some(key) = &self.api_key {
            builder = builder.header("X-Subscription-Token", key);
        }
        let resp = builder.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Brave HTTP {}: {}",
                status,
                text.chars().take(200).collect::<String>()
            );
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Brave returned malformed JSON: {e}"))?;
        let results = parsed
            .pointer("/web/results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut hits = Vec::new();
        for r in results {
            hits.push(SearchHit {
                title: r
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no title)")
                    .to_string(),
                url: r
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                snippet: r
                    .get("description")
                    .or_else(|| r.get("snippet"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                published_at: r
                    .get("age")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                source: Some("brave".into()),
            });
        }
        Ok(dedupe_limit(hits, req.limit))
    }
}

/// Custom JSON endpoint. Supports:
/// - Open shape: `{ "results": [ { title, url, snippet } ] }`
/// - Legacy moonshot shape: `{ "search_results": [ ... ] }` via POST `{ "text_query": ... }`
struct CustomJsonProvider {
    endpoint: String,
    api_key: Option<String>,
    client: reqwest::Client,
    legacy_moonshot_shape: bool,
}

#[async_trait]
impl WebSearchProvider for CustomJsonProvider {
    fn name(&self) -> &str {
        "custom"
    }

    async fn search(&self, req: &SearchRequest) -> anyhow::Result<Vec<SearchHit>> {
        let resp = if self.legacy_moonshot_shape {
            let mut builder = self
                .client
                .post(&self.endpoint)
                .json(&json!({ "text_query": req.query }));
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            builder.send().await?
        } else {
            let mut url = reqwest::Url::parse(&self.endpoint)
                .map_err(|e| anyhow::anyhow!("invalid web_search.base_url: {e}"))?;
            url.query_pairs_mut()
                .append_pair("q", &req.query)
                .append_pair("limit", &req.limit.to_string());
            let mut builder = self.client.get(url);
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            builder.send().await?
        };
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "Search HTTP {}: {}",
                status,
                text.chars().take(200).collect::<String>()
            );
        }
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("Search returned malformed JSON: {e}"))?;
        let results = parsed
            .get("results")
            .or_else(|| parsed.get("search_results"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut hits = Vec::new();
        for r in results {
            hits.push(SearchHit {
                title: r
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no title)")
                    .to_string(),
                url: r.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                snippet: r
                    .get("snippet")
                    .or_else(|| r.get("content"))
                    .or_else(|| r.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                published_at: r
                    .get("published_at")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                source: r
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
            });
        }
        Ok(dedupe_limit(hits, req.limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn dedupe_and_normalize() {
        let hits = vec![
            SearchHit {
                title: "a".into(),
                url: "https://example.com/a".into(),
                snippet: "".into(),
                published_at: None,
                source: None,
            },
            SearchHit {
                title: "dup".into(),
                url: "https://example.com/a".into(),
                snippet: "".into(),
                published_at: None,
                source: None,
            },
            SearchHit {
                title: "b".into(),
                url: "https://example.com/b".into(),
                snippet: "".into(),
                published_at: None,
                source: None,
            },
        ];
        let out = dedupe_limit(hits, 10);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn legacy_endpoint_not_double_suffixed() {
        assert_eq!(
            normalize_legacy_moonshot_search_endpoint("https://x/v1/search"),
            "https://x/v1/search"
        );
        assert_eq!(
            normalize_legacy_moonshot_search_endpoint("https://x/"),
            "https://x/v1/search"
        );
    }

    async fn serve_once(status: u16, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
        format!("http://{addr}/search")
    }

    #[tokio::test]
    async fn searxng_success_and_dedupe() {
        let endpoint = serve_once(
            200,
            r#"{"results":[
                {"title":"A","url":"https://ex.com/a","content":"one"},
                {"title":"Dup","url":"https://ex.com/a","content":"dup"},
                {"title":"B","url":"https://ex.com/b","content":"two"}
            ]}"#,
        )
        .await;
        let p = SearxngProvider {
            endpoint,
            api_key: None,
            client: reqwest::Client::new(),
        };
        let hits = p
            .search(&SearchRequest {
                query: "q".into(),
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "A");
    }

    #[tokio::test]
    async fn brave_401() {
        let endpoint = serve_once(401, r#"{"error":"unauthorized"}"#).await;
        let p = BraveProvider {
            endpoint,
            api_key: Some("secret-key-should-not-leak".into()),
            client: reqwest::Client::new(),
        };
        let err = p
            .search(&SearchRequest {
                query: "q".into(),
                limit: 5,
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401") || err.to_ascii_lowercase().contains("unauthor"));
        assert!(!err.contains("secret-key-should-not-leak"));
    }

    #[tokio::test]
    async fn custom_empty_and_malformed() {
        let endpoint = serve_once(200, r#"{"results":[]}"#).await;
        let p = CustomJsonProvider {
            endpoint: endpoint.clone(),
            api_key: None,
            client: reqwest::Client::new(),
            legacy_moonshot_shape: false,
        };
        let hits = p
            .search(&SearchRequest {
                query: "q".into(),
                limit: 5,
            })
            .await
            .unwrap();
        assert!(hits.is_empty());

        let endpoint = serve_once(200, "not-json").await;
        let p = CustomJsonProvider {
            endpoint,
            api_key: None,
            client: reqwest::Client::new(),
            legacy_moonshot_shape: false,
        };
        assert!(p
            .search(&SearchRequest {
                query: "q".into(),
                limit: 5,
            })
            .await
            .is_err());
    }

    #[tokio::test]
    async fn provider_429_and_5xx() {
        for status in [429u16, 503] {
            let endpoint = serve_once(status, r#"{"error":"busy"}"#).await;
            let p = SearxngProvider {
                endpoint,
                api_key: None,
                client: reqwest::Client::new(),
            };
            let err = p
                .search(&SearchRequest {
                    query: "q".into(),
                    limit: 3,
                })
                .await
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&status.to_string()),
                "status={status} err={err}"
            );
        }
    }

    #[test]
    fn try_new_without_config_is_none() {
        let cfg = Arc::new(WebServicesConfig {
            search: None,
            fetch: WebFetchServiceConfig::default(),
            migration_hint: None,
        });
        assert!(crate::builtin::WebSearchTool::try_new(cfg).is_none());
    }
}
