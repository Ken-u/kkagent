//! WebSearch / FetchURL tools — provider-agnostic.

use async_trait::async_trait;
use reqwest::redirect::Policy;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::web_providers::{SearchRequest, WebSearchProvider, WebServicesConfig};
use crate::{Tool, ToolContext, ToolOutput};

pub struct WebSearchTool {
    cfg: Arc<WebServicesConfig>,
    provider: Arc<dyn WebSearchProvider>,
}

impl WebSearchTool {
    pub fn try_new(cfg: Arc<WebServicesConfig>) -> Option<Self> {
        let provider = cfg.build_search_provider()?;
        Some(Self { cfg, provider })
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "WebSearch"
    }
    fn description(&self) -> &str {
        "Search the web for up-to-date information. Requires [services.web_search] in config."
    }
    fn read_only(&self) -> bool {
        true
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "limit": {"type": "integer", "description": "Max results (default from config)"}
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
        let default_limit = self
            .cfg
            .search
            .as_ref()
            .map(|s| s.default_limit)
            .unwrap_or(5);
        let limit = input
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(default_limit)
            .clamp(1, 20);

        match self
            .provider
            .search(&SearchRequest {
                query: query.to_string(),
                limit,
            })
            .await
        {
            Ok(hits) if hits.is_empty() => Ok(ToolOutput::success("No search results.")),
            Ok(hits) => {
                let mut lines = Vec::new();
                let mut urls = Vec::new();
                for (i, h) in hits.iter().enumerate() {
                    urls.push(h.url.clone());
                    let mut line = format!("{}. {}\n   {}\n   {}", i + 1, h.title, h.url, {
                        h.snippet.chars().take(300).collect::<String>()
                    });
                    if let Some(src) = &h.source {
                        line.push_str(&format!("\n   source: {src}"));
                    }
                    if let Some(at) = &h.published_at {
                        line.push_str(&format!("\n   published: {at}"));
                    }
                    lines.push(line);
                }
                let mut out = ToolOutput::success_with_data(
                    lines.join("\n\n"),
                    json!({
                        "provider": self.provider.name(),
                        "count": hits.len(),
                        "urls": urls,
                    }),
                );
                if let Some(hint) = &self.cfg.migration_hint {
                    out = out.with_note(hint.clone());
                }
                Ok(out)
            }
            Err(e) => {
                let msg = e.to_string();
                // Never leak API keys in tool output.
                let safe = redact_secrets(&msg);
                Ok(ToolOutput::error(format!(
                    "WebSearch failed ({provider}): {safe}. Configure [services.web_search] with a working provider (searxng / brave / custom).",
                    provider = self.provider.name()
                )))
            }
        }
    }
}

pub struct FetchUrlTool {
    cfg: Arc<WebServicesConfig>,
    client: reqwest::Client,
}

impl FetchUrlTool {
    pub fn new(cfg: Arc<WebServicesConfig>) -> Self {
        let timeout = Duration::from_millis(cfg.fetch.timeout_ms.max(1_000));
        Self {
            cfg,
            client: reqwest::Client::builder()
                .timeout(timeout)
                .redirect(Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

use std::time::Duration;

#[async_trait]
impl Tool for FetchUrlTool {
    fn name(&self) -> &str {
        "FetchURL"
    }
    fn description(&self) -> &str {
        "Fetch a URL and return extracted text content. Works with direct HTTP GET; optional [services.web_fetch] proxy."
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

        if let Some(endpoint) = &self.cfg.fetch.base_url {
            let mut req = self.client.post(endpoint).json(&json!({ "url": url }));
            if let Some(key) = &self.cfg.fetch.api_key {
                req = req.bearer_auth(key);
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    match read_response_limited(resp, 4 * 1024 * 1024).await {
                        Ok((_, text)) if status.is_success() => {
                            let body = truncate(&extract_readable_text(&text), max_chars);
                            return Ok(ToolOutput::success_with_data(
                                body,
                                json!({"source_url": url, "via": "web_fetch"}),
                            ));
                        }
                        Ok(_) => tracing::warn!(
                            "web_fetch HTTP {}, falling back to direct GET",
                            status
                        ),
                        Err(e) => tracing::warn!(
                            "web_fetch response rejected ({e}), falling back to direct GET"
                        ),
                    }
                }
                Err(e) => tracing::warn!("web_fetch error {e}, falling back to direct GET"),
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
                if !(ct.is_empty()
                    || ct.contains("text/")
                    || ct.contains("json")
                    || ct.contains("xml")
                    || ct.contains("html")
                    || ct.contains("javascript"))
                {
                    return Ok(ToolOutput::error(format!(
                        "FetchURL unsupported content-type: {ct}"
                    )));
                }
                let body = if ct.contains("html") || text.trim_start().starts_with('<') {
                    extract_readable_text(&text)
                } else {
                    text
                };
                Ok(ToolOutput::success_with_data(
                    truncate(&body, max_chars),
                    json!({"source_url": url, "via": "direct"}),
                ))
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

fn redact_secrets(s: &str) -> String {
    // Best-effort: drop bearer-looking tokens from error strings.
    let mut out = s.to_string();
    for marker in ["Bearer ", "api_key=", "X-Subscription-Token:"] {
        if let Some(i) = out.find(marker) {
            let rest = &out[i + marker.len()..];
            let end = rest
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .unwrap_or(rest.len().min(16));
            out.replace_range(i + marker.len()..i + marker.len() + end, "***");
        }
    }
    out
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

/// Stronger than tag-stripping alone: drop script/style/noscript blocks first.
fn extract_readable_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut cleaned = String::new();
    let mut i = 0;
    let chars: Vec<char> = html.chars().collect();
    let lower_chars: Vec<char> = lower.chars().collect();
    while i < chars.len() {
        let rest: String = lower_chars[i..].iter().collect();
        if rest.starts_with("<script") || rest.starts_with("<style") || rest.starts_with("<noscript")
        {
            let end_tag = if rest.starts_with("<script") {
                "</script>"
            } else if rest.starts_with("<style") {
                "</style>"
            } else {
                "</noscript>"
            };
            if let Some(rel) = rest.find(end_tag) {
                i += rel + end_tag.chars().count();
                continue;
            }
        }
        cleaned.push(chars[i]);
        i += 1;
    }
    let mut out = String::with_capacity(cleaned.len());
    let mut in_tag = false;
    for ch in cleaned.chars() {
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

    #[test]
    fn strips_script_blocks() {
        let html = "<html><script>alert(1)</script><p>Hello <b>world</b></p></html>";
        let text = extract_readable_text(html);
        assert!(text.contains("Hello"));
        assert!(!text.contains("alert"));
    }
}
