use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct TelemetryEvent {
    pub name: String,
    pub time_ms: i64,
    pub properties: Map<String, Value>,
}

#[async_trait]
pub trait TelemetryAppender: Send + Sync {
    async fn append(&self, event: TelemetryEvent);
    async fn flush(&self) {}
}

pub struct ConsoleAppender;

#[async_trait]
impl TelemetryAppender for ConsoleAppender {
    async fn append(&self, event: TelemetryEvent) {
        tracing::debug!(
            target: "kkagent.telemetry",
            event = %event.name,
            props = %serde_json::Value::Object(event.properties),
            "telemetry"
        );
    }
}

pub struct FileAppender {
    path: std::path::PathBuf,
    lock: Mutex<()>,
}

impl FileAppender {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: Mutex::new(()),
        }
    }
}

#[async_trait]
impl TelemetryAppender for FileAppender {
    async fn append(&self, event: TelemetryEvent) {
        let _g = self.lock.lock().await;
        if let Some(parent) = self.path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let line = serde_json::json!({
            "name": event.name,
            "time": event.time_ms,
            "properties": event.properties,
        });
        if let Ok(s) = serde_json::to_string(&line) {
            use tokio::io::AsyncWriteExt;
            if let Ok(mut f) = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
                .await
            {
                let _ = f.write_all(s.as_bytes()).await;
                let _ = f.write_all(b"\n").await;
            }
        }
    }
}

pub struct TelemetryService {
    appenders: Mutex<Vec<Arc<dyn TelemetryAppender>>>,
    context: Mutex<Map<String, Value>>,
}

pub type TelemetryServiceHandle = Arc<TelemetryService>;

impl TelemetryService {
    pub fn new() -> TelemetryServiceHandle {
        Arc::new(Self {
            appenders: Mutex::new(Vec::new()),
            context: Mutex::new(Map::new()),
        })
    }

    pub async fn add_appender(&self, appender: Arc<dyn TelemetryAppender>) {
        self.appenders.lock().await.push(appender);
    }

    pub async fn set_context(&self, key: impl Into<String>, value: Value) {
        self.context.lock().await.insert(key.into(), value);
    }

    pub async fn track(&self, name: impl Into<String>, mut properties: Map<String, Value>) {
        {
            let ctx = self.context.lock().await;
            for (k, v) in ctx.iter() {
                properties.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
        let event = TelemetryEvent {
            name: name.into(),
            time_ms: chrono::Utc::now().timestamp_millis(),
            properties,
        };
        let appenders = self.appenders.lock().await.clone();
        for a in appenders {
            a.append(event.clone()).await;
        }
    }

    pub async fn track_json(&self, name: impl Into<String>, props: Value) {
        let map = props.as_object().cloned().unwrap_or_default();
        self.track(name, map).await;
    }

    pub async fn flush(&self) {
        let appenders = self.appenders.lock().await.clone();
        for a in appenders {
            a.flush().await;
        }
    }
}

impl Default for TelemetryService {
    fn default() -> Self {
        Self {
            appenders: Mutex::new(Vec::new()),
            context: Mutex::new(Map::new()),
        }
    }
}
