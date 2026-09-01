//! Durable, cross-session token usage history.
//!
//! Every LLM call lands in `usage_events` (one row per call) and is folded
//! into `usage_daily` (one row per `(day, model, location)`). The web
//! dashboard reads only `usage_daily` for the 30-day charts; `usage_events`
//! keeps session-level drill-down for the last 30 days and is trimmed by
//! [`UsageStore::cleanup_expired`] so the database cannot grow unbounded.
//!
//! Writes go through a process-wide sink ([`set_global`]) so every call
//! site — main turns, nested subagents, compaction summaries, the goal
//! judge — records without each having to thread a store handle through.
//! This mirrors the audit trail's global-writer pattern.

use std::sync::Arc;
use std::sync::OnceLock;

use chrono::{Datelike, Utc};
use rusqlite::Connection;
use serde::Serialize;

use crate::transcript::db::SharedSqlite;

/// Per-day retention for `usage_events` (drill-down rows). `usage_daily`
/// aggregates are kept forever — they are bounded by days × models × sites.
pub const EVENT_RETENTION_DAYS: i64 = 30;

/// One LLM call's token usage, attributed to a model at a call site.
#[derive(Debug, Clone, Serialize)]
pub struct UsageEvent {
    /// RFC 3339 timestamp of the call.
    pub ts: String,
    /// Local calendar day (`YYYY-MM-DD`) used for daily aggregation.
    pub day: String,
    pub session_id: String,
    pub model: String,
    /// `main` | `subagent` | `compaction` | `judge`.
    pub location: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// One `(day, model, location)` aggregate as stored in / read from
/// `usage_daily`.
#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    pub day: String,
    pub model: String,
    pub location: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl DailyUsage {
    /// Provider-normalized effective input (cache buckets folded in).
    pub fn total_input_tokens(&self) -> u64 {
        // Anthropic-style rows (input excludes cache buckets) are detected
        // the same way as `TokenUsage`: a row never mixes semantics within
        // one model, and cache_creation > 0 implies Anthropic reporting.
        if self.cache_creation_input_tokens > 0 {
            self.input_tokens
                .saturating_add(self.cache_creation_input_tokens)
                .saturating_add(self.cache_read_input_tokens)
        } else {
            self.input_tokens
        }
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_input_tokens().saturating_add(self.output_tokens)
    }
}

/// Per-session totals derived from the retention-bounded event table.
#[derive(Debug, Clone, Serialize)]
pub struct SessionUsage {
    pub session_id: String,
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Local calendar day for "now" — the dashboard's day boundaries follow the
/// user's clock, not UTC.
pub fn local_day_string() -> String {
    let now = chrono::Local::now();
    format!("{:04}-{:02}-{:02}", now.year(), now.month(), now.day())
}

pub(crate) fn open_tables(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS usage_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts TEXT NOT NULL,
            day TEXT NOT NULL,
            session_id TEXT NOT NULL,
            model TEXT NOT NULL,
            location TEXT NOT NULL,
            input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            cache_creation_input_tokens INTEGER NOT NULL,
            cache_read_input_tokens INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_events_day ON usage_events(day);
        CREATE INDEX IF NOT EXISTS idx_usage_events_session ON usage_events(session_id);

        CREATE TABLE IF NOT EXISTS usage_daily (
            day TEXT NOT NULL,
            model TEXT NOT NULL,
            location TEXT NOT NULL,
            calls INTEGER NOT NULL DEFAULT 0,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (day, model, location)
        );",
    )
}

/// Durable usage history on top of the shared transcript SQLite handle.
#[derive(Clone)]
pub struct UsageStore {
    conn: SharedSqlite,
}

impl UsageStore {
    pub fn from_shared(conn: SharedSqlite) -> anyhow::Result<Self> {
        {
            let guard = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("transcript db lock poisoned"))?;
            open_tables(&guard)?;
        }
        Ok(Self { conn })
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Arc::new(std::sync::Mutex::new(
            Connection::open_in_memory()
                .map_err(|e| anyhow::anyhow!("open in-memory usage db: {e}"))?,
        ));
        {
            let guard = conn
                .lock()
                .map_err(|_| anyhow::anyhow!("usage db lock poisoned"))?;
            open_tables(&guard)?;
        }
        Ok(Self { conn })
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|_| anyhow::anyhow!("transcript db lock poisoned"))
    }

    /// Record one call: an event row plus the daily aggregate upsert, in a
    /// single transaction. Failures are logged and swallowed by the global
    /// sink — usage bookkeeping must never break the LLM turn.
    pub fn record(&self, event: &UsageEvent) -> anyhow::Result<()> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction()
            .map_err(|e| anyhow::anyhow!("usage tx begin: {e}"))?;
        tx.execute(
            "INSERT INTO usage_events
                (ts, day, session_id, model, location,
                 input_tokens, output_tokens,
                 cache_creation_input_tokens, cache_read_input_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                event.ts,
                event.day,
                event.session_id,
                event.model,
                event.location,
                event.input_tokens as i64,
                event.output_tokens as i64,
                event.cache_creation_input_tokens as i64,
                event.cache_read_input_tokens as i64,
            ],
        )
        .map_err(|e| anyhow::anyhow!("usage event insert: {e}"))?;
        tx.execute(
            "INSERT INTO usage_daily
                (day, model, location, calls, input_tokens, output_tokens,
                 cache_creation_input_tokens, cache_read_input_tokens)
             VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)
             ON CONFLICT (day, model, location) DO UPDATE SET
                calls = calls + 1,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_input_tokens =
                    cache_creation_input_tokens + excluded.cache_creation_input_tokens,
                cache_read_input_tokens =
                    cache_read_input_tokens + excluded.cache_read_input_tokens",
            rusqlite::params![
                event.day,
                event.model,
                event.location,
                event.input_tokens as i64,
                event.output_tokens as i64,
                event.cache_creation_input_tokens as i64,
                event.cache_read_input_tokens as i64,
            ],
        )
        .map_err(|e| anyhow::anyhow!("usage daily upsert: {e}"))?;
        tx.commit()
            .map_err(|e| anyhow::anyhow!("usage tx commit: {e}"))?;
        Ok(())
    }

    /// Daily aggregates for the last `days` days (by day descending, then
    /// volume). Used for drill-down tables; charts use [`Self::totals_by_day`].
    pub fn query_daily(&self, days: i64) -> anyhow::Result<Vec<DailyUsage>> {
        let cutoff = cutoff_day(days);
        let conn = self.lock()?;
        let rows = read_daily_filtered(&conn, &cutoff)?;
        Ok(rows)
    }

    /// Per-model totals over the last `days` days.
    pub fn totals_by_model(&self, days: i64) -> anyhow::Result<Vec<DailyUsage>> {
        let cutoff = cutoff_day(days);
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT '' AS day, model, '' AS location, SUM(calls), SUM(input_tokens),
                    SUM(output_tokens), SUM(cache_creation_input_tokens),
                    SUM(cache_read_input_tokens)
             FROM usage_daily WHERE day >= ?1
             GROUP BY model ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
        )?;
        let rows = stmt
            .query_map([cutoff], map_total_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("usage by-model query: {e}"))?;
        Ok(rows)
    }

    /// Per-location totals over the last `days` days.
    pub fn totals_by_location(&self, days: i64) -> anyhow::Result<Vec<DailyUsage>> {
        let cutoff = cutoff_day(days);
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT '' AS day, '' AS model, location, SUM(calls), SUM(input_tokens),
                    SUM(output_tokens), SUM(cache_creation_input_tokens),
                    SUM(cache_read_input_tokens)
             FROM usage_daily WHERE day >= ?1
             GROUP BY location ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC",
        )?;
        let rows = stmt
            .query_map([cutoff], map_total_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("usage by-location query: {e}"))?;
        Ok(rows)
    }

    /// Day-level totals (all models/locations collapsed) for the last `days`
    /// days, ascending by day — chart-ready.
    pub fn totals_by_day(&self, days: i64) -> anyhow::Result<Vec<DailyUsage>> {
        let cutoff = cutoff_day(days);
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT day, '' AS model, '' AS location, SUM(calls), SUM(input_tokens),
                    SUM(output_tokens), SUM(cache_creation_input_tokens),
                    SUM(cache_read_input_tokens)
             FROM usage_daily WHERE day >= ?1
             GROUP BY day ORDER BY day ASC",
        )?;
        let rows = stmt
            .query_map([cutoff], map_total_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("usage by-day query: {e}"))?;
        Ok(rows)
    }

    /// Delete event rows older than the retention window; daily aggregates
    /// are kept forever. Returns the number of removed rows.
    pub fn cleanup_expired(&self) -> anyhow::Result<usize> {
        let cutoff = cutoff_day(EVENT_RETENTION_DAYS);
        let conn = self.lock()?;
        let removed = conn
            .execute("DELETE FROM usage_events WHERE day < ?1", [cutoff])
            .map_err(|e| anyhow::anyhow!("usage cleanup: {e}"))?;
        Ok(removed)
    }

    /// Per-session totals over the last `days` days, from the event table
    /// (bounded by the retention window). Only the dashboard's drill-down
    /// uses this; aggregates above cover the unlimited horizon.
    pub fn sessions_from_events(&self, days: i64) -> anyhow::Result<Vec<SessionUsage>> {
        let cutoff = cutoff_day(days);
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, COUNT(*), SUM(input_tokens), SUM(output_tokens),
                    SUM(cache_creation_input_tokens), SUM(cache_read_input_tokens)
             FROM usage_events WHERE day >= ?1
             GROUP BY session_id
             ORDER BY SUM(input_tokens) + SUM(output_tokens) DESC
             LIMIT 500",
        )?;
        let rows = stmt
            .query_map([cutoff], |row| {
                Ok(SessionUsage {
                    session_id: row.get(0)?,
                    calls: row.get::<_, i64>(1)?.max(0) as u64,
                    input_tokens: row.get::<_, i64>(2)?.max(0) as u64,
                    output_tokens: row.get::<_, i64>(3)?.max(0) as u64,
                    cache_creation_input_tokens: row.get::<_, i64>(4)?.max(0) as u64,
                    cache_read_input_tokens: row.get::<_, i64>(5)?.max(0) as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("usage per-session query: {e}"))?;
        Ok(rows)
    }
}

fn cutoff_day(days: i64) -> String {
    let cutoff = Utc::now() - chrono::Duration::days(days.saturating_sub(1));
    format!(
        "{:04}-{:02}-{:02}",
        cutoff.year(),
        cutoff.month(),
        cutoff.day()
    )
}

fn map_total_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DailyUsage> {
    Ok(DailyUsage {
        day: row.get(0)?,
        model: row.get(1)?,
        location: row.get(2)?,
        calls: row.get::<_, i64>(3)?.max(0) as u64,
        input_tokens: row.get::<_, i64>(4)?.max(0) as u64,
        output_tokens: row.get::<_, i64>(5)?.max(0) as u64,
        cache_creation_input_tokens: row.get::<_, i64>(6)?.max(0) as u64,
        cache_read_input_tokens: row.get::<_, i64>(7)?.max(0) as u64,
    })
}

/// Simple SELECT used by [`UsageStore::query_daily`] (kept separate so the
/// caller can hold the lock only for the read).
fn read_daily_filtered(conn: &Connection, cutoff: &str) -> rusqlite::Result<Vec<DailyUsage>> {
    let mut stmt = conn.prepare(
        "SELECT day, model, location, calls, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens
         FROM usage_daily WHERE day >= ?1
         ORDER BY day DESC,
                  input_tokens + output_tokens DESC,
                  model ASC, location ASC",
    )?;
    let rows = stmt
        .query_map([cutoff], map_total_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

// --- Process-wide sink -------------------------------------------------

static GLOBAL_STORE: OnceLock<UsageStore> = OnceLock::new();

/// Install the process-wide usage sink. Later calls are no-ops (first wins).
pub fn set_global(store: UsageStore) {
    let _ = GLOBAL_STORE.set(store);
}

/// Handle to the installed sink, for readers (HTTP API). Holds the shared
/// SQLite handle, not the connection lock.
pub fn global_snapshot() -> Option<UsageStore> {
    GLOBAL_STORE.get().cloned()
}

/// Record through the global sink when installed. Failures are logged and
/// swallowed; test builds never write (same policy as the audit trail).
pub fn try_record(event: UsageEvent) {
    if cfg!(test) {
        return;
    }
    let Some(store) = GLOBAL_STORE.get() else {
        return;
    };
    if let Err(error) = store.record(&event) {
        tracing::warn!("usage record failed: {error:#}");
    }
}

/// Best-effort startup cleanup of expired event rows.
pub fn cleanup_expired_global() {
    if let Some(store) = GLOBAL_STORE.get() {
        match store.cleanup_expired() {
            Ok(removed) if removed > 0 => {
                tracing::info!("usage history: trimmed {removed} expired event row(s)")
            }
            Ok(_) => {}
            Err(error) => tracing::warn!("usage cleanup failed: {error:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> UsageStore {
        UsageStore::open_in_memory().expect("in-memory usage store")
    }

    fn event(day: &str, model: &str, location: &str, input: u64, output: u64) -> UsageEvent {
        UsageEvent {
            ts: format!("{day}T00:00:00Z"),
            day: day.to_string(),
            session_id: "s1".into(),
            model: model.into(),
            location: location.into(),
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }
    }

    #[test]
    fn record_folds_into_daily_aggregates() {
        let s = store();
        // Relative days around today so the 30-day window always contains
        // them regardless of when the test runs.
        let today = local_day_string();
        let minus2 = day_offset(-2);
        let day1 = today.clone();
        let day2 = today.clone();
        s.record(&event(&minus2, "m1", "main", 100, 10)).unwrap();
        s.record(&event(&minus2, "m1", "main", 200, 20)).unwrap();
        s.record(&event(&minus2, "m2", "subagent", 5, 1)).unwrap();
        s.record(&event(&day2, "m1", "main", 7, 7)).unwrap();

        let daily = s.query_daily(30).unwrap();
        assert_eq!(daily.len(), 3);
        // Newest day first.
        assert_eq!(daily[0].day, day1);
        // Same-day rows sort by volume desc.
        assert_eq!(daily[1].model, "m1");
        assert_eq!(daily[1].calls, 2);
        assert_eq!(daily[1].input_tokens, 300);
        assert_eq!(daily[1].total_tokens(), 330);

        let by_model = s.totals_by_model(30).unwrap();
        assert_eq!(by_model.len(), 2);
        assert_eq!(by_model[0].model, "m1");
        assert_eq!(by_model[0].calls, 3);

        let by_location = s.totals_by_location(30).unwrap();
        assert_eq!(by_location.len(), 2);
        assert_eq!(by_location[0].location, "main");

        let by_day = s.totals_by_day(30).unwrap();
        assert_eq!(by_day.len(), 2);
        assert_eq!(by_day[0].day, minus2);
        assert_eq!(by_day[0].input_tokens, 305);

        // Window filtering: only rows within the last 1 day remain.
        assert!(s.query_daily(1).unwrap().iter().all(|r| r.day >= {
            let t = Utc::now() - chrono::Duration::hours(23);
            format!("{:04}-{:02}-{:02}", t.year(), t.month(), t.day())
        }));
    }

    fn day_offset(days: i64) -> String {
        let d = Utc::now() + chrono::Duration::days(days);
        format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
    }

    #[test]
    fn cleanup_trims_only_expired_events() {
        let s = store();
        s.record(&event("2020-01-01", "m1", "main", 1, 1)).unwrap();
        s.record(&event("2020-01-02", "m1", "main", 1, 1)).unwrap();
        s.record(&event(&local_day_string(), "m1", "main", 1, 1))
            .unwrap();
        let removed = s.cleanup_expired().unwrap();
        assert_eq!(removed, 2);
        // Daily aggregates survive the trim.
        assert_eq!(s.query_daily(40000).unwrap().len(), 3);
    }
}
