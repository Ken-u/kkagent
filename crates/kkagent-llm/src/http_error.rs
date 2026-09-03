use std::error::Error as StdError;
use std::time::{Duration, SystemTime};

use reqwest::{header::HeaderMap, Response, StatusCode};

const MAX_RETRY_AFTER: Duration = Duration::from_secs(300);
const MAX_ERROR_BODY_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("API returned HTTP {status}: {body}")]
pub struct LlmHttpError {
    pub status: StatusCode,
    pub body: String,
    pub retry_after: Option<Duration>,
}

impl LlmHttpError {
    pub fn is_rate_limited(&self) -> bool {
        self.status == StatusCode::TOO_MANY_REQUESTS
    }
}

/// Canonical error-message marker for first-token timeouts. All matching sites
/// (agent_loop classification, stream tests) must use this constant instead of
/// re-typing the literal, so the Display text and detection stay in lockstep.
pub const FIRST_TOKEN_TIMEOUT_MARKER: &str = "first token timeout:";

/// Streaming request did not receive a meaningful first content chunk in time.
#[derive(Debug, thiserror::Error)]
#[error("{FIRST_TOKEN_TIMEOUT_MARKER} no content received within {timeout_ms}ms for model {model}")]
pub struct FirstTokenTimeoutError {
    pub timeout_ms: u64,
    pub model: String,
}

impl FirstTokenTimeoutError {
    pub fn is_retryable(&self) -> bool {
        true
    }
}

/// Flatten a `reqwest::Error` into a single message that keeps the status,
/// error-kind flags and the full source chain. The bare Display text (e.g.
/// "error decoding response body") hides whether the response body was bad
/// JSON or the connection was cut mid-stream, so we unwrap it here.
pub fn detailed_reqwest_error(error: &reqwest::Error) -> String {
    let mut message = error.to_string();
    let mut details = Vec::new();
    if let Some(url) = error.url() {
        details.push(format!("url={url}"));
    }
    if let Some(status) = error.status() {
        details.push(format!("status={status}"));
    }
    let kinds = [
        (error.is_decode(), "decode"),
        (error.is_body(), "body"),
        (error.is_request(), "request"),
        (error.is_connect(), "connect"),
        (error.is_timeout(), "timeout"),
    ];
    details.extend(
        kinds
            .into_iter()
            .filter(|(flag, _)| *flag)
            .map(|(_, label)| format!("kind={label}")),
    );
    if !details.is_empty() {
        message.push_str(&format!(" [{}]", details.join(", ")));
    }
    let mut source = StdError::source(error);
    while let Some(err) = source {
        message.push_str(&format!(": {err}"));
        source = err.source();
    }
    message
}

/// Convert a `reqwest::Error` into an `anyhow::Error` carrying the flattened
/// detail chain, and log it so transport failures stay visible in traces.
pub fn reqwest_error(error: reqwest::Error) -> anyhow::Error {
    let detail = detailed_reqwest_error(&error);
    tracing::warn!("LLM transport error: {detail}");
    anyhow::anyhow!(detail)
}

pub async fn response_error(response: Response) -> anyhow::Error {
    let status = response.status();
    let headers = response.headers().clone();
    let body = truncate_body(response.text().await.unwrap_or_default());
    let retry_after = retry_after_hint(&headers, &body, SystemTime::now());
    LlmHttpError {
        status,
        body,
        retry_after,
    }
    .into()
}

pub fn stream_error_event(error: &anyhow::Error) -> crate::types::StreamEvent {
    if let Some(http_error) = error.downcast_ref::<LlmHttpError>() {
        if http_error.is_rate_limited() {
            return crate::types::StreamEvent::RateLimited {
                message: error.to_string(),
                retry_after: http_error.retry_after,
            };
        }
    }
    crate::types::StreamEvent::Error(error.to_string())
}

fn retry_after_hint(headers: &HeaderMap, body: &str, now: SystemTime) -> Option<Duration> {
    let header_delay = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after(value, now))
        .or_else(|| {
            ["x-ratelimit-reset-after", "ratelimit-reset-after"]
                .iter()
                .find_map(|name| header_seconds(headers, name))
        })
        .or_else(|| {
            ["x-ratelimit-reset", "ratelimit-reset"]
                .iter()
                .find_map(|name| header_reset(headers, name, now))
        });
    header_delay
        .or_else(|| json_retry_after(body))
        .map(|delay| delay.min(MAX_RETRY_AFTER))
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    parse_seconds(value).or_else(|| {
        httpdate::parse_http_date(value.trim())
            .ok()
            .map(|deadline| deadline.duration_since(now).unwrap_or(Duration::ZERO))
    })
}

fn header_seconds(headers: &HeaderMap, name: &str) -> Option<Duration> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_seconds)
}

fn header_reset(headers: &HeaderMap, name: &str, now: SystemTime) -> Option<Duration> {
    let value = headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    if value >= 1_000_000_000.0 {
        let deadline = SystemTime::UNIX_EPOCH + Duration::from_secs_f64(value);
        Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
    } else {
        Some(Duration::from_secs_f64(value))
    }
}

fn parse_seconds(value: &str) -> Option<Duration> {
    let seconds = value
        .trim()
        .trim_end_matches('s')
        .trim()
        .parse::<f64>()
        .ok()?;
    (seconds.is_finite() && seconds >= 0.0).then(|| Duration::from_secs_f64(seconds))
}

fn json_retry_after(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    find_json_retry_after(&value)
}

fn find_json_retry_after(value: &serde_json::Value) -> Option<Duration> {
    match value {
        serde_json::Value::Object(object) => {
            for (key, value) in object {
                let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
                if matches!(
                    normalized.as_str(),
                    "retryafter" | "retryafterseconds" | "retrydelay"
                ) {
                    let parsed = value
                        .as_f64()
                        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
                        .map(Duration::from_secs_f64)
                        .or_else(|| value.as_str().and_then(parse_seconds));
                    if parsed.is_some() {
                        return parsed;
                    }
                }
            }
            object.values().find_map(find_json_retry_after)
        }
        serde_json::Value::Array(values) => values.iter().find_map(find_json_retry_after),
        _ => None,
    }
}

fn truncate_body(mut body: String) -> String {
    if body.len() <= MAX_ERROR_BODY_BYTES {
        return body;
    }
    let mut boundary = MAX_ERROR_BODY_BYTES;
    while !body.is_char_boundary(boundary) {
        boundary -= 1;
    }
    body.truncate(boundary);
    body.push('…');
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderValue, RETRY_AFTER};

    #[test]
    fn retry_after_seconds_takes_precedence_over_json() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("12"));
        let delay = retry_after_hint(
            &headers,
            r#"{"error":{"retry_after":30}}"#,
            SystemTime::UNIX_EPOCH,
        );
        assert_eq!(delay, Some(Duration::from_secs(12)));
    }

    #[test]
    fn parses_http_date_and_nested_provider_json() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let deadline = now + Duration::from_secs(20);
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&httpdate::fmt_http_date(deadline)).unwrap(),
        );
        assert_eq!(
            retry_after_hint(&headers, "", now),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            retry_after_hint(
                &HeaderMap::new(),
                r#"{"error":{"details":[{"retryDelay":"2.5s"}]}}"#,
                now,
            ),
            Some(Duration::from_millis(2_500))
        );
    }

    #[test]
    fn excessive_retry_delay_is_capped() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("3600"));
        assert_eq!(
            retry_after_hint(&headers, "", SystemTime::UNIX_EPOCH),
            Some(MAX_RETRY_AFTER)
        );
    }

    #[test]
    fn rate_limit_error_becomes_structured_stream_event() {
        let error: anyhow::Error = LlmHttpError {
            status: StatusCode::TOO_MANY_REQUESTS,
            body: "limited".into(),
            retry_after: Some(Duration::from_secs(9)),
        }
        .into();

        assert!(matches!(
            stream_error_event(&error),
            crate::types::StreamEvent::RateLimited {
                retry_after: Some(delay),
                ..
            } if delay == Duration::from_secs(9)
        ));
    }

    #[test]
    fn first_token_timeout_display_and_detection() {
        let error: anyhow::Error = FirstTokenTimeoutError {
            timeout_ms: 1500,
            model: "demo".into(),
        }
        .into();
        assert_eq!(
            error.to_string(),
            "first token timeout: no content received within 1500ms for model demo"
        );
        assert!(matches!(
            stream_error_event(&error),
            crate::types::StreamEvent::Error(message)
                if message.contains(FIRST_TOKEN_TIMEOUT_MARKER)
        ));
    }

    #[test]
    fn detailed_reqwest_error_keeps_source_chain() {
        // Spin up a minimal server that advertises a chunked body but closes
        // the connection mid-transfer, producing a real reqwest decode error.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().unwrap().0;
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut conn, &mut buf);
            let response = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhell";
            let _ = std::io::Write::write_all(&mut conn, response);
        });

        let error: Option<reqwest::Error> = rt.block_on(async move {
            let client = reqwest::Client::new();
            client
                .get(format!("http://{addr}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .err()
        });
        server.join().unwrap();

        let error = error.expect("truncated chunked body should yield a decode error");
        let message = detailed_reqwest_error(&error);
        assert!(message.contains("kind=decode"), "{message}");
        // The flattened source chain must explain *why* decoding failed
        // (incomplete message body), which the bare Display text hides.
        assert!(
            message.contains("error decoding response body") || message.contains("body"),
            "{message}"
        );
    }

    #[test]
    fn reqwest_error_wraps_detail() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = std::thread::spawn(move || {
            let mut conn = listener.accept().unwrap().0;
            let mut buf = [0u8; 1024];
            let _ = std::io::Read::read(&mut conn, &mut buf);
            let response = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhell";
            let _ = std::io::Write::write_all(&mut conn, response);
        });

        let message = rt.block_on(async move {
            let client = reqwest::Client::new();
            match client
                .get(format!("http://{addr}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
            {
                Ok(_) => unreachable!("truncated chunked body should fail"),
                Err(error) => reqwest_error(error).to_string(),
            }
        });
        server.join().unwrap();

        assert!(message.contains("kind=decode"), "{message}");
    }
}
