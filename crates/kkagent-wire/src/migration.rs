use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::record::WireRecord;

pub const WIRE_PROTOCOL_VERSION: &str = "1.5";

#[derive(Debug, Error)]
pub enum WireError {
    #[error("missing wire migration for version {0}")]
    MigrationMissing(String),
    #[error("wire version newer than supported: {0}")]
    NewerVersion(String),
}

pub type WireMigrationRecord = WireRecord;

pub trait WireMigration: Send + Sync {
    fn source_version(&self) -> &'static str;
    fn target_version(&self) -> &'static str;
    fn migrate_record(&self, record: WireMigrationRecord) -> WireMigrationRecord;
}

struct MigrateV10ToV11;
struct MigrateV11ToV12;
struct MigrateV12ToV13;
struct MigrateV13ToV14;
struct MigrateV14ToV15;

impl WireMigration for MigrateV10ToV11 {
    fn source_version(&self) -> &'static str {
        "1.0"
    }
    fn target_version(&self) -> &'static str {
        "1.1"
    }
    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        if record.record_type != "context.append_message" {
            return record;
        }
        let Some(message) = record.fields.get_mut("message") else {
            return record;
        };
        let Some(msg_obj) = message.as_object_mut() else {
            return record;
        };
        let Some(tool_calls) = msg_obj.get_mut("toolCalls") else {
            return record;
        };
        let Some(arr) = tool_calls.as_array_mut() else {
            return record;
        };
        for tc in arr.iter_mut() {
            let Some(obj) = tc.as_object_mut() else {
                continue;
            };
            if let Some(func) = obj.remove("function") {
                if let Some(f) = func.as_object() {
                    if let Some(name) = f.get("name") {
                        obj.insert("name".into(), name.clone());
                    }
                    if let Some(args) = f.get("arguments") {
                        obj.insert("arguments".into(), args.clone());
                    }
                }
            }
        }
        record
    }
}

impl WireMigration for MigrateV11ToV12 {
    fn source_version(&self) -> &'static str {
        "1.1"
    }
    fn target_version(&self) -> &'static str {
        "1.2"
    }
    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        if record.record_type != "permission.record_approval_result" {
            return record;
        }
        let decision = record
            .fields
            .get("result")
            .and_then(|r| r.get("decision"))
            .and_then(|d| d.as_str())
            .unwrap_or("");
        if decision != "approved" {
            return record;
        }
        let action = record
            .fields
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let pattern = match action.as_str() {
            "run command" => Some("Bash"),
            "stop background task" => Some("TaskOutput"),
            "edit file" | "edit file outside of working directory" | "write file" => Some("Write"),
            "run command in plan mode" | "run background command" => None,
            _ => None,
        };
        if let Some(p) = pattern {
            record
                .fields
                .insert("sessionApprovalRule".into(), Value::String(p.into()));
        }
        record
    }
}

impl WireMigration for MigrateV12ToV13 {
    fn source_version(&self) -> &'static str {
        "1.2"
    }
    fn target_version(&self) -> &'static str {
        "1.3"
    }
    fn migrate_record(&self, record: WireMigrationRecord) -> WireMigrationRecord {
        record
    }
}

impl WireMigration for MigrateV13ToV14 {
    fn source_version(&self) -> &'static str {
        "1.3"
    }
    fn target_version(&self) -> &'static str {
        "1.4"
    }
    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        // Flatten goal.* accounting fields into goal.update envelope.
        match record.record_type.as_str() {
            "goal.account_usage" => {
                let mut fields = Map::new();
                fields.insert(
                    "goalId".into(),
                    record.fields.get("goalId").cloned().unwrap_or(Value::Null),
                );
                fields.insert("status".into(), json!("active"));
                if let Some(t) = record.fields.get("tokensUsed") {
                    fields.insert("tokensUsed".into(), t.clone());
                }
                if let Some(w) = record.fields.get("wallClockMs") {
                    fields.insert("wallClockMs".into(), w.clone());
                }
                if let Some(t) = record.time {
                    fields.insert("time".into(), Value::from(t));
                }
                WireRecord {
                    record_type: "goal.update".into(),
                    time: record.time,
                    fields,
                }
            }
            "goal.continuation" => {
                let mut fields = record.fields.clone();
                fields.insert("status".into(), json!("active"));
                record.record_type = "goal.update".into();
                record.fields = fields;
                record
            }
            _ => record,
        }
    }
}

impl WireMigration for MigrateV14ToV15 {
    fn source_version(&self) -> &'static str {
        "1.4"
    }
    fn target_version(&self) -> &'static str {
        "1.5"
    }
    fn migrate_record(&self, mut record: WireMigrationRecord) -> WireMigrationRecord {
        let advances = record.record_type == "goal.create"
            || (record.record_type == "goal.update"
                && (record.fields.get("status").and_then(|s| s.as_str()) == Some("active")
                    || (record.fields.get("status").is_none()
                        && record.fields.get("wallClockMs").is_some())));
        if !advances {
            return record;
        }
        if record.fields.contains_key("wallClockResumedAt") {
            return record;
        }
        if let Some(t) = record.time {
            record
                .fields
                .insert("wallClockResumedAt".into(), Value::from(t));
        }
        record
    }
}

static MIGRATE_1_0_TO_1_1: MigrateV10ToV11 = MigrateV10ToV11;
static MIGRATE_1_1_TO_1_2: MigrateV11ToV12 = MigrateV11ToV12;
static MIGRATE_1_2_TO_1_3: MigrateV12ToV13 = MigrateV12ToV13;
static MIGRATE_1_3_TO_1_4: MigrateV13ToV14 = MigrateV13ToV14;
static MIGRATE_1_4_TO_1_5: MigrateV14ToV15 = MigrateV14ToV15;

fn migrations() -> Vec<&'static dyn WireMigration> {
    vec![
        &MIGRATE_1_0_TO_1_1,
        &MIGRATE_1_1_TO_1_2,
        &MIGRATE_1_2_TO_1_3,
        &MIGRATE_1_3_TO_1_4,
        &MIGRATE_1_4_TO_1_5,
    ]
}

fn compare_wire_versions(a: &str, b: &str) -> i32 {
    let parse = |s: &str| -> (u32, u32) {
        let mut parts = s.split('.');
        let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor)
    };
    let (am, ai) = parse(a);
    let (bm, bi) = parse(b);
    if am != bm {
        return match am.cmp(&bm) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        };
    }
    match ai.cmp(&bi) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

pub fn is_newer_wire_version(read_version: &str) -> bool {
    compare_wire_versions(read_version, WIRE_PROTOCOL_VERSION) > 0
}

pub fn resolve_wire_migrations(
    read_version: &str,
) -> Result<Vec<&'static dyn WireMigration>, WireError> {
    if compare_wire_versions(read_version, WIRE_PROTOCOL_VERSION) >= 0 {
        return Ok(Vec::new());
    }
    let all = migrations();
    let mut out = Vec::new();
    let mut version = read_version.to_string();
    while compare_wire_versions(&version, WIRE_PROTOCOL_VERSION) < 0 {
        let found = all.iter().find(|m| m.source_version() == version);
        let Some(m) = found else {
            return Err(WireError::MigrationMissing(version));
        };
        version = m.target_version().to_string();
        out.push(*m);
    }
    Ok(out)
}

pub fn migrate_wire_record(
    record: WireMigrationRecord,
    migrations: &[&dyn WireMigration],
) -> WireMigrationRecord {
    migrations
        .iter()
        .fold(record, |cur, m| m.migrate_record(cur))
}

pub fn migrate_wire_records(
    records: Vec<WireMigrationRecord>,
    read_version: Option<&str>,
) -> Result<Vec<WireMigrationRecord>, WireError> {
    let migrations = match read_version {
        Some(v) => {
            if is_newer_wire_version(v) {
                return Err(WireError::NewerVersion(v.into()));
            }
            resolve_wire_migrations(v)?
        }
        None => migrations(),
    };
    Ok(records
        .into_iter()
        .map(|r| migrate_wire_record(r, &migrations))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn migrate_tool_calls_v1_0() {
        let record = WireRecord::from_value(json!({
            "type": "context.append_message",
            "message": {
                "toolCalls": [{
                    "type": "function",
                    "id": "1",
                    "function": { "name": "Bash", "arguments": "{}" }
                }]
            }
        }))
        .unwrap();
        let migrated = migrate_wire_records(vec![record], Some("1.0")).unwrap();
        let tc = &migrated[0].fields["message"]["toolCalls"][0];
        assert_eq!(tc["name"], "Bash");
        assert!(tc.get("function").is_none());
    }

    #[test]
    fn migrate_to_1_5_sets_wall_clock() {
        let record = WireRecord::from_value(json!({
            "type": "goal.create",
            "time": 12345,
            "goalId": "g1",
            "objective": "x"
        }))
        .unwrap();
        let migrated = migrate_wire_records(vec![record], Some("1.4")).unwrap();
        assert_eq!(migrated[0].fields["wallClockResumedAt"], 12345);
    }
}
