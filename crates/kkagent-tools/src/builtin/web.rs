use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::{Tool, ToolContext, ToolOutput};

#[derive(Clone)]
pub struct WebServicesConfig {
    pub search_base_url: Option<String>,
    pub search_api_key: Option<String>,
    pub fetch_base_url: Option<String>,
    pub fetch_api_key: Option<String>,
}

impl WebServicesConfig {
    pub fn from_app(config: &kkagent_config::AppConfig) -> Self {
        let services = config.services.as_ref();
        Self {
            search_base_url: services
                .and_then(|s| s.moonshot_search.as_ref())
                .map(|e| e.base_url.clone()),
            search_api_key: services
                .and_then(|s| s.moonshot_search.as_ref())
                .and_then(|e| e.api_key.clone())
                .or_else(|| {
                    services
                        .and_then(|s| s.moonshot_fetch.as_ref())
                        .and_then(|e| e.api_key.clone())
                }),
            fetch_base_url: services
                .and_then(|s| s.moonshot_fetch.as_ref())
                .map(|e| e.base_url.clone()),
            fetch_api_key: services
                .and_then(|s| s.moonshot_fetch.as_ref())
                .and_then(|e| e.api_key.clone()),
        }
    }
}

pub struct WebSearchTool {
    cfg: Arc<WebServicesConfig>,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(cfg: Arc<WebServicesConfig>) -> Self {
        Self {
            cfg,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }
    fn description(&self) -> &str {
        "Search the web for up-to-date information. Requires services.moonshot_search in config."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "limit": {"type": "integer", "description": "Max results (default 5)"}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if query.is_empty() {
            return Ok(ToolOutput::error("Missing query"));
        }
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(5) as usize;

        // Provider matrix: moonshot → local DuckDuckGo HTML fallback.
        if let Some(base) = self.cfg.search_base_url.as_ref() {
            match self.moonshot_search(base, query, limit).await {
                Ok(out) => return Ok(out),
                Err(e) => tracing::warn!("moonshot search failed ({e}), trying local fallback"),
            }
        }
        match self.local_ddg_search(query, limit).await {
            Ok(out) => Ok(out),
            Err(e) => Ok(ToolOutput::error(format!(
                "WebSearch failed (moonshot+local): {e}. Configure [services.moonshot_search]."
            ))),
        }
    }
}

impl WebSearchTool {
    async fn moonshot_search(
        &self,
        base: &str,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<ToolOutput> {
        let url = format!("{}/v1/search", base.trim_end_matches('/'));
        let mut req = self.client.post(&url).json(&json!({ "text_query": query }));
        if let Some(key) = &self.cfg.search_api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("HTTP {}: {}", status, &text[..text.len().min(200)]);
        }
        let parsed: Value = serde_json::from_str(&text).unwrap_or(json!({}));
        let results = parsed
            .get("search_results")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if results.is_empty() {
            return Ok(ToolOutput::success("No search results."));
        }
        let mut lines = Vec::new();
        for (i, r) in results.iter().take(limit).enumerate() {
            let title = r.get("title").and_then(|v| v.as_str()).unwrap_or("(no title)");
            let url = r.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let snippet = r
                .get("snippet")
                .or_else(|| r.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            lines.push(format!(
                "{}. {}\n   {}\n   {}",
                i + 1,
                title,
                url,
                snippet.chars().take(300).collect::<String>()
            ));
        }
        Ok(ToolOutput::success_with_data(
            lines.join("\n\n"),
            json!({"provider": "moonshot", "count": lines.len()}),
        ))
    }

    async fn local_ddg_search(&self, query: &str, limit: usize) -> anyhow::Result<ToolOutput> {
        let url = format!(
            "https://html.duckduckgo.com/html/?q={}",
            urlencoding_minimal(query)
        );
        let resp = self
            .client
            .get(&url)
            .header("User-Agent", "kkagent/0.1")
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("ddg HTTP {status}");
        }
        // Very light scrape: collect result-like anchors.
        let mut lines = Vec::new();
        for (i, chunk) in text.split("result__a").skip(1).take(limit).enumerate() {
            let href = chunk
                .split("href=\"")
                .nth(1)
                .and_then(|s| s.split('"').next())
                .unwrap_or("");
            let title = chunk
                .split('>')
                .nth(1)
                .and_then(|s| s.split('<').next())
                .unwrap_or("(result)")
                .trim();
            lines.push(format!("{}. {}\n   {}", i + 1, title, href));
        }
        if lines.is_empty() {
            anyhow::bail!("no local results parsed");
        }
        Ok(ToolOutput::success_with_data(
            lines.join("\n\n"),
            json!({"provider": "local-ddg", "count": lines.len()}),
        ))
    }
}

fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct FetchUrlTool {
    cfg: Arc<WebServicesConfig>,
    client: reqwest::Client,
}

impl FetchUrlTool {
    pub fn new(cfg: Arc<WebServicesConfig>) -> Self {
        Self {
            cfg,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

#[async_trait]
impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "FetchURL"
    }
    fn description(&self) -> &str {
        "Fetch a URL and return text content (HTML stripped lightly). Uses moonshot_fetch when configured, else direct HTTP GET."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "URL to fetch"},
                "max_chars": {"type": "integer", "description": "Max characters to return (default 20000)"}
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let url = input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if url.is_empty() {
            return Ok(ToolOutput::error("Missing url"));
        }
        let max_chars = input
            .get("max_chars")
            .and_then(|v| v.as_u64())
            .unwrap_or(20_000) as usize;

        // Prefer moonshot fetch service when configured
        if let Some(base) = &self.cfg.fetch_base_url {
            let endpoint = format!("{}/v1/fetch", base.trim_end_matches('/'));
            let mut req = self
                .client
                .post(&endpoint)
                .json(&json!({ "url": url }));
            if let Some(key) = &self.cfg.fetch_api_key {
                req = req.bearer_auth(key);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.is_success() {
                        let body = truncate(&strip_tags_light(&text), max_chars);
                        return Ok(ToolOutput::success(body));
                    }
                    tracing::warn!("moonshot fetch HTTP {}, falling back to direct GET", status);
                }
                Err(e) => tracing::warn!("moonshot fetch error {}, falling back: {}", e, e),
            }
        }

        match self.client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let text = resp.text().await.unwrap_or_default();
                if !status.is_success() {
                    return Ok(ToolOutput::error(format!(
                        "FetchURL HTTP {}: {}",
                        status,
                        &text[..text.len().min(300)]
                    )));
                }
                let body = if ct.contains("html") {
                    strip_tags_light(&text)
                } else {
                    text
                };
                Ok(ToolOutput::success(truncate(&body, max_chars)))
            }
            Err(e) => Ok(ToolOutput::error(format!("FetchURL failed: {}", e))),
        }
    }
}

fn strip_tags_light(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    let t: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        format!("{}\n… truncated …", t)
    } else {
        t
    }
}
