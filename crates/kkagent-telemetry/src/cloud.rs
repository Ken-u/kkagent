use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::privacy::clean_telemetry_properties;
use crate::service::{TelemetryAppender, TelemetryEvent};

#[derive(Clone)]
pub struct CloudAppenderOptions {
    pub endpoint: String,
    pub app_name: String,
    pub device_id: String,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub build_sha: Option<String>,
    pub ui_mode: Option<String>,
    pub get_access_token: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    pub flush_threshold: usize,
    pub flush_interval_ms: u64,
    pub request_timeout_ms: u64,
    pub spill_dir: PathBuf,
}

impl Default for CloudAppenderOptions {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self {
            endpoint: std::env::var("KKAGENT_TELEMETRY_ENDPOINT")
                .unwrap_or_else(|_| "https://telemetry.kkagent.local/v1/events".into()),
            app_name: "kkagent".into(),
            device_id: uuid::Uuid::new_v4().to_string(),
            session_id: None,
            model: None,
            build_sha: None,
            ui_mode: Some("tui".into()),
            get_access_token: None,
            flush_threshold: 50,
            flush_interval_ms: 30_000,
            request_timeout_ms: 10_000,
            spill_dir: home.join(".kkagent").join("telemetry"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct EnrichedCloudEvent {
    name: String,
    time: i64,
    properties: Map<String, Value>,
    app_name: String,
    device_id: String,
    session_id: Option<String>,
    model: Option<String>,
    build_sha: Option<String>,
    ui_mode: Option<String>,
    platform: String,
    arch: String,
}

pub struct CloudAppender {
    options: CloudAppenderOptions,
    queue: Mutex<VecDeque<EnrichedCloudEvent>>,
    client: reqwest::Client,
    flush_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl CloudAppender {
    pub fn new(options: CloudAppenderOptions) -> Arc<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(options.request_timeout_ms))
            .build()
            .unwrap_or_default();
        let this = Arc::new(Self {
            options,
            queue: Mutex::new(VecDeque::new()),
            client,
            flush_task: Mutex::new(None),
        });
        let weak = Arc::downgrade(&this);
        let interval = this.options.flush_interval_ms;
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval.max(1000)));
            loop {
                ticker.tick().await;
                let Some(strong) = weak.upgrade() else { break };
                let _ = strong.flush().await;
            }
        });
        // store handle without blocking
        let this2 = Arc::clone(&this);
        tokio::spawn(async move {
            *this2.flush_task.lock().await = Some(handle);
        });
        this
    }

    fn enrich(&self, event: TelemetryEvent) -> EnrichedCloudEvent {
        EnrichedCloudEvent {
            name: event.name,
            time: event.time_ms,
            properties: clean_telemetry_properties(event.properties),
            app_name: self.options.app_name.clone(),
            device_id: self.options.device_id.clone(),
            session_id: self.options.session_id.clone(),
            model: self.options.model.clone(),
            build_sha: self.options.build_sha.clone(),
            ui_mode: self.options.ui_mode.clone(),
            platform: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
        }
    }

    async fn spill_failed(&self, events: &[EnrichedCloudEvent]) {
        let _ = tokio::fs::create_dir_all(&self.options.spill_dir).await;
        let path = self.options.spill_dir.join(format!(
            "failed-{}.jsonl",
            chrono::Utc::now().timestamp_millis()
        ));
        let mut body = String::new();
        for e in events {
            if let Ok(line) = serde_json::to_string(e) {
                body.push_str(&line);
                body.push('\n');
            }
        }
        let _ = tokio::fs::write(path, body).await;
    }

    pub async fn flush(&self) -> anyhow::Result<()> {
        let batch: Vec<EnrichedCloudEvent> = {
            let mut q = self.queue.lock().await;
            q.drain(..).collect()
        };
        if batch.is_empty() {
            return Ok(());
        }
        let mut req = self.client.post(&self.options.endpoint).json(&batch);
        if let Some(getter) = &self.options.get_access_token {
            if let Some(token) = getter() {
                req = req.bearer_auth(token);
            }
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => {
                tracing::warn!("cloud telemetry HTTP {}", resp.status());
                self.spill_failed(&batch).await;
                Ok(())
            }
            Err(e) => {
                tracing::warn!("cloud telemetry send failed: {e}");
                self.spill_failed(&batch).await;
                Ok(())
            }
        }
    }
}

#[async_trait]
impl TelemetryAppender for CloudAppender {
    async fn append(&self, event: TelemetryEvent) {
        let enriched = self.enrich(event);
        let should_flush = {
            let mut q = self.queue.lock().await;
            q.push_back(enriched);
            q.len() >= self.options.flush_threshold
        };
        if should_flush {
            let _ = self.flush().await;
        }
    }

    async fn flush(&self) {
        let _ = CloudAppender::flush(self).await;
    }
}
