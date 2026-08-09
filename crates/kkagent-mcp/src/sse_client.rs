//! Classic MCP SSE transport (GET event stream + POST messages).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use reqwest_eventsource::{Event, EventSource};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

struct Pending {
    tx: oneshot::Sender<Value>,
}

/// Minimal JSON-RPC MCP client over legacy SSE transport.
pub struct SseMcpClient {
    post_url: Arc<Mutex<Option<String>>>,
    http: reqwest::Client,
    headers: HashMap<String, String>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    next_id: Arc<Mutex<u64>>,
    _reader: tokio::task::JoinHandle<()>,
}

impl SseMcpClient {
    pub async fn connect(
        sse_url: &str,
        headers: HashMap<String, String>,
        bearer: Option<String>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;

        let post_url = Arc::new(Mutex::new(None::<String>));
        let pending: Arc<Mutex<HashMap<u64, Pending>>> = Arc::new(Mutex::new(HashMap::new()));
        let (endpoint_tx, mut endpoint_rx) = mpsc::channel::<String>(1);

        let post_url_reader = Arc::clone(&post_url);
        let pending_reader = Arc::clone(&pending);
        let sse_url_owned = sse_url.to_string();
        let headers_owned = headers.clone();
        let bearer_owned = bearer.clone();

        let reader = tokio::spawn(async move {
            // Build EventSource from request
            let mut builder = reqwest::Client::new().get(&sse_url_owned);
            builder = builder.header("Accept", "text/event-stream");
            for (k, v) in &headers_owned {
                builder = builder.header(k, v);
            }
            if let Some(token) = &bearer_owned {
                builder = builder.bearer_auth(token);
            }
            let mut es = match EventSource::new(builder) {
                Ok(es) => es,
                Err(e) => {
                    tracing::error!("SSE EventSource create failed: {e}");
                    return;
                }
            };
            while let Some(item) = es.next().await {
                match item {
                    Ok(Event::Open) => {}
                    Ok(Event::Message(msg)) => {
                        let event_name = msg.event;
                        let data = msg.data;
                        if event_name == "endpoint"
                            || event_name.is_empty() && data.starts_with("http")
                        {
                            let endpoint = data.trim().to_string();
                            let absolute = if endpoint.starts_with("http") {
                                endpoint
                            } else {
                                // resolve relative to sse url
                                match url::Url::parse(&sse_url_owned) {
                                    Ok(base) => base
                                        .join(&endpoint)
                                        .map(|u| u.to_string())
                                        .unwrap_or(endpoint),
                                    Err(_) => endpoint,
                                }
                            };
                            *post_url_reader.lock().await = Some(absolute.clone());
                            let _ = endpoint_tx.send(absolute).await;
                        } else {
                            // JSON-RPC response
                            if let Ok(value) = serde_json::from_str::<Value>(&data) {
                                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                                    if let Some(p) = pending_reader.lock().await.remove(&id) {
                                        let _ = p.tx.send(value);
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("SSE stream error: {e}");
                        // keep trying until drop
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        // Wait for endpoint event
        let endpoint = timeout(Duration::from_secs(30), endpoint_rx.recv())
            .await
            .context("waiting for SSE endpoint event")?
            .ok_or_else(|| anyhow!("SSE closed before endpoint"))?;
        tracing::info!("SSE MCP endpoint: {endpoint}");
        let _ = endpoint;

        Ok(Self {
            post_url,
            http,
            headers,
            pending,
            next_id: Arc::new(Mutex::new(1)),
            _reader: reader,
        })
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut n = self.next_id.lock().await;
            let id = *n;
            *n += 1;
            id
        };
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, Pending { tx });

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let post_url = self
            .post_url
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("SSE post endpoint not ready"))?;
        let mut req = self
            .http
            .post(&post_url)
            .header("Content-Type", "application/json")
            .json(&body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.context("SSE POST")?;
        if !resp.status().is_success() && resp.status().as_u16() != 202 {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("SSE POST failed ({status}): {text}");
        }

        let value = timeout(Duration::from_secs(60), rx)
            .await
            .context("SSE RPC timeout")?
            .map_err(|_| anyhow!("SSE RPC cancelled"))?;
        if let Some(err) = value.get("error") {
            anyhow::bail!("SSE RPC error: {err}");
        }
        Ok(value.get("result").cloned().unwrap_or(Value::Null))
    }

    pub async fn initialize(&self) -> Result<()> {
        let _ = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "kkagent", "version": "0.1.0" }
                }),
            )
            .await?;
        // notifications/initialized — fire and forget via POST without waiting id
        let post_url = self
            .post_url
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("no endpoint"))?;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        });
        let mut req = self.http.post(&post_url).json(&body);
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let _ = req.send().await;
        Ok(())
    }

    pub async fn list_tools(&self) -> Result<Vec<crate::client::McpToolInfo>> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .filter_map(|t| {
                Some(crate::client::McpToolInfo {
                    server_name: String::new(),
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(serde_json::json!({"type":"object"})),
                })
            })
            .collect())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<String> {
        let result = self
            .request(
                "tools/call",
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await?;
        let mut output = String::new();
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    output.push_str(text);
                }
            }
        }
        if output.is_empty() {
            output = result.to_string();
        }
        Ok(output)
    }
}
