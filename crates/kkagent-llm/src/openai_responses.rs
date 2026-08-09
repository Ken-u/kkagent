//! OpenAI Responses API (`/v1/responses`) streaming adapter.

use futures_util::StreamExt;
use reqwest::Client;
use serde_json::json;
use tokio::sync::mpsc;

use crate::types::{ChatContent, LlmRequest, StreamEvent, TokenUsage};

pub async fn openai_responses_stream(
    client: &Client,
    base_url: &str,
    api_key: &str,
    request: LlmRequest,
    event_tx: mpsc::Sender<StreamEvent>,
) -> anyhow::Result<()> {
    let url = crate::stream::api_endpoint(base_url, "responses");
    crate::stream::reject_video_inputs(&request, "OpenAI Responses")?;

    let mut input: Vec<serde_json::Value> = Vec::new();
    for m in &request.messages {
        let mut texts = Vec::new();
        let mut images = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        for c in &m.content {
            match c {
                ChatContent::Text { text } => texts.push(text.clone()),
                ChatContent::Image { media_type, data } => images.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                    "detail": "auto",
                })),
                ChatContent::Video { .. } => unreachable!("video inputs rejected above"),
                ChatContent::Thinking { thinking } => texts.push(thinking.clone()),
                ChatContent::ToolUse { id, name, input } => {
                    tool_calls.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    }));
                }
                ChatContent::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    tool_results.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": content,
                    }));
                }
            }
        }
        if m.role == "assistant" {
            if !texts.is_empty() {
                input.push(json!({
                    "role": "assistant",
                    "content": texts.join("\n"),
                }));
            }
            for tc in tool_calls {
                input.push(tc);
            }
        } else if m.role == "user" {
            if !images.is_empty() {
                let mut content: Vec<serde_json::Value> = texts
                    .iter()
                    .map(|text| json!({"type": "input_text", "text": text}))
                    .collect();
                content.extend(images);
                input.push(json!({
                    "role": "user",
                    "content": content,
                }));
            } else if !texts.is_empty() {
                input.push(json!({"role": "user", "content": texts.join("\n")}));
            }
            for tr in tool_results {
                input.push(tr);
            }
        } else if !texts.is_empty() {
            input.push(json!({
                "role": m.role,
                "content": texts.join("\n"),
            }));
        }
    }

    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": &t.name,
                "description": &t.description,
                "parameters": &t.input_schema,
            })
        })
        .collect();

    let mut body = json!({
        "model": &request.model,
        "input": input,
        "max_output_tokens": request.max_tokens.min(100_000),
        "stream": true,
    });
    if let Some(sys) = &request.system {
        body["instructions"] = json!(sys);
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }
    if let Some(t) = &request.thinking {
        body["reasoning"] = json!({
            "effort": if t.budget_tokens >= 16_000 { "high" }
                      else if t.budget_tokens >= 4_000 { "medium" }
                      else { "low" }
        });
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
        anyhow::bail!("HTTP {status}: {text}");
    }

    let mut stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut usage = TokenUsage::default();
    let mut active_call: Option<(String, String)> = None; // id, name
    let mut arg_buf = String::new();

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
                flush_tool(&event_tx, &mut active_call, &mut arg_buf).await;
                let _ = event_tx.send(StreamEvent::MessageEnd { usage }).await;
                return Ok(());
            }
            let Ok(event) = serde_json::from_str::<serde_json::Value>(data) else {
                continue;
            };
            let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "response.output_text.delta" | "response.text.delta" => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        if !delta.is_empty() {
                            let _ = event_tx
                                .send(StreamEvent::TextDelta(delta.to_string()))
                                .await;
                        }
                    }
                }
                "response.reasoning_summary_text.delta" | "response.reasoning.delta" => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        let _ = event_tx
                            .send(StreamEvent::ThinkingDelta(delta.to_string()))
                            .await;
                    }
                }
                "response.function_call_arguments.delta" => {
                    if let Some(delta) = event.get("delta").and_then(|v| v.as_str()) {
                        arg_buf.push_str(delta);
                        if let Some((id, _)) = &active_call {
                            let _ = event_tx
                                .send(StreamEvent::ToolUseInputDelta {
                                    id: id.clone(),
                                    delta: delta.to_string(),
                                })
                                .await;
                        }
                    }
                }
                "response.output_item.added" => {
                    if let Some(item) = event.get("item") {
                        if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                            flush_tool(&event_tx, &mut active_call, &mut arg_buf).await;
                            let id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("call")
                                .to_string();
                            let name = item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("tool")
                                .to_string();
                            let _ = event_tx
                                .send(StreamEvent::ToolUseStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                })
                                .await;
                            active_call = Some((id, name));
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = event.get("item") {
                        if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                            if let Some(args) = item.get("arguments").and_then(|v| v.as_str()) {
                                if arg_buf.is_empty() && !args.is_empty() {
                                    if let Some((id, _)) = &active_call {
                                        let _ = event_tx
                                            .send(StreamEvent::ToolUseInputDelta {
                                                id: id.clone(),
                                                delta: args.to_string(),
                                            })
                                            .await;
                                    }
                                    arg_buf = args.to_string();
                                }
                            }
                            flush_tool(&event_tx, &mut active_call, &mut arg_buf).await;
                        }
                    }
                }
                "response.completed" => {
                    if let Some(u) = event
                        .get("response")
                        .and_then(|r| r.get("usage"))
                        .or_else(|| event.get("usage"))
                    {
                        usage.input_tokens = u
                            .get("input_tokens")
                            .or_else(|| u.get("prompt_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        usage.output_tokens = u
                            .get("output_tokens")
                            .or_else(|| u.get("completion_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        usage.cache_read_input_tokens = u
                            .get("input_tokens_details")
                            .and_then(|d| d.get("cached_tokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }
                    flush_tool(&event_tx, &mut active_call, &mut arg_buf).await;
                    let _ = event_tx
                        .send(StreamEvent::MessageEnd {
                            usage: usage.clone(),
                        })
                        .await;
                    return Ok(());
                }
                "error" | "response.failed" => {
                    let msg = event
                        .get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("responses API error");
                    let _ = event_tx.send(StreamEvent::Error(msg.to_string())).await;
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    flush_tool(&event_tx, &mut active_call, &mut arg_buf).await;
    let _ = event_tx.send(StreamEvent::MessageEnd { usage }).await;
    Ok(())
}

async fn flush_tool(
    tx: &mpsc::Sender<StreamEvent>,
    active: &mut Option<(String, String)>,
    args: &mut String,
) {
    if let Some((id, _)) = active.take() {
        let _ = tx.send(StreamEvent::ToolUseEnd { id }).await;
    }
    args.clear();
}
