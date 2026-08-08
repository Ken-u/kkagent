use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;

use crate::types::{LlmRequest, StreamEvent, TokenUsage, ChatContent, ThinkingParams};

pub async fn anthropic_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let url = format!("{}/v1/messages", base_url);

    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            let content: Vec<serde_json::Value> = m.content.iter().map(|c| match c {
                ChatContent::Text { text } => json!({"type": "text", "text": text}),
                ChatContent::ToolUse { id, name, input } => {
                    json!({"type": "tool_use", "id": id, "name": name, "input": input})
                }
                ChatContent::ToolResult { tool_use_id, content, is_error } => {
                    json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
                }
                ChatContent::Thinking { thinking } => json!({"type": "thinking", "thinking": thinking}),
            }).collect();
            json!({"role": &m.role, "content": content})
        })
        .collect();

    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": &t.name,
                "description": &t.description,
                "input_schema": &t.input_schema,
            })
        })
        .collect();

    let max_tokens = request.max_tokens.min(16384);
    let mut body = json!({
        "model": &request.model,
        "messages": messages,
        "max_tokens": max_tokens,
        "stream": true,
    });

    if let Some(system) = &request.system {
        body["system"] = json!(system);
    }

    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    if let Some(thinking) = &request.thinking {
        body["thinking"] = json!({
            "type": "enabled",
            "budget_tokens": thinking.budget_tokens,
        });
    }

    tracing::debug!("LLM request URL: {}", url);
    tracing::debug!("LLM request model: {}, tools: {}, thinking: {}", 
        request.model, request.tools.len(), request.thinking.is_some());

    let resp = client
        .post(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .timeout(std::time::Duration::from_secs(300))
        .body(body.to_string())
        .send()
        .await?;

    tracing::debug!("LLM response status: {}", resp.status());

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        tracing::error!("LLM error: HTTP {} - {}", status, &text[..text.len().min(500)]);
        let _ = event_tx
            .send(StreamEvent::Error(format!("HTTP {}: {}", status, text)))
            .await;
        return Ok(());
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut chunk_count = 0u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        chunk_count += 1;
        if chunk_count <= 3 {
            tracing::debug!("SSE chunk #{}: {} bytes", chunk_count, chunk.len());
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk));

        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].to_string();
            buffer = buffer[pos + 1..].to_string();

            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    return Ok(());
                }
                if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                    if let Some(evt) = parse_sse_event(&event) {
                        if event_tx.send(evt).await.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn parse_sse_event(event: &serde_json::Value) -> Option<StreamEvent> {
    let event_type = event.get("type")?.as_str()?;
    match event_type {
        "content_block_start" => {
            let block = event.get("content_block")?;
            let block_type = block.get("type")?.as_str()?;
            match block_type {
                "tool_use" => {
                    let id = block.get("id")?.as_str()?.to_string();
                    let name = block.get("name")?.as_str()?.to_string();
                    Some(StreamEvent::ToolUseStart { id, name })
                }
                _ => None,
            }
        }
        "content_block_delta" => {
            let delta = event.get("delta")?;
            let delta_type = delta.get("type")?.as_str()?;
            match delta_type {
                "text_delta" => {
                    let text = delta.get("text")?.as_str()?.to_string();
                    Some(StreamEvent::TextDelta(text))
                }
                "thinking_delta" => {
                    let text = delta.get("thinking")?.as_str()?.to_string();
                    Some(StreamEvent::ThinkingDelta(text))
                }
                "input_json_delta" => {
                    let text = delta.get("partial_json")?.as_str()?.to_string();
                    Some(StreamEvent::ToolUseInputDelta(text))
                }
                _ => None,
            }
        }
        "content_block_stop" => Some(StreamEvent::ToolUseEnd),
        "message_delta" => {
            let usage = event.get("usage");
            let token_usage = if let Some(u) = usage {
                TokenUsage {
                    input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    cache_creation_input_tokens: u
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    cache_read_input_tokens: u
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                }
            } else {
                TokenUsage::default()
            };
            Some(StreamEvent::MessageEnd { usage: token_usage })
        }
        "message_start" => {
            let msg = event.get("message")?;
            let usage = msg.get("usage")?;
            let token_usage = TokenUsage {
                input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cache_creation_input_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read_input_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            };
            None // we report usage at message_delta end
        }
        "error" => {
            let msg = event.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Some(StreamEvent::Error(msg))
        }
        _ => None,
    }
}
