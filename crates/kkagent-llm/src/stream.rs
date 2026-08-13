use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;

use crate::types::{ChatContent, LlmRequest, StreamEvent, TokenUsage};

const ANTHROPIC_FALLBACK_MAX_TOKENS: u32 = 128_000;

/// Resolve max_tokens for Anthropic with per-model ceiling.
fn resolve_anthropic_max_tokens(model: &str, override_: Option<u32>) -> Option<u32> {
    let ceiling = anthropic_model_ceiling(model).unwrap_or(ANTHROPIC_FALLBACK_MAX_TOKENS);
    Some(override_.map(|o| o.min(ceiling)).unwrap_or(ceiling))
}

/// Heuristic ceiling based on Anthropic model family/version.
fn anthropic_model_ceiling(model: &str) -> Option<u32> {
    let lower = model.to_ascii_lowercase();
    // Claude 4 series supports 64k+ outputs; treat as 128k for newer endpoints.
    if lower.contains("claude-opus-4")
        || lower.contains("claude-sonnet-4")
        || lower.contains("claude-haiku-4")
    {
        return Some(128_000);
    }
    // Claude 3.x series is generally limited to 8k output.
    if lower.contains("claude-3") {
        return Some(8_192);
    }
    // Older/unrecognized Anthropic models: rely on fallback.
    None
}

pub async fn anthropic_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let url = api_endpoint(base_url, "messages");
    reject_video_inputs(&request, "Anthropic")?;

    let mut messages = Vec::new();
    for message in &request.messages {
        let content: Vec<serde_json::Value> = message
            .content
            .iter()
            .map(|content| match content {
                ChatContent::Text { text } => json!({"type": "text", "text": text}),
                ChatContent::Image { media_type, data } => json!({
                    "type": "image",
                    "source": {"type": "base64", "media_type": media_type, "data": data}
                }),
                ChatContent::Video { .. } => unreachable!("video inputs rejected above"),
                ChatContent::ToolUse { id, name, input } => {
                    json!({"type": "tool_use", "id": id, "name": name, "input": input})
                }
                ChatContent::ToolResult { tool_use_id, content, is_error } => {
                    json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
                }
                ChatContent::Thinking { thinking } => json!({"type": "thinking", "thinking": thinking}),
            })
            .collect();
        push_strict_provider_message(&mut messages, &message.role, "content", content, |part| {
            part.get("type").and_then(serde_json::Value::as_str) == Some("tool_result")
        });
    }

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

    let mut body = json!({
        "model": &request.model,
        "messages": messages,
        "stream": true,
    });
    if let Some(max_tokens) = resolve_anthropic_max_tokens(&request.model, request.max_tokens) {
        body["max_tokens"] = json!(max_tokens);
    }

    if let Some(system) = &request.system {
        body["system"] = json!(system);
    }

    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    if let Some(thinking) = &request.thinking {
        if thinking.adaptive {
            body["thinking"] = json!({"type": "adaptive"});
            if let Some(effort) = &thinking.effort {
                body["output_config"] = json!({"effort": effort});
            }
        } else {
            body["thinking"] = json!({
                "type": "enabled",
                "budget_tokens": thinking.budget_tokens,
            });
        }
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
        tracing::error!("LLM error: HTTP {}", status);
        return Err(crate::response_error(resp).await);
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut chunk_count = 0u64;
    let mut tool_blocks = std::collections::HashMap::<u64, String>::new();
    let mut usage = TokenUsage::default();
    let mut stop_reason = None;
    let mut completed = false;

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
                    if completed {
                        return Ok(());
                    }
                    anyhow::bail!("Anthropic stream ended before message_stop");
                }
                let event = serde_json::from_str::<serde_json::Value>(data)
                    .map_err(|error| anyhow::anyhow!("invalid Anthropic SSE JSON: {error}"))?;
                if event.get("type").and_then(|value| value.as_str()) == Some("error") {
                    let message = event
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(|message| message.as_str())
                        .unwrap_or("unknown Anthropic stream error");
                    anyhow::bail!("Anthropic stream error: {message}");
                }
                if let Some(evt) =
                    parse_sse_event(&event, &mut tool_blocks, &mut usage, &mut stop_reason)
                {
                    completed |= matches!(evt, StreamEvent::MessageEnd { .. });
                    if event_tx.send(evt).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    if completed {
        Ok(())
    } else {
        anyhow::bail!("Anthropic stream connection closed before message_stop")
    }
}

fn parse_sse_event(
    event: &serde_json::Value,
    tool_blocks: &mut std::collections::HashMap<u64, String>,
    usage: &mut TokenUsage,
    stop_reason: &mut Option<String>,
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
            if let Some(reason) = event
                .get("delta")
                .and_then(|delta| delta.get("stop_reason"))
                .and_then(|reason| reason.as_str())
            {
                *stop_reason = Some(reason.to_string());
            }
            if let Some(value) = event.get("usage") {
                usage.input_tokens = value
                    .get("input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(usage.input_tokens);
                usage.output_tokens = value
                    .get("output_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(usage.output_tokens);
                usage.cache_creation_input_tokens = value
                    .get("cache_creation_input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(usage.cache_creation_input_tokens);
                usage.cache_read_input_tokens = value
                    .get("cache_read_input_tokens")
                    .and_then(|value| value.as_u64())
                    .unwrap_or(usage.cache_read_input_tokens);
            }
            None
        }
        "message_stop" => Some(StreamEvent::MessageEnd {
            usage: usage.clone(),
            stop_reason: stop_reason.clone(),
        }),
        "message_start" => {
            let value = event.get("message")?.get("usage")?;
            usage.input_tokens = value
                .get("input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(usage.input_tokens);
            usage.cache_creation_input_tokens = value
                .get("cache_creation_input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(usage.cache_creation_input_tokens);
            usage.cache_read_input_tokens = value
                .get("cache_read_input_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(usage.cache_read_input_tokens);
            None
        }
        "error" => None,
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
                ChatContent::Video {
                    media_type,
                    path,
                    filename,
                } => {
                    if !kimi {
                        anyhow::bail!("video input requires the Kimi provider");
                    }
                    let file_id =
                        upload_kimi_video(client, base_url, api_key, media_type, path, filename)
                            .await?;
                    media_parts.push(json!({
                        "type": "video_url",
                        "video_url": {"url": format!("ms://{file_id}"), "id": file_id}
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
            for tr in tool_results {
                messages.push(tr);
            }
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
        if let Some(max_tokens) = request.max_tokens {
            body["max_completion_tokens"] = json!(max_tokens);
        }
        if request.thinking.is_some() {
            body["thinking"] = json!({"type": "enabled"});
        }
    } else if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens.min(16_384));
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
        return Err(crate::response_error(resp).await);
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    // Track partial tool call argument deltas by index
    let mut tool_ids: std::collections::HashMap<usize, (String, String)> =
        std::collections::HashMap::new();
    let mut started: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut usage = TokenUsage::default();
    let mut completed = false;

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
                for index in started.drain() {
                    if let Some((id, _)) = tool_ids.get(&index) {
                        let _ = event_tx
                            .send(StreamEvent::ToolUseEnd { id: id.clone() })
                            .await;
                    }
                }
                let _ = event_tx
                    .send(StreamEvent::MessageEnd {
                        usage,
                        stop_reason: None,
                    })
                    .await;
                return Ok(());
            }
            let event = serde_json::from_str::<serde_json::Value>(data)
                .map_err(|error| anyhow::anyhow!("invalid OpenAI SSE JSON: {error}"))?;
            if let Some(error) = event.get("error") {
                let message = error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown OpenAI stream error");
                anyhow::bail!("OpenAI stream error: {message}");
            }
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
            if choice
                .get("finish_reason")
                .is_some_and(|reason| !reason.is_null())
            {
                completed = true;
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
    if completed {
        let _ = event_tx
            .send(StreamEvent::MessageEnd {
                usage,
                stop_reason: None,
            })
            .await;
        Ok(())
    } else {
        anyhow::bail!("OpenAI stream connection closed before [DONE] or finish_reason")
    }
}

pub async fn google_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    // Google Generative Language API (streamGenerateContent)
    reject_video_inputs(&request, "Google GenAI")?;
    let base = base_url.trim_end_matches('/');
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse&key={}",
        base, request.model, api_key
    );

    let tool_names: std::collections::HashMap<&str, &str> = request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|content| match content {
            ChatContent::ToolUse { id, name, .. } => Some((id.as_str(), name.as_str())),
            _ => None,
        })
        .collect();

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
                ChatContent::Video { .. } => unreachable!("video inputs rejected above"),
                ChatContent::Thinking { thinking } => parts.push(json!({"text": thinking})),
                ChatContent::ToolUse { id, name, input } => {
                    let mut part = json!({"functionCall": {"name": name, "args": input}});
                    if let Some(signature) = id.strip_prefix("google-sig:") {
                        part["thoughtSignature"] = json!(signature);
                    }
                    parts.push(part);
                }
                ChatContent::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let name = tool_names
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("tool");
                    parts.push(json!({
                        "functionResponse": {
                            "name": name,
                            "response": {"result": content},
                        }
                    }));
                }
            }
        }
        if !parts.is_empty() {
            push_strict_provider_message(&mut contents, role, "parts", parts, |part| {
                part.get("functionResponse").is_some()
            });
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
    });
    if let Some(max_tokens) = request.max_tokens {
        body["generationConfig"] = json!({"maxOutputTokens": max_tokens.min(8_192)});
    }
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
        return Err(crate::response_error(resp).await);
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut usage = TokenUsage::default();
    let mut completed = false;
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
            let event = serde_json::from_str::<serde_json::Value>(data)
                .map_err(|error| anyhow::anyhow!("invalid Google SSE JSON: {error}"))?;
            if let Some(error) = event.get("error") {
                let message = error
                    .get("message")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown Google stream error");
                anyhow::bail!("Google stream error: {message}");
            }
            if let Some(value) = event.get("usageMetadata") {
                usage.input_tokens = value
                    .get("promptTokenCount")
                    .and_then(|token| token.as_u64())
                    .unwrap_or(usage.input_tokens);
                usage.output_tokens = value
                    .get("candidatesTokenCount")
                    .and_then(|token| token.as_u64())
                    .unwrap_or(usage.output_tokens);
                usage.cache_read_input_tokens = value
                    .get("cachedContentTokenCount")
                    .and_then(|token| token.as_u64())
                    .unwrap_or(usage.cache_read_input_tokens);
            }
            let Some(cands) = event.get("candidates").and_then(|c| c.as_array()) else {
                continue;
            };
            let Some(candidate) = cands.first() else {
                continue;
            };
            completed |= candidate
                .get("finishReason")
                .and_then(|reason| reason.as_str())
                .is_some();
            let Some(parts) = candidate
                .get("content")
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
                    let id = part
                        .get("thoughtSignature")
                        .and_then(|value| value.as_str())
                        .map(|signature| format!("google-sig:{signature}"))
                        .unwrap_or_else(|| format!("google-{}", uuid::Uuid::new_v4()));
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
    if completed {
        let _ = event_tx
            .send(StreamEvent::MessageEnd {
                usage,
                stop_reason: None,
            })
            .await;
        Ok(())
    } else {
        anyhow::bail!("Google stream connection closed before finishReason")
    }
}

/// Anthropic and Gemini/Vertex require strictly alternating user/model turns.
/// Consecutive user turns naturally occur after compaction and when steer input
/// follows a tool result, so normalize them at the provider boundary while
/// preserving the provider-agnostic session history.
///
/// The merge is asymmetric like Kimi's: a tool-result-only user turn absorbs a
/// following user turn, and a text user turn absorbs another non-tool user turn.
/// A text turn never absorbs a following tool-result-only turn because that
/// result must remain adjacent to its preceding assistant tool use.
fn push_strict_provider_message(
    messages: &mut Vec<serde_json::Value>,
    role: &str,
    parts_key: &str,
    parts: Vec<serde_json::Value>,
    is_tool_result: impl Fn(&serde_json::Value) -> bool,
) {
    let current_is_tool_result_only = !parts.is_empty() && parts.iter().all(&is_tool_result);
    if role == "user" {
        if let Some(previous) = messages.last_mut().filter(|previous| {
            previous.get("role").and_then(serde_json::Value::as_str) == Some("user")
        }) {
            if let Some(previous_parts) = previous
                .get_mut(parts_key)
                .and_then(serde_json::Value::as_array_mut)
            {
                let previous_is_tool_result_only =
                    !previous_parts.is_empty() && previous_parts.iter().all(&is_tool_result);
                if previous_is_tool_result_only || !current_is_tool_result_only {
                    previous_parts.extend(parts);
                    return;
                }
            }
        }
    }
    messages.push(json!({"role": role, (parts_key): parts}));
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

pub(crate) fn reject_video_inputs(request: &LlmRequest, provider: &str) -> anyhow::Result<()> {
    if request.messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|content| matches!(content, ChatContent::Video { .. }))
    }) {
        anyhow::bail!("{provider} provider does not support local video uploads");
    }
    Ok(())
}

async fn upload_kimi_video(
    client: &Client,
    base_url: &str,
    api_key: &str,
    media_type: &str,
    path: &str,
    filename: &str,
) -> anyhow::Result<String> {
    let bytes = tokio::fs::read(path).await?;
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str(media_type)?;
    let response = client
        .post(api_endpoint(base_url, "files"))
        .bearer_auth(api_key)
        .multipart(
            reqwest::multipart::Form::new()
                .text("purpose", "video")
                .part("file", part),
        )
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(crate::response_error(response).await);
    }
    let body = response.text().await?;
    serde_json::from_str::<serde_json::Value>(&body)?
        .get("id")
        .and_then(|id| id.as_str())
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("Kimi video upload response did not contain a file id"))
}

#[cfg(test)]
mod tests {
    use super::{
        anthropic_stream, google_stream, kimi_stream, openai_stream, push_strict_provider_message,
    };
    use crate::types::{ChatContent, ChatMessage, LlmRequest, StreamEvent, ThinkingParams};
    use reqwest::Client;
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{mpsc, oneshot},
    };

    struct CapturedRequest {
        head: String,
        body: String,
    }

    async fn capture_request(socket: &mut tokio::net::TcpStream) -> CapturedRequest {
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
        CapturedRequest {
            head: String::from_utf8_lossy(&bytes[..header_end]).into_owned(),
            body: String::from_utf8_lossy(&bytes[header_end + 4..]).into_owned(),
        }
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
            let captured = capture_request(&mut socket).await;
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
            max_tokens: Some(128),
            system: Some("be helpful".into()),
            thinking: None,
        }
    }

    #[test]
    fn google_merges_steer_after_user_role_tool_result() {
        let mut contents = vec![json!({
            "role": "user",
            "parts": [{"functionResponse": {"name": "Read", "response": {"result": "ok"}}}],
        })];

        push_strict_provider_message(
            &mut contents,
            "user",
            "parts",
            vec![json!({"text": "steer guidance"})],
            |part| part.get("functionResponse").is_some(),
        );
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);

        push_strict_provider_message(
            &mut contents,
            "model",
            "parts",
            vec![json!({"text": "done"})],
            |part| part.get("functionResponse").is_some(),
        );
        assert_eq!(contents.len(), 2);
    }

    #[test]
    fn anthropic_merges_steer_after_user_role_tool_result() {
        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"}],
        })];

        push_strict_provider_message(
            &mut messages,
            "user",
            "content",
            vec![json!({"type": "text", "text": "steer guidance"})],
            |part| part.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"),
        );
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["content"].as_array().unwrap().len(), 2);

        push_strict_provider_message(
            &mut messages,
            "assistant",
            "content",
            vec![json!({"type": "text", "text": "done"})],
            |part| part.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"),
        );
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn strict_role_merge_does_not_move_tool_results_after_text() {
        let mut messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": "prompt"}],
        })];

        push_strict_provider_message(
            &mut messages,
            "user",
            "content",
            vec![json!({"type": "tool_result", "tool_use_id": "tool-1", "content": "ok"})],
            |part| part.get("type").and_then(serde_json::Value::as_str) == Some("tool_result"),
        );

        assert_eq!(messages.len(), 2);
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
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}\n",
            "data: {\"type\":\"message_stop\"}\n"
        );
        let (base_url, captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, mut rx) = mpsc::channel(16);
        let mut request = request();
        request.thinking = Some(ThinkingParams {
            budget_tokens: 10_000,
            adaptive: true,
            effort: Some("high".into()),
        });
        anthropic_stream(&Client::new(), &base_url, "secret", request, tx)
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(matches!(&events[0], StreamEvent::TextDelta(text) if text == "hi"));
        assert!(events.iter().any(|event| matches!(event, StreamEvent::ToolUseStart { id, name } if id == "call-1" && name == "Read")));
        assert!(events.iter().any(|event| matches!(event, StreamEvent::MessageEnd { usage, stop_reason } if usage.input_tokens == 4 && usage.output_tokens == 2 && stop_reason.as_deref() == Some("tool_use"))));
        let captured = captured.await.unwrap();
        let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
        assert!(body["thinking"].get("budget_tokens").is_none());
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
    async fn openai_orders_tool_result_before_followup_image() {
        let sse = "data: [DONE]\n";
        let (base_url, captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let mut request = request();
        request.messages.push(ChatMessage {
            role: "assistant".into(),
            content: vec![ChatContent::ToolUse {
                id: "call-image".into(),
                name: "ReadMediaFile".into(),
                input: serde_json::json!({"path":"screen.png"}),
            }],
        });
        request.messages.push(ChatMessage {
            role: "user".into(),
            content: vec![
                ChatContent::ToolResult {
                    tool_use_id: "call-image".into(),
                    content: "Image attached.".into(),
                    is_error: false,
                },
                ChatContent::Image {
                    media_type: "image/jpeg".into(),
                    data: "AQID".into(),
                },
            ],
        });
        let (tx, _rx) = mpsc::channel(8);
        openai_stream(&Client::new(), &base_url, "token", request, tx)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&captured.await.unwrap().body).unwrap();
        assert_eq!(body["messages"][3]["role"], "tool");
        assert_eq!(body["messages"][4]["role"], "user");
        assert_eq!(body["messages"][4]["content"][0]["type"], "image_url");
    }

    #[tokio::test]
    async fn openai_rejects_truncated_streams() {
        let sse = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n";
        let (base_url, _captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, _rx) = mpsc::channel(8);
        let error = openai_stream(&Client::new(), &base_url, "token", request(), tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("connection closed"));
    }

    #[tokio::test]
    async fn google_requires_finish_reason_and_reports_usage() {
        let sse = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hello\"}]}}]}\n",
            "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2}}\n"
        );
        let (base_url, captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let mut request = request();
        request.messages.push(ChatMessage {
            role: "assistant".into(),
            content: vec![ChatContent::ToolUse {
                id: "google-sig:opaque-signature".into(),
                name: "Read".into(),
                input: serde_json::json!({"path": "README.md"}),
            }],
        });
        request.messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::ToolResult {
                tool_use_id: "google-sig:opaque-signature".into(),
                content: "contents".into(),
                is_error: false,
            }],
        });
        let (tx, mut rx) = mpsc::channel(8);
        google_stream(&Client::new(), &base_url, "google-key", request, tx)
            .await
            .unwrap();
        assert!(matches!(rx.recv().await, Some(StreamEvent::TextDelta(text)) if text == "hello"));
        assert!(
            matches!(rx.recv().await, Some(StreamEvent::MessageEnd { usage, .. }) if usage.input_tokens == 5 && usage.output_tokens == 2)
        );
        let captured = captured.await.unwrap();
        assert!(captured.head.starts_with(
            "POST /v1beta/models/test-model:streamGenerateContent?alt=sse&key=google-key HTTP/1.1"
        ));
        let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
        assert_eq!(
            body["contents"][1]["parts"][0]["thoughtSignature"],
            "opaque-signature"
        );
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "Read"
        );
    }

    #[tokio::test]
    async fn google_rejects_truncated_streams() {
        let sse = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]}}]}\n";
        let (base_url, _captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, _rx) = mpsc::channel(8);
        let error = google_stream(&Client::new(), &base_url, "key", request(), tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("connection closed"));
    }

    #[tokio::test]
    async fn anthropic_rejects_truncated_streams() {
        let sse = "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n";
        let (base_url, _captured) = serve_once("200 OK", "text/event-stream", sse).await;
        let (tx, _rx) = mpsc::channel(8);
        let error = anthropic_stream(&Client::new(), &base_url, "token", request(), tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("connection closed"));
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
        request.thinking = Some(ThinkingParams {
            budget_tokens: 32,
            adaptive: false,
            effort: None,
        });
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
            matches!(rx.recv().await, Some(StreamEvent::MessageEnd { usage, .. }) if usage.input_tokens == 7 && usage.output_tokens == 3)
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
    async fn kimi_uploads_video_then_uses_ms_file_reference() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let (capture_tx, capture_rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut upload_socket, _) = listener.accept().await.unwrap();
            let upload = capture_request(&mut upload_socket).await;
            let upload_body = r#"{"id":"file-1"}"#;
            let upload_response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{upload_body}",
                upload_body.len()
            );
            upload_socket
                .write_all(upload_response.as_bytes())
                .await
                .unwrap();

            let (mut chat_socket, _) = listener.accept().await.unwrap();
            let chat = capture_request(&mut chat_socket).await;
            let sse = "data: [DONE]\n";
            let chat_response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                sse.len()
            );
            chat_socket
                .write_all(chat_response.as_bytes())
                .await
                .unwrap();
            let _ = capture_tx.send((upload, chat));
        });

        let video_path = std::env::temp_dir().join(format!("kkagent-{}.mp4", uuid::Uuid::new_v4()));
        std::fs::write(&video_path, b"fake-video-payload").unwrap();
        let mut request = request();
        request.messages[0].content.push(ChatContent::Video {
            media_type: "video/mp4".into(),
            path: video_path.to_string_lossy().into_owned(),
            filename: "clip.mp4".into(),
        });
        let (tx, _rx) = mpsc::channel(8);
        kimi_stream(&Client::new(), &base_url, "kimi-token", request, tx)
            .await
            .unwrap();
        let (upload, chat) = capture_rx.await.unwrap();
        assert!(upload.head.starts_with("POST /v1/files HTTP/1.1"));
        assert!(upload.body.contains("name=\"purpose\""));
        assert!(upload.body.contains("video"));
        assert!(upload.body.contains("fake-video-payload"));
        assert!(chat.head.starts_with("POST /v1/chat/completions HTTP/1.1"));
        let body: serde_json::Value = serde_json::from_str(&chat.body).unwrap();
        let content = body["messages"][1]["content"].as_array().unwrap();
        assert!(content.iter().any(|part| {
            part["type"] == "video_url" && part["video_url"]["url"] == "ms://file-1"
        }));
        std::fs::remove_file(video_path).unwrap();
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
