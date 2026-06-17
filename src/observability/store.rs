//! Durable per-`tools/call` metadata store backed by SQLite (`rusqlite`,
//! bundled). One row per proxied tool call, indexed for the desktop
//! Observability tab. Payload bodies live elsewhere (in-memory ring buffer,
//! R3) and are joined back by `request_uid`.
//!
//! All methods are synchronous and intended to be driven from a dedicated
//! blocking writer task (R4). The connection is guarded by a `Mutex` so the
//! same [`Store`] can be shared (e.g. `Arc<Store>`) between that writer and
//! read-only API handlers.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

/// Columns selected (and mapped) in [`Store::query`] / [`Store::get_by_request_uid`].
const SELECT_COLUMNS: &str = "id, request_uid, endpoint, server_name, \
    server_type, transport, profile, client_name, client_version, \
    client_user_agent, client_origin, tool, ts_start, ts_end, duration_ms, \
    success, error_message, request_bytes, response_bytes, streamed";

/// A single proxied `tools/call`'s metadata row. Constructed by the capture
/// pipeline (R4) for insertion and re-hydrated on query. Deliberately holds no
/// request/response bodies — those stay in the in-memory payload buffer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallRecord {
    /// Auto-increment row id. `None` when constructing for insert; populated on read.
    pub id: Option<i64>,
    /// Canonical per-message correlation key from `current_request_context()`.
    pub request_uid: Option<String>,
    pub endpoint: Option<String>,
    pub server_name: Option<String>,
    pub server_type: Option<String>,
    pub transport: Option<String>,
    pub profile: Option<String>,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub client_user_agent: Option<String>,
    pub client_origin: Option<String>,
    /// Prefixed tool name routed for this call.
    pub tool: String,
    /// Call start / end as epoch milliseconds, and the measured duration.
    pub ts_start: i64,
    pub ts_end: i64,
    pub duration_ms: i64,
    pub success: bool,
    pub error_message: Option<String>,
    pub request_bytes: i64,
    pub response_bytes: i64,
    /// `true` when the underlying response was streamed/SSE and aggregated.
    pub streamed: bool,
}

/// Filter for [`Store::query`]. All fields are ANDed; `None` means unconstrained.
/// `since`/`until` bound `ts_start` as a half-open `[since, until)` window.
#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub server_name: Option<String>,
    pub tool: Option<String>,
    pub success: Option<bool>,
    pub request_uid: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
}

/// One time bucket of aggregated metrics for sparklines. `server` is `None`
/// for the global (all-servers) series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateBucket {
    pub server: Option<String>,
    pub bucket_start: i64,
    pub count: u64,
    pub error_count: u64,
    pub p50_ms: u64,
    pub p95_ms: u64,
}

/// Outcome of [`Store::enforce_size_cap`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeCapResult {
    /// Whether any rows were evicted to fit under the cap.
    pub evicted: bool,
    pub deleted_rows: usize,
    /// Oldest `ts_start` still retained after eviction, if any rows remain.
    pub oldest_retained_ts: Option<i64>,
}

/// SQLite-backed metadata store.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Open (creating if needed) `<dir>/observability.db`, enabling WAL and
    /// incremental auto-vacuum, and idempotently create the schema.
    pub fn open(dir: &Path) -> rusqlite::Result<Self> {
        let path = dir.join("observability.db");
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Open an in-memory store (used by tests).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS calls (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_uid TEXT,
                endpoint TEXT,
                server_name TEXT,
                server_type TEXT,
                transport TEXT,
                profile TEXT,
                client_name TEXT,
                client_version TEXT,
                client_user_agent TEXT,
                client_origin TEXT,
                tool TEXT NOT NULL,
                ts_start INTEGER NOT NULL,
                ts_end INTEGER NOT NULL,
                duration_ms INTEGER NOT NULL,
                success INTEGER NOT NULL,
                error_message TEXT,
                request_bytes INTEGER NOT NULL DEFAULT 0,
                response_bytes INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS idx_calls_request_uid ON calls(request_uid);
            CREATE INDEX IF NOT EXISTS idx_calls_server_ts ON calls(server_name, ts_start);
            CREATE INDEX IF NOT EXISTS idx_calls_ts ON calls(ts_start);
            CREATE INDEX IF NOT EXISTS idx_calls_success ON calls(success);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
}

impl Store {
    /// Insert a batch of records in a single transaction. Returns the count inserted.
    pub fn insert_batch(&self, records: &[CallRecord]) -> rusqlite::Result<usize> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO calls (
                    request_uid, endpoint, server_name, server_type,
                    transport, profile, client_name, client_version,
                    client_user_agent, client_origin, tool, ts_start, ts_end,
                    duration_ms, success, error_message, request_bytes,
                    response_bytes, streamed
                ) VALUES (
                    ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                    ?14, ?15, ?16, ?17, ?18, ?19
                )",
            )?;
            for r in records {
                stmt.execute(params![
                    r.request_uid,
                    r.endpoint,
                    r.server_name,
                    r.server_type,
                    r.transport,
                    r.profile,
                    r.client_name,
                    r.client_version,
                    r.client_user_agent,
                    r.client_origin,
                    r.tool,
                    r.ts_start,
                    r.ts_end,
                    r.duration_ms,
                    r.success as i64,
                    r.error_message,
                    r.request_bytes,
                    r.response_bytes,
                    r.streamed as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(records.len())
    }

    /// Query records (newest first), filtered by `filter`, with `limit`/`offset` paging.
    pub fn query(
        &self,
        filter: &QueryFilter,
        limit: i64,
        offset: i64,
    ) -> rusqlite::Result<Vec<CallRecord>> {
        let mut sql = format!("SELECT {SELECT_COLUMNS} FROM calls WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(s) = &filter.server_name {
            sql.push_str(" AND server_name LIKE ? ESCAPE '\\'");
            args.push(Box::new(like_contains(s)));
        }
        if let Some(t) = &filter.tool {
            sql.push_str(" AND tool LIKE ? ESCAPE '\\'");
            args.push(Box::new(like_contains(t)));
        }
        if let Some(s) = filter.success {
            sql.push_str(" AND success = ?");
            args.push(Box::new(s as i64));
        }
        if let Some(u) = &filter.request_uid {
            sql.push_str(" AND request_uid = ?");
            args.push(Box::new(u.clone()));
        }
        if let Some(since) = filter.since {
            sql.push_str(" AND ts_start >= ?");
            args.push(Box::new(since));
        }
        if let Some(until) = filter.until {
            sql.push_str(" AND ts_start < ?");
            args.push(Box::new(until));
        }
        sql.push_str(" ORDER BY ts_start DESC, id DESC LIMIT ? OFFSET ?");
        args.push(Box::new(limit));
        args.push(Box::new(offset));

        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(params.as_slice(), row_to_record)?;
        rows.collect()
    }

    /// Fetch the most recent record for a `request_uid`, if any.
    pub fn get_by_request_uid(&self, uid: &str) -> rusqlite::Result<Option<CallRecord>> {
        let conn = self.conn.lock().unwrap();
        let sql =
            format!("SELECT {SELECT_COLUMNS} FROM calls WHERE request_uid = ?1 ORDER BY ts_start DESC, id DESC LIMIT 1");
        conn.query_row(&sql, params![uid], row_to_record).optional()
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CallRecord> {
    Ok(CallRecord {
        id: row.get(0)?,
        request_uid: row.get(1)?,
        endpoint: row.get(2)?,
        server_name: row.get(3)?,
        server_type: row.get(4)?,
        transport: row.get(5)?,
        profile: row.get(6)?,
        client_name: row.get(7)?,
        client_version: row.get(8)?,
        client_user_agent: row.get(9)?,
        client_origin: row.get(10)?,
        tool: row.get(11)?,
        ts_start: row.get(12)?,
        ts_end: row.get(13)?,
        duration_ms: row.get(14)?,
        success: row.get::<_, i64>(15)? != 0,
        error_message: row.get(16)?,
        request_bytes: row.get(17)?,
        response_bytes: row.get(18)?,
        streamed: row.get::<_, i64>(19)? != 0,
    })
}

/// Build a case-insensitive substring `LIKE` pattern (`%value%`) for use with
/// `ESCAPE '\'`. Backslash-escapes the LIKE metacharacters (`\`, `%`, `_`) in
/// the user-supplied value so literal occurrences match literally.
fn like_contains(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('%');
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}

impl Store {
    /// Time-bucketed metrics over `[since, until)` for sparklines. Emits one
    /// [`AggregateBucket`] per (server, bucket) plus a global series
    /// (`server == None`). `bucket_seconds` is the bucket width; buckets are
    /// aligned to multiples of the width from the epoch.
    pub fn aggregate(
        &self,
        bucket_seconds: i64,
        since: i64,
        until: i64,
    ) -> rusqlite::Result<Vec<AggregateBucket>> {
        let bucket_ms = bucket_seconds.max(1) * 1000;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT server_name, ts_start, duration_ms, success FROM calls \
             WHERE ts_start >= ?1 AND ts_start < ?2 ORDER BY ts_start ASC",
        )?;
        let rows = stmt.query_map(params![since, until], |row| {
            let server: Option<String> = row.get(0)?;
            let ts: i64 = row.get(1)?;
            let dur: i64 = row.get(2)?;
            let ok: i64 = row.get(3)?;
            Ok((server, ts, dur.max(0) as u64, ok != 0))
        })?;

        use std::collections::BTreeMap;
        // key: (server, bucket_start) -> (durations, error_count)
        let mut acc: BTreeMap<(Option<String>, i64), (Vec<u64>, u64)> = BTreeMap::new();
        for row in rows {
            let (server, ts, dur, ok) = row?;
            let bucket = (ts / bucket_ms) * bucket_ms;
            for key in [(server.clone(), bucket), (None, bucket)] {
                let entry = acc.entry(key).or_default();
                entry.0.push(dur);
                if !ok {
                    entry.1 += 1;
                }
            }
        }

        // Build populated buckets, keeping global (server == None) separate so
        // we can zero-fill it into a contiguous series spanning the window.
        let mut global: BTreeMap<i64, AggregateBucket> = BTreeMap::new();
        let mut per_server = Vec::new();
        for ((server, bucket_start), (mut durations, error_count)) in acc {
            durations.sort_unstable();
            let bucket = AggregateBucket {
                server: server.clone(),
                bucket_start,
                count: durations.len() as u64,
                error_count,
                p50_ms: percentile(&durations, 50),
                p95_ms: percentile(&durations, 95),
            };
            if server.is_none() {
                global.insert(bucket_start, bucket);
            } else {
                per_server.push(bucket);
            }
        }

        // Emit one global bucket per step across the aligned `[since, until)`
        // window, filling gaps with zeros so the sparkline draws a real
        // time-shape instead of collapsing bursts to a single point.
        let mut out = Vec::new();
        let start = (since / bucket_ms) * bucket_ms;
        let mut bucket_start = start;
        while bucket_start < until {
            out.push(global.remove(&bucket_start).unwrap_or(AggregateBucket {
                server: None,
                bucket_start,
                count: 0,
                error_count: 0,
                p50_ms: 0,
                p95_ms: 0,
            }));
            bucket_start += bucket_ms;
        }
        // Any global buckets outside the aligned window (shouldn't normally
        // occur) are appended to avoid silently dropping data.
        out.extend(global.into_values());
        out.extend(per_server);
        Ok(out)
    }

    /// Delete all records for a server (cascade on server deletion). Returns rows removed.
    pub fn delete_for_server(&self, name: &str) -> rusqlite::Result<usize> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM calls WHERE server_name = ?1", params![name])
    }

    /// Remove every record (global purge), then reclaim space.
    pub fn purge_all(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM calls", [])?;
        conn.execute_batch("PRAGMA incremental_vacuum;")?;
        Ok(())
    }

    /// Delete records whose `ts_start` is older than `days` before now. Returns rows removed.
    pub fn enforce_retention(&self, days: i64) -> rusqlite::Result<usize> {
        let cutoff = now_ms() - days.max(0) * 86_400_000;
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM calls WHERE ts_start < ?1", params![cutoff])
    }

    /// Evict oldest rows until the database fits under `max_mb`, then run an
    /// incremental vacuum. Reports whether eviction occurred and the oldest
    /// `ts_start` still retained.
    pub fn enforce_size_cap(&self, max_mb: u64) -> rusqlite::Result<SizeCapResult> {
        let max_bytes = max_mb.saturating_mul(1024 * 1024) as i64;
        let conn = self.conn.lock().unwrap();
        let mut deleted_rows = 0usize;
        let mut evicted = false;
        loop {
            let size = db_size_bytes(&conn)?;
            if size <= max_bytes {
                break;
            }
            // Delete the oldest chunk, then reclaim pages and re-measure.
            let n = conn.execute(
                "DELETE FROM calls WHERE id IN (SELECT id FROM calls ORDER BY ts_start ASC, id ASC LIMIT 256)",
                [],
            )?;
            conn.execute_batch("PRAGMA incremental_vacuum;")?;
            if n == 0 {
                break;
            }
            deleted_rows += n;
            evicted = true;
        }
        let oldest_retained_ts: Option<i64> = conn
            .query_row("SELECT MIN(ts_start) FROM calls", [], |r| r.get(0))
            .optional()?
            .flatten();
        Ok(SizeCapResult {
            evicted,
            deleted_rows,
            oldest_retained_ts,
        })
    }
}

/// Nearest-rank percentile (`p` in `0..=100`) over a pre-sorted slice.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    let rank = ((p as f64 / 100.0) * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

/// Current wall-clock time in epoch milliseconds.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Approximate on-disk size = `page_count * page_size`.
fn db_size_bytes(conn: &Connection) -> rusqlite::Result<i64> {
    let page_count: i64 = conn.pragma_query_value(None, "page_count", |r| r.get(0))?;
    let page_size: i64 = conn.pragma_query_value(None, "page_size", |r| r.get(0))?;
    Ok(page_count * page_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(server: &str, ts: i64, success: bool, duration: i64) -> CallRecord {
        CallRecord {
            request_uid: Some(format!("uid-{server}-{ts}")),
            server_name: Some(server.to_string()),
            endpoint: Some(server.to_string()),
            tool: format!("{server}__do"),
            ts_start: ts,
            ts_end: ts + duration,
            duration_ms: duration,
            success,
            request_bytes: 10,
            response_bytes: 20,
            ..Default::default()
        }
    }

    #[test]
    fn open_creates_file_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        {
            let store = Store::open(dir.path()).unwrap();
            store.insert_batch(&[rec("a", 1000, true, 5)]).unwrap();
        }
        assert!(dir.path().join("observability.db").exists());
        // Re-open: schema creation must be idempotent and data persists.
        let store = Store::open(dir.path()).unwrap();
        let rows = store.query(&QueryFilter::default(), 10, 0).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn insert_query_and_get_by_request_uid() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_batch(&[
                rec("alpha", 1000, true, 5),
                rec("beta", 2000, false, 50),
                rec("alpha", 3000, true, 15),
            ])
            .unwrap();

        // Newest-first ordering.
        let all = store.query(&QueryFilter::default(), 10, 0).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].ts_start, 3000);
        assert_eq!(all[2].ts_start, 1000);

        // Filter by server (exact name).
        let alpha = store
            .query(
                &QueryFilter {
                    server_name: Some("alpha".into()),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(alpha.len(), 2);

        // Substring (LIKE) matching: a leading fragment still matches.
        let alph = store
            .query(
                &QueryFilter {
                    server_name: Some("alph".into()),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(alph.len(), 2);

        // Mid-string fragment, case-insensitive.
        let mid = store
            .query(
                &QueryFilter {
                    server_name: Some("LPH".into()),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(mid.len(), 2);

        // Tool substring fragment (rec() builds tool = "{server}__do").
        let by_tool = store
            .query(
                &QueryFilter {
                    tool: Some("ta__".into()),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(by_tool.len(), 1);
        assert_eq!(by_tool[0].server_name.as_deref(), Some("beta"));

        // Filter by success + paging.
        let failed = store
            .query(
                &QueryFilter {
                    success: Some(false),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].server_name.as_deref(), Some("beta"));

        let page2 = store.query(&QueryFilter::default(), 1, 1).unwrap();
        assert_eq!(page2.len(), 1);
        assert_eq!(page2[0].ts_start, 2000);

        // get_by_request_uid round-trip.
        let got = store.get_by_request_uid("uid-beta-2000").unwrap().unwrap();
        assert_eq!(got.server_name.as_deref(), Some("beta"));
        assert!(!got.success);
        assert!(store.get_by_request_uid("missing").unwrap().is_none());
    }

    #[test]
    fn query_escapes_like_wildcards_in_filters() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_batch(&[
                rec("a_b", 1000, true, 5),
                rec("axb", 2000, true, 5),
                rec("c%d", 3000, true, 5),
                rec("cXYd", 4000, true, 5),
            ])
            .unwrap();

        // `_` in the value is escaped, so it matches a literal underscore only
        // — not the single-char LIKE wildcard (which would also match "axb").
        let underscore = store
            .query(
                &QueryFilter {
                    server_name: Some("a_b".into()),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(underscore.len(), 1);
        assert_eq!(underscore[0].server_name.as_deref(), Some("a_b"));

        // `%` in the value is escaped, so it matches a literal percent only —
        // not the multi-char LIKE wildcard (which would also match "cXYd").
        let percent = store
            .query(
                &QueryFilter {
                    server_name: Some("c%d".into()),
                    ..Default::default()
                },
                10,
                0,
            )
            .unwrap();
        assert_eq!(percent.len(), 1);
        assert_eq!(percent[0].server_name.as_deref(), Some("c%d"));
    }

    #[test]
    fn aggregate_buckets_with_percentiles_and_global_series() {
        let store = Store::open_in_memory().unwrap();
        // All within one 60s bucket starting at 0.
        store
            .insert_batch(&[
                rec("alpha", 1_000, true, 10),
                rec("alpha", 2_000, false, 100),
                rec("beta", 3_000, true, 30),
            ])
            .unwrap();

        let buckets = store.aggregate(60, 0, 60_000).unwrap();
        let global: Vec<_> = buckets.iter().filter(|b| b.server.is_none()).collect();
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].count, 3);
        assert_eq!(global[0].error_count, 1);
        assert_eq!(global[0].bucket_start, 0);

        let alpha: Vec<_> = buckets
            .iter()
            .filter(|b| b.server.as_deref() == Some("alpha"))
            .collect();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].count, 2);
        assert_eq!(alpha[0].error_count, 1);
        assert_eq!(alpha[0].p50_ms, 10);
        assert_eq!(alpha[0].p95_ms, 100);
    }

    #[test]
    fn aggregate_global_series_is_zero_filled_across_window() {
        let store = Store::open_in_memory().unwrap();
        // A single burst within one minute, queried over a 1-hour window.
        store
            .insert_batch(&[
                rec("alpha", 1_000, true, 10),
                rec("alpha", 2_000, false, 100),
                rec("beta", 3_000, true, 30),
            ])
            .unwrap();

        let buckets = store.aggregate(60, 0, 3_600_000).unwrap();
        let global: Vec<_> = buckets.iter().filter(|b| b.server.is_none()).collect();
        // 3_600_000 / 60_000 = 60 contiguous global buckets.
        assert_eq!(global.len(), 60);
        // Ascending, contiguous bucket starts.
        for (i, b) in global.iter().enumerate() {
            assert_eq!(b.bucket_start, i as i64 * 60_000);
        }
        // Only the first bucket is populated; the rest are zero-filled.
        assert_eq!(global[0].count, 3);
        assert_eq!(global[0].error_count, 1);
        for b in &global[1..] {
            assert_eq!(b.count, 0);
            assert_eq!(b.error_count, 0);
            assert_eq!(b.p50_ms, 0);
            assert_eq!(b.p95_ms, 0);
        }

        // Per-server buckets remain non-empty only.
        let alpha: Vec<_> = buckets
            .iter()
            .filter(|b| b.server.as_deref() == Some("alpha"))
            .collect();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].count, 2);
    }

    #[test]
    fn retention_deletes_old_rows() {
        let store = Store::open_in_memory().unwrap();
        let now = now_ms();
        let day = 86_400_000;
        store
            .insert_batch(&[
                rec("a", now - 10 * day, true, 5),
                rec("a", now - day, true, 5),
            ])
            .unwrap();
        let deleted = store.enforce_retention(7).unwrap();
        assert_eq!(deleted, 1);
        let rows = store.query(&QueryFilter::default(), 10, 0).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn size_cap_evicts_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let batch: Vec<CallRecord> = (0..2000)
            .map(|i| rec("a", 1000 + i as i64, i % 2 == 0, 5))
            .collect();
        store.insert_batch(&batch).unwrap();

        // Generous cap: nothing evicted.
        let big = store.enforce_size_cap(1024).unwrap();
        assert!(!big.evicted);
        assert_eq!(big.oldest_retained_ts, Some(1000));

        // Zero cap: evict everything (oldest-first), DB never fits.
        let zero = store.enforce_size_cap(0).unwrap();
        assert!(zero.evicted);
        assert!(zero.deleted_rows > 0);
        assert_eq!(zero.oldest_retained_ts, None);
        assert_eq!(store.query(&QueryFilter::default(), 1, 0).unwrap().len(), 0);
    }

    #[test]
    fn delete_for_server_and_purge_all() {
        let store = Store::open_in_memory().unwrap();
        store
            .insert_batch(&[
                rec("alpha", 1000, true, 5),
                rec("beta", 2000, true, 5),
                rec("alpha", 3000, true, 5),
            ])
            .unwrap();

        let removed = store.delete_for_server("alpha").unwrap();
        assert_eq!(removed, 2);
        assert_eq!(
            store.query(&QueryFilter::default(), 10, 0).unwrap().len(),
            1
        );

        store.purge_all().unwrap();
        assert_eq!(
            store.query(&QueryFilter::default(), 10, 0).unwrap().len(),
            0
        );
    }
}
