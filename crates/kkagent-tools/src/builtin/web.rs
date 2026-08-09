use async_trait::async_trait;
use reqwest::redirect::Policy;
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
            anyhow::bail!(
                "HTTP {}: {}",
                status,
                text.chars().take(200).collect::<String>()
            );
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
            let title = r
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("(no title)");
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
                // Redirect targets must be validated individually to prevent SSRF.
                .redirect(Policy::none())
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
            .unwrap_or(20_000)
            .min(200_000) as usize;
        let parsed_url = match reqwest::Url::parse(url) {
            Ok(url) => url,
            Err(e) => return Ok(ToolOutput::error(format!("Invalid URL: {e}"))),
        };
        if let Err(e) = validate_public_http_url(&parsed_url).await {
            return Ok(ToolOutput::error(format!("FetchURL blocked: {e}")));
        }

        // Prefer moonshot fetch service when configured
        if let Some(base) = &self.cfg.fetch_base_url {
            let endpoint = format!("{}/v1/fetch", base.trim_end_matches('/'));
            let mut req = self.client.post(&endpoint).json(&json!({ "url": url }));
            if let Some(key) = &self.cfg.fetch_api_key {
                req = req.bearer_auth(key);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    match read_response_limited(resp, 4 * 1024 * 1024).await {
                        Ok((_, text)) if status.is_success() => {
                            let body = truncate(&strip_tags_light(&text), max_chars);
                            return Ok(ToolOutput::success(body));
                        }
                        Ok(_) => tracing::warn!(
                            "moonshot fetch HTTP {}, falling back to direct GET",
                            status
                        ),
                        Err(e) => tracing::warn!(
                            "moonshot fetch response rejected ({e}), falling back to direct GET"
                        ),
                    }
                }
                Err(e) => tracing::warn!("moonshot fetch error {}, falling back: {}", e, e),
            }
        }

        match self.fetch_validated(parsed_url).await {
            Ok(resp) => {
                let status = resp.status();
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let (_, text) = match read_response_limited(resp, 4 * 1024 * 1024).await {
                    Ok(body) => body,
                    Err(e) => return Ok(ToolOutput::error(format!("FetchURL failed: {e}"))),
                };
                if !status.is_success() {
                    return Ok(ToolOutput::error(format!(
                        "FetchURL HTTP {}: {}",
                        status,
                        text.chars().take(300).collect::<String>()
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

impl FetchUrlTool {
    async fn fetch_validated(&self, mut url: reqwest::Url) -> anyhow::Result<reqwest::Response> {
        const MAX_REDIRECTS: usize = 5;
        for redirect_count in 0..=MAX_REDIRECTS {
            validate_public_http_url(&url).await?;
            let response = self.client.get(url.clone()).send().await?;
            if !response.status().is_redirection() {
                return Ok(response);
            }
            if redirect_count == MAX_REDIRECTS {
                anyhow::bail!("too many redirects");
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow::anyhow!("redirect response missing Location header"))?
                .to_str()?;
            url = url.join(location)?;
        }
        unreachable!("redirect loop always returns or errors")
    }
}

async fn read_response_limited(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<(usize, String)> {
    if response
        .content_length()
        .is_some_and(|len| len > max_bytes as u64)
    {
        anyhow::bail!("response exceeds {max_bytes} byte limit");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("response exceeds {max_bytes} byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let len = bytes.len();
    Ok((len, String::from_utf8_lossy(&bytes).into_owned()))
}

async fn validate_public_http_url(url: &reqwest::Url) -> anyhow::Result<()> {
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("only http and https URLs are allowed");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL has no host"))?;
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
        anyhow::bail!("local hosts are not allowed");
    }
    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("URL has no usable port"))?;
    let addresses: Vec<std::net::SocketAddr> =
        tokio::net::lookup_host((host, port)).await?.collect();
    if addresses.is_empty() {
        anyhow::bail!("host resolved to no addresses");
    }
    if let Some(blocked) = addresses.iter().find(|addr| !is_public_ip(addr.ip())) {
        anyhow::bail!("host resolves to blocked address {}", blocked.ip());
    }
    Ok(())
}

fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || ip.is_unspecified()
                || a == 0
                || a >= 240
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0 && c == 0)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && (b == 18 || b == 19)))
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_ip(std::net::IpAddr::V4(v4));
            }
            let segments = ip.segments();
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8))
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

#[cfg(test)]
mod fetch_security_tests {
    use super::*;

    #[test]
    fn blocks_non_public_addresses() {
        for ip in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be blocked");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn rejects_localhost_url() {
        let err = validate_public_http_url(&reqwest::Url::parse("http://localhost/a").unwrap())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("local hosts"));
    }
}
