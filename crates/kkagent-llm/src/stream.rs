use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;

use crate::types::{ChatContent, LlmRequest, StreamEvent, TokenUsage};

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
    tracing::debug!(
        "LLM request model: {}, tools: {}, thinking: {}",
        request.model,
        request.tools.len(),
        request.thinking.is_some()
    );

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
        tracing::error!("LLM error: HTTP {} - {}", status, truncate_utf8(&text, 500));
        anyhow::bail!("HTTP {}: {}", status, text);
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
            let _msg = event.get("message")?;
            let _usage = _msg.get("usage")?;
            None // we report usage at message_delta end
        }
        "error" => {
            let msg = event
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error")
                .to_string();
            Some(StreamEvent::Error(msg))
        }
        _ => None,
    }
}

pub async fn openai_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in &request.messages {
        // Flatten content blocks into OpenAI-ish messages.
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for c in &m.content {
            match c {
                ChatContent::Text { text } => text_parts.push(text.clone()),
                ChatContent::Thinking { thinking } => text_parts.push(thinking.clone()),
                ChatContent::ToolUse { id, name, input } => {
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": input.to_string(),
                        }
                    }));
                }
                ChatContent::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    tool_results.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    }));
                }
            }
        }
        if m.role == "assistant" {
            let mut msg = json!({"role": "assistant", "content": text_parts.join("\n")});
            if !tool_calls.is_empty() {
                msg["tool_calls"] = json!(tool_calls);
            }
            messages.push(msg);
        } else if m.role == "user" {
            if !text_parts.is_empty() {
                messages.push(json!({"role": "user", "content": text_parts.join("\n")}));
            }
            for tr in tool_results {
                messages.push(tr);
            }
        } else {
            messages.push(json!({"role": &m.role, "content": text_parts.join("\n")}));
        }
    }

    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": &t.name,
                    "description": &t.description,
                    "parameters": &t.input_schema,
                }
            })
        })
        .collect();

    let mut body = json!({
        "model": &request.model,
        "messages": messages,
        "max_tokens": request.max_tokens.min(16384),
        "stream": true,
    });
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .timeout(std::time::Duration::from_secs(300))
        .body(body.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {}: {}", status, text);
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    // Track partial tool call argument deltas by index
    let mut tool_ids: std::collections::HashMap<usize, (String, String)> =
        std::collections::HashMap::new();
    let mut started: std::collections::HashSet<usize> = std::collections::HashSet::new();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].to_string();
            buffer = buffer[pos + 1..].to_string();
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            if data == "[DONE]" {
                let _ = event_tx
                    .send(StreamEvent::MessageEnd {
                        usage: TokenUsage::default(),
                    })
                    .await;
                return Ok(());
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let Some(choices) = event.get("choices").and_then(|c| c.as_array()) else {
                continue;
            };
            let Some(choice) = choices.first() else {
                continue;
            };
            if let Some(delta) = choice.get("delta") {
                if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                    if !content.is_empty() {
                        let _ = event_tx
                            .send(StreamEvent::TextDelta(content.to_string()))
                            .await;
                    }
                }
                if let Some(tcs) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            let name = tc
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            tool_ids.insert(idx, (id.to_string(), name.clone()));
                            if !started.contains(&idx) {
                                started.insert(idx);
                                let _ = event_tx
                                    .send(StreamEvent::ToolUseStart {
                                        id: id.to_string(),
                                        name,
                                    })
                                    .await;
                            }
                        }
                        if let Some(args) = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                        {
                            if !args.is_empty() {
                                let _ = event_tx
                                    .send(StreamEvent::ToolUseInputDelta(args.to_string()))
                                    .await;
                            }
                        }
                    }
                }
            }
            if choice.get("finish_reason").and_then(|v| v.as_str()) == Some("tool_calls") {
                let _ = event_tx.send(StreamEvent::ToolUseEnd).await;
            }
        }
    }
    Ok(())
}

pub async fn google_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    // Google Generative Language API (streamGenerateContent)
    let base = base_url.trim_end_matches('/');
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
        base, request.model, api_key
    );

    let mut contents = Vec::new();
    for m in &request.messages {
        let role = if m.role == "assistant" {
            "model"
        } else {
            "user"
        };
        let mut parts = Vec::new();
        for c in &m.content {
            match c {
                ChatContent::Text { text } => parts.push(json!({"text": text})),
                ChatContent::Thinking { thinking } => parts.push(json!({"text": thinking})),
                ChatContent::ToolUse { id, name, input } => {
                    parts.push(json!({
                        "functionCall": {"name": name, "args": input},
                        "thoughtSignature": id,
                    }));
                }
                ChatContent::ToolResult {
                    tool_use_id: _,
                    content,
                    ..
                } => {
                    parts.push(json!({
                        "functionResponse": {
                            "name": "tool",
                            "response": {"result": content},
                        }
                    }));
                }
            }
        }
        if !parts.is_empty() {
            contents.push(json!({"role": role, "parts": parts}));
        }
    }

    let tools: Vec<serde_json::Value> = if request.tools.is_empty() {
        Vec::new()
    } else {
        let decls: Vec<_> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "name": &t.name,
                    "description": &t.description,
                    "parameters": &t.input_schema,
                })
            })
            .collect();
        vec![json!({"functionDeclarations": decls})]
    };

    let mut body = json!({
        "contents": contents,
        "generationConfig": {"maxOutputTokens": request.max_tokens.min(8192)},
    });
    if let Some(system) = &request.system {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    let resp = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "text/event-stream")
        .timeout(std::time::Duration::from_secs(300))
        .body(body.to_string())
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("HTTP {}: {}", status, text);
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find('\n') {
            let line = buffer[..pos].to_string();
            buffer = buffer[pos + 1..].to_string();
            let line = line.trim();
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let Some(cands) = event.get("candidates").and_then(|c| c.as_array()) else {
                continue;
            };
            let Some(parts) = cands
                .first()
                .and_then(|c| c.get("content"))
                .and_then(|c| c.get("parts"))
                .and_then(|p| p.as_array())
            else {
                continue;
            };
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    let _ = event_tx
                        .send(StreamEvent::TextDelta(text.to_string()))
                        .await;
                }
                if let Some(fc) = part.get("functionCall") {
                    let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                    let args = fc.get("args").cloned().unwrap_or(json!({}));
                    let id = format!("google-{}", uuid::Uuid::new_v4());
                    let _ = event_tx
                        .send(StreamEvent::ToolUseStart {
                            id: id.clone(),
                            name: name.to_string(),
                        })
                        .await;
                    let _ = event_tx
                        .send(StreamEvent::ToolUseInputDelta(args.to_string()))
                        .await;
                    let _ = event_tx.send(StreamEvent::ToolUseEnd).await;
                }
            }
        }
    }
    let _ = event_tx
        .send(StreamEvent::MessageEnd {
            usage: TokenUsage::default(),
        })
        .await;
    Ok(())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod utf8_tests {
    use super::truncate_utf8;

    #[test]
    fn truncates_at_character_boundary() {
        assert_eq!(truncate_utf8("中文错误", 5), "中");
        assert_eq!(truncate_utf8("plain", 50), "plain");
    }
}
