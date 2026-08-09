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
    let mut active_calls = std::collections::HashMap::<String, ActiveCall>::new();

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
                anyhow::bail!("OpenAI Responses stream ended before response.completed");
            }
            let event = serde_json::from_str::<serde_json::Value>(data)
                .map_err(|error| anyhow::anyhow!("invalid OpenAI Responses SSE JSON: {error}"))?;
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
                        let key = response_item_key(&event, None);
                        if let Some(call) = key.and_then(|key| active_calls.get_mut(&key)) {
                            call.saw_arguments = true;
                            let _ = event_tx
                                .send(StreamEvent::ToolUseInputDelta {
                                    id: call.id.clone(),
                                    delta: delta.to_string(),
                                })
                                .await;
                        }
                    }
                }
                "response.output_item.added" => {
                    if let Some(item) = event.get("item") {
                        if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
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
                            let key =
                                response_item_key(&event, Some(item)).unwrap_or_else(|| id.clone());
                            active_calls.insert(
                                key,
                                ActiveCall {
                                    id,
                                    saw_arguments: false,
                                },
                            );
                        }
                    }
                }
                "response.output_item.done" => {
                    if let Some(item) = event.get("item") {
                        if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                            let key = response_item_key(&event, Some(item));
                            if let Some(mut call) = key.and_then(|key| active_calls.remove(&key)) {
                                if let Some(args) =
                                    item.get("arguments").and_then(|value| value.as_str())
                                {
                                    if !call.saw_arguments && !args.is_empty() {
                                        let _ = event_tx
                                            .send(StreamEvent::ToolUseInputDelta {
                                                id: call.id.clone(),
                                                delta: args.to_string(),
                                            })
                                            .await;
                                        call.saw_arguments = true;
                                    }
                                }
                                let _ =
                                    event_tx.send(StreamEvent::ToolUseEnd { id: call.id }).await;
                            }
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
                    flush_tools(&event_tx, &mut active_calls).await;
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
                        .or_else(|| {
                            event
                                .get("response")
                                .and_then(|response| response.get("error"))
                        })
                        .and_then(|e| e.get("message"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("responses API error");
                    anyhow::bail!("OpenAI Responses stream error: {msg}");
                }
                _ => {}
            }
        }
    }
    anyhow::bail!("OpenAI Responses stream connection closed before response.completed")
}

struct ActiveCall {
    id: String,
    saw_arguments: bool,
}

fn response_item_key(
    event: &serde_json::Value,
    item: Option<&serde_json::Value>,
) -> Option<String> {
    event
        .get("output_index")
        .and_then(|value| value.as_u64())
        .map(|index| format!("index:{index}"))
        .or_else(|| {
            event
                .get("item_id")
                .and_then(|value| value.as_str())
                .map(|id| format!("item:{id}"))
        })
        .or_else(|| {
            item.and_then(|item| item.get("id"))
                .and_then(|value| value.as_str())
                .map(|id| format!("item:{id}"))
        })
        .or_else(|| {
            item.and_then(|item| item.get("call_id"))
                .and_then(|value| value.as_str())
                .map(|id| format!("call:{id}"))
        })
}

async fn flush_tools(
    tx: &mpsc::Sender<StreamEvent>,
    active: &mut std::collections::HashMap<String, ActiveCall>,
) {
    for (_, call) in active.drain() {
        let _ = tx.send(StreamEvent::ToolUseEnd { id: call.id }).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatMessage, ToolDef};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn request() -> LlmRequest {
        LlmRequest {
            model: "gpt-test".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "hello".into(),
                }],
            }],
            tools: vec![ToolDef {
                name: "Read".into(),
                description: "read".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: 128,
            system: None,
            thinking: None,
        }
    }

    async fn serve_sse(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut bytes = [0_u8; 4096];
                let count = socket.read(&mut bytes).await.unwrap();
                request.extend_from_slice(&bytes[..count]);
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn preserves_parallel_function_call_boundaries() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"id\":\"item-a\",\"call_id\":\"a\",\"name\":\"One\"}}\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"item-b\",\"call_id\":\"b\",\"name\":\"Two\"}}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"delta\":\"{\\\"y\\\":2}\"}\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":0,\"delta\":\"{\\\"x\\\":1}\"}\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"a\",\"arguments\":\"{\\\"x\\\":1}\"}}\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"call_id\":\"b\",\"arguments\":\"{\\\"y\\\":2}\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":4,\"output_tokens\":2}}}\n"
        );
        let base_url = serve_sse(sse).await;
        let (tx, mut rx) = mpsc::channel(32);
        openai_responses_stream(&Client::new(), &base_url, "token", request(), tx)
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

    #[tokio::test]
    async fn rejects_truncated_streams() {
        let base_url =
            serve_sse("data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n")
                .await;
        let (tx, _rx) = mpsc::channel(8);
        let error = openai_responses_stream(&Client::new(), &base_url, "token", request(), tx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("connection closed"));
    }
}
