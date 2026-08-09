use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::migration::WIRE_PROTOCOL_VERSION;

pub const AGENT_WIRE_RECORD_KEY: &str = "wire.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireRecord {
    #[serde(rename = "type")]
    pub record_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<i64>,
    #[serde(flatten)]
    pub fields: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireMetadataRecord {
    #[serde(rename = "type")]
    pub record_type: String,
    pub protocol_version: String,
    pub created_at: i64,
}

pub fn is_wire_record(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|o| o.get("type"))
        .and_then(|t| t.as_str())
        .is_some()
}

pub fn create_wire_metadata_record(now_ms: i64) -> WireMetadataRecord {
    WireMetadataRecord {
        record_type: "metadata".into(),
        protocol_version: WIRE_PROTOCOL_VERSION.into(),
        created_at: now_ms,
    }
}

pub fn is_wire_metadata_record(record: &WireRecord) -> bool {
    record.record_type == "metadata"
        && record
            .fields
            .get("protocol_version")
            .and_then(|v| v.as_str())
            .is_some()
        && record
            .fields
            .get("created_at")
            .and_then(|v| v.as_i64())
            .is_some()
}

pub fn op_to_wire_record(op_type: &str, payload: Value, now_ms: i64) -> WireRecord {
    let mut fields = Map::new();
    match payload {
        Value::Object(map) => fields = map,
        other => {
            fields.insert("payload".into(), other);
        }
    }
    if !fields.contains_key("time") {
        fields.insert("time".into(), Value::from(now_ms));
    }
    let time = fields.get("time").and_then(|v| v.as_i64());
    WireRecord {
        record_type: op_type.into(),
        time,
        fields,
    }
}

pub fn wire_record_to_payload(record: &WireRecord) -> Value {
    if record.fields.len() == 1 && record.fields.contains_key("payload") {
        return record.fields.get("payload").cloned().unwrap_or(Value::Null);
    }
    Value::Object(record.fields.clone())
}

impl WireRecord {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.fields.get(key)
    }

    pub fn to_value(&self) -> Value {
        let mut map = self.fields.clone();
        map.insert("type".into(), Value::String(self.record_type.clone()));
        if let Some(t) = self.time {
            map.insert("time".into(), Value::from(t));
        }
        Value::Object(map)
    }

    pub fn from_value(value: Value) -> Option<Self> {
        let mut obj = value.as_object()?.clone();
        let record_type = obj.remove("type")?.as_str()?.to_string();
        let time = obj.remove("time").and_then(|v| v.as_i64());
        Some(Self {
            record_type,
            time,
            fields: obj,
        })
    }
}
