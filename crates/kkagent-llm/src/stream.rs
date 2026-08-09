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
    let url = api_endpoint(base_url, "messages");

    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            let content: Vec<serde_json::Value> = m.content.iter().map(|c| match c {
                ChatContent::Text { text } => json!({"type": "text", "text": text}),
                ChatContent::Image { media_type, data } => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                }),
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
    let mut tool_blocks = std::collections::HashMap::<u64, String>::new();

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
                    if let Some(evt) = parse_sse_event(&event, &mut tool_blocks) {
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

fn parse_sse_event(
    event: &serde_json::Value,
    tool_blocks: &mut std::collections::HashMap<u64, String>,
) -> Option<StreamEvent> {
    let event_type = event.get("type")?.as_str()?;
    match event_type {
        "content_block_start" => {
            let block = event.get("content_block")?;
            let block_type = block.get("type")?.as_str()?;
            match block_type {
                "tool_use" => {
                    let id = block.get("id")?.as_str()?.to_string();
                    let name = block.get("name")?.as_str()?.to_string();
                    let index = event
                        .get("index")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    tool_blocks.insert(index, id.clone());
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
                    let delta = delta.get("partial_json")?.as_str()?.to_string();
                    let index = event
                        .get("index")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    let id = tool_blocks.get(&index)?.clone();
                    Some(StreamEvent::ToolUseInputDelta { id, delta })
                }
                _ => None,
            }
        }
        "content_block_stop" => {
            let index = event
                .get("index")
                .and_then(|value| value.as_u64())
                .unwrap_or(0);
            tool_blocks
                .remove(&index)
                .map(|id| StreamEvent::ToolUseEnd { id })
        }
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
    chat_completions_stream(client, base_url, api_key, request, event_tx, false).await
}

pub async fn kimi_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    chat_completions_stream(client, base_url, api_key, request, event_tx, true).await
}

async fn chat_completions_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
    kimi: bool,
) -> anyhow::Result<()> {
    let url = api_endpoint(base_url, "chat/completions");

    let mut messages: Vec<serde_json::Value> = Vec::new();
    if let Some(system) = &request.system {
        messages.push(json!({"role": "system", "content": system}));
    }
    for m in &request.messages {
        // Flatten content blocks into OpenAI-ish messages.
        let mut text_parts = Vec::new();
        let mut thinking_parts = Vec::new();
        let mut media_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for c in &m.content {
            match c {
                ChatContent::Text { text } => text_parts.push(text.clone()),
                ChatContent::Image { media_type, data } => {
                    media_parts.push(json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:{media_type};base64,{data}")}
                    }));
                }
                ChatContent::Thinking { thinking } if kimi => {
                    thinking_parts.push(thinking.clone());
                }
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
            if kimi && !thinking_parts.is_empty() {
                msg["reasoning_content"] = json!(thinking_parts.join("\n"));
            }
            messages.push(msg);
        } else if m.role == "user" {
            if !media_parts.is_empty() {
                let mut content: Vec<serde_json::Value> = text_parts
                    .iter()
                    .map(|text| json!({"type": "text", "text": text}))
                    .collect();
                content.extend(media_parts);
                messages.push(json!({"role": "user", "content": content}));
            } else if !text_parts.is_empty() {
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
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if kimi {
        body["max_completion_tokens"] = json!(request.max_tokens);
        if request.thinking.is_some() {
            body["thinking"] = json!({"type": "enabled"});
        }
    } else {
        body["max_tokens"] = json!(request.max_tokens.min(16384));
    }
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
    let mut usage = TokenUsage::default();

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
                let _ = event_tx.send(StreamEvent::MessageEnd { usage }).await;
                return Ok(());
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            if let Some(value) = event.get("usage").or_else(|| {
                event
                    .get("choices")
                    .and_then(|choices| choices.get(0))
                    .and_then(|choice| choice.get("usage"))
            }) {
                update_openai_usage(&mut usage, value);
            }
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
                for key in ["reasoning_content", "reasoning_details", "reasoning"] {
                    if let Some(content) = delta.get(key).and_then(|v| v.as_str()) {
                        if !content.is_empty() {
                            let _ = event_tx
                                .send(StreamEvent::ThinkingDelta(content.to_string()))
                                .await;
                        }
                        break;
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
                                if let Some((id, _)) = tool_ids.get(&idx) {
                                    let _ = event_tx
                                        .send(StreamEvent::ToolUseInputDelta {
                                            id: id.clone(),
                                            delta: args.to_string(),
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }
            }
            if choice.get("finish_reason").and_then(|v| v.as_str()) == Some("tool_calls") {
                for index in started.drain() {
                    if let Some((id, _)) = tool_ids.get(&index) {
                        let _ = event_tx
                            .send(StreamEvent::ToolUseEnd { id: id.clone() })
                            .await;
                    }
                }
            }
        }
    }
    let _ = event_tx.send(StreamEvent::MessageEnd { usage }).await;
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
                ChatContent::Image { media_type, data } => parts.push(json!({
                    "inlineData": {"mimeType": media_type, "data": data}
                })),
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
                        .send(StreamEvent::ToolUseInputDelta {
                            id: id.clone(),
                            delta: args.to_string(),
                        })
                        .await;
                    let _ = event_tx.send(StreamEvent::ToolUseEnd { id }).await;
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

pub(crate) fn api_endpoint(base_url: &str, resource: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/{resource}")
    } else {
        format!("{base}/v1/{resource}")
    }
}

fn update_openai_usage(usage: &mut TokenUsage, value: &serde_json::Value) {
    usage.input_tokens = value
        .get("prompt_tokens")
        .or_else(|| value.get("input_tokens"))
        .and_then(|token| token.as_u64())
        .unwrap_or(usage.input_tokens);
    usage.output_tokens = value
        .get("completion_tokens")
        .or_else(|| value.get("output_tokens"))
        .and_then(|token| token.as_u64())
        .unwrap_or(usage.output_tokens);
    usage.cache_read_input_tokens = value
        .get("prompt_tokens_details")
        .or_else(|| value.get("input_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(|token| token.as_u64())
        .unwrap_or(usage.cache_read_input_tokens);
}

#[cfg(test)]
mod tests {
    use super::{anthropic_stream, kimi_stream, openai_stream, truncate_utf8};
    use crate::types::{ChatContent, ChatMessage, LlmRequest, StreamEvent, ThinkingParams};
    use reqwest::Client;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{mpsc, oneshot},
    };

    struct CapturedRequest {
        head: String,
        body: String,
    }

    async fn serve_once(
        status: &str,
        content_type: &str,
        body: &str,
    ) -> (String, oneshot::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (request_tx, request_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut expected = None;
            loop {
                let mut chunk = [0_u8; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..count]);
                if expected.is_none() {
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..end]);
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        expected = Some(end + 4 + length);
                    }
                }
                if expected.is_some_and(|length| bytes.len() >= length) {
                    break;
                }
            }
            let header_end = bytes
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .unwrap();
            let captured = CapturedRequest {
                head: String::from_utf8_lossy(&bytes[..header_end]).into_owned(),
                body: String::from_utf8_lossy(&bytes[header_end + 4..]).into_owned(),
            };
            let _ = request_tx.send(captured);
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        (format!("http://{address}"), request_rx)
    }

    fn request() -> LlmRequest {
        LlmRequest {
            model: "test-model".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "hello".into(),
                }],
            }],
            tools: Vec::new(),
            max_tokens: 128,
            system: Some("be helpful".into()),
            thinking: None,
        }
    }

    #[test]
    fn truncates_at_character_boundary() {
        assert_eq!(truncate_utf8("中文错误", 5), "中");
        assert_eq!(truncate_utf8("plain", 50), "plain");
    }

    #[tokio::test]
    async fn anthropic_rejects_http_error_and_preserves_unicode() {
        let (base_url, captured) =
            serve_once("429 Too Many Requests", "application/json", "中文限流").await;
        let (tx, _rx) = mpsc::channel(8);
        let error = anthropic_stream(&Client::new(), &base_url, "secret", request(), tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("429"));
        assert!(error.to_string().contains("中文限流"));
        let captured = captured.await.unwrap();
        assert!(captured.head.starts_with("POST /v1/messages HTTP/1.1"));
        assert!(captured
            .head
            .to_ascii_lowercase()
            .contains("x-api-key: secret"));
    }

    #[tokio::test]
    async fn anthropic_streams_text_tool_and_usage() {
        let sse = concat!(
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n",
            "data: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"Read\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n",
            "data: {\"type\":\"content_block_stop\"}\n",
            "data: {\"type\":\"message_delta\",\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}\n"
        );
        let (base_url, captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, mut rx) = mpsc::channel(16);
        anthropic_stream(&Client::new(), &base_url, "secret", request(), tx)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(matches!(&events[0], StreamEvent::TextDelta(text) if text == "hi"));
        assert!(events.iter().any(|event| matches!(event, StreamEvent::ToolUseStart { id, name } if id == "call-1" && name == "Read")));
        assert!(events.iter().any(|event| matches!(event, StreamEvent::MessageEnd { usage } if usage.input_tokens == 4 && usage.output_tokens == 2)));
        let captured = captured.await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
    }

    #[tokio::test]
    async fn openai_uses_bearer_auth_and_streams_events() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n",
            "data: [DONE]\n"
        );
        let (base_url, captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, mut rx) = mpsc::channel(8);
        openai_stream(&Client::new(), &base_url, "token", request(), tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(text)) if text == "hello"));
        assert!(matches!(
            rx.recv().await,
            Some(StreamEvent::MessageEnd { .. })
        ));
        let captured = captured.await.unwrap();
        assert!(captured
            .head
            .to_ascii_lowercase()
            .contains("authorization: bearer token"));
        assert!(captured
            .head
            .starts_with("POST /v1/chat/completions HTTP/1.1"));
    }

    #[tokio::test]
    async fn kimi_uses_native_request_reasoning_and_usage_contract() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"answer\"},\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}]}\n",
            "data: [DONE]\n"
        );
        let (base_url, captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let mut request = request();
        request.thinking = Some(ThinkingParams { budget_tokens: 32 });
        let (tx, mut rx) = mpsc::channel(8);
        kimi_stream(
            &Client::new(),
            &format!("{base_url}/v1"),
            "kimi-token",
            request,
            tx,
        )
        .await
        .unwrap();
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::ThinkingDelta(text)) if text == "think")
        );
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(text)) if text == "answer"));
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::MessageEnd { usage }) if usage.input_tokens == 7 && usage.output_tokens == 3)
        );
        let captured = captured.await.unwrap();
        assert!(captured
            .head
            .starts_with("POST /v1/chat/completions HTTP/1.1"));
        let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(body["max_completion_tokens"], 128);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[tokio::test]
    async fn openai_preserves_ids_for_interleaved_parallel_tool_arguments() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"One\",\"arguments\":\"{\\\"x\\\":\"}},{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"Two\",\"arguments\":\"{\\\"y\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\"2}\"}},{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: [DONE]\n"
        );
        let (base_url, _captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, mut rx) = mpsc::channel(16);
        openai_stream(&Client::new(), &base_url, "token", request(), tx)
            .await
            .unwrap();
        let mut arguments = std::collections::HashMap::<String, String>::new();
        let mut ended = std::collections::HashSet::new();
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::ToolUseInputDelta { id, delta } => {
                    arguments.entry(id).or_default().push_str(&delta);
                }
                StreamEvent::ToolUseEnd { id } => {
                    ended.insert(id);
                }
                _ => {}
            }
        }
        assert_eq!(arguments.get("a").map(String::as_str), Some("{\"x\":1}"));
        assert_eq!(arguments.get("b").map(String::as_str), Some("{\"y\":2}"));
        assert_eq!(
            ended,
            std::collections::HashSet::from(["a".into(), "b".into()])
        );
    }
}
