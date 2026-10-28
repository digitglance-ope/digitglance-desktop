//! The upload queue (outbox): what to send next, and what the server said.
//!
//! States: pending -> synced (accepted | duplicate), failed (failed_permanent,
//! kept forever for a person to look at), or failed_transient (retried after a
//! capped exponential backoff with jitter). Nothing here deletes a row.

use chrono::{Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::store::db::{self, now_str, ts, Store};
use crate::store::error::{Result, StoreError};

pub const BACKOFF_BASE_MS: u64 = 2_000;
pub const BACKOFF_CAP_MS: u64 = 600_000;

/// Delay before retry number `attempts` (1 = first retry): the exponential
/// step `min(cap, base * 2^(attempts-1))`, jittered down by up to 20%
/// (`jitter01` in [0, 1)). Always within [0.8 * step, step] and never above the cap.
pub fn backoff_delay_ms(attempts: u32, jitter01: f64) -> u64 {
    let exp = attempts.saturating_sub(1).min(30);
    let step = BACKOFF_BASE_MS
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_CAP_MS);
    let j = if jitter01.is_finite() {
        jitter01.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let delay = step as f64 * (1.0 - 0.2 * j);
    (delay.round() as u64).min(BACKOFF_CAP_MS)
}

fn random01() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::fill(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub seq: i64,
    pub transaction_id: String,
    pub kind: String,
    pub status: String,
    pub payload: Value,
    pub attempts: i64,
    pub next_attempt_at: Option<String>,
    pub last_error: Option<String>,
    pub sync_result: Option<String>,
    pub server_id: Option<String>,
    pub server_receipt_number: Option<String>,
    pub flags: Value,
    pub created_at: String,
    pub synced_at: Option<String>,
}

const ITEM_COLS: &str =
    "seq, transaction_id, kind, status, payload_json, attempts, next_attempt_at,
    last_error, sync_result, server_id, server_receipt_number, flags_json, created_at, synced_at";

fn item(r: &rusqlite::Row) -> rusqlite::Result<QueueItem> {
    let payload: String = r.get(4)?;
    let flags: Option<String> = r.get(11)?;
    Ok(QueueItem {
        seq: r.get(0)?,
        transaction_id: r.get(1)?,
        kind: r.get(2)?,
        status: r.get(3)?,
        payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
        attempts: r.get(5)?,
        next_attempt_at: r.get(6)?,
        last_error: r.get(7)?,
        sync_result: r.get(8)?,
        server_id: r.get(9)?,
        server_receipt_number: r.get(10)?,
        flags: flags
            .and_then(|f| serde_json::from_str(&f).ok())
            .unwrap_or(Value::Null),
        created_at: r.get(12)?,
        synced_at: r.get(13)?,
    })
}

/// Pending items, and transient failures whose wait is over, oldest first.
pub fn next_batch(store: &Store, limit: Option<i64>) -> Result<Vec<QueueItem>> {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let sql = format!(
        "SELECT {ITEM_COLS} FROM outbox WHERE seq IN (
             SELECT seq FROM (SELECT seq FROM outbox WHERE status = 'pending' ORDER BY seq LIMIT ?2)
             UNION ALL
             SELECT seq FROM (SELECT seq FROM outbox
                              WHERE status = 'failed_transient' AND next_attempt_at <= ?1
                              ORDER BY seq LIMIT ?2))
         ORDER BY seq LIMIT ?2"
    );
    let mut st = store.conn.prepare_cached(&sql)?;
    let rows = st.query_map(params![now_str(), limit], item)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct MarkResult {
    pub transaction_id: String,
    pub status: String,
    #[serde(default)]
    pub server_id: Option<String>,
    #[serde(default)]
    pub receipt_number: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub flags: Option<Value>,
}

#[derive(Debug, Serialize, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MarkSummary {
    pub synced: u64,
    pub failed: u64,
    pub retrying: u64,
    /// Results whose transactionId is not in the queue (ignored).
    pub unknown: Vec<String>,
    /// Results for items already synced (left as they were).
    pub ignored: u64,
}

pub fn mark(store: &mut Store, results: &[MarkResult]) -> Result<MarkSummary> {
    for r in results {
        match r.status.as_str() {
            "accepted" | "duplicate" | "failed_permanent" | "failed_transient" => {}
            s => return Err(StoreError::invalid(format!("Unknown result status '{s}'."))),
        }
    }
    let now = Utc::now();
    let now_s = ts(now);
    let tx = store.conn.transaction()?;
    let mut sum = MarkSummary::default();
    for r in results {
        let cur: Option<(String, String, i64)> = tx
            .query_row(
                "SELECT status, kind, attempts FROM outbox WHERE transaction_id = ?1",
                [&r.transaction_id],
                |x| Ok((x.get(0)?, x.get(1)?, x.get(2)?)),
            )
            .optional()?;
        let Some((status, kind, attempts)) = cur else {
            sum.unknown.push(r.transaction_id.clone());
            continue;
        };
        if status == "synced" {
            sum.ignored += 1;
            continue;
        }
        let flags = r
            .flags
            .as_ref()
            .filter(|f| !f.is_null())
            .map(serde_json::to_string)
            .transpose()?;
        match r.status.as_str() {
            "accepted" | "duplicate" => {
                tx.execute(
                    "UPDATE outbox SET status = 'synced', sync_result = ?2, server_id = ?3,
                         server_receipt_number = ?4, flags_json = COALESCE(?5, flags_json),
                         next_attempt_at = NULL, last_error = NULL, synced_at = ?6, updated_at = ?6
                     WHERE transaction_id = ?1",
                    params![
                        r.transaction_id,
                        r.status,
                        r.server_id,
                        r.receipt_number,
                        flags,
                        now_s
                    ],
                )?;
                if kind == "sale" || kind == "return" {
                    tx.execute(
                        "UPDATE local_stock_delta SET synced_at = ?2 WHERE transaction_id = ?1 AND synced_at IS NULL",
                        params![r.transaction_id, now_s],
                    )?;
                }
                sum.synced += 1;
            }
            "failed_permanent" => {
                tx.execute(
                    "UPDATE outbox SET status = 'failed', sync_result = 'failed_permanent',
                         last_error = ?2, flags_json = COALESCE(?3, flags_json), next_attempt_at = NULL,
                         attempts = attempts + 1, updated_at = ?4
                     WHERE transaction_id = ?1",
                    params![
                        r.transaction_id,
                        r.reason.clone().unwrap_or_else(|| "rejected".into()),
                        flags,
                        now_s
                    ],
                )?;
                db::audit_insert(
                    &tx,
                    "sync_failed_permanent",
                    None,
                    &json!({ "transactionId": r.transaction_id, "kind": kind, "reason": r.reason }),
                )?;
                sum.failed += 1;
            }
            _ => {
                let n = (attempts + 1).max(1);
                let delay = backoff_delay_ms(n as u32, random01());
                let next = ts(now + Duration::milliseconds(delay as i64));
                tx.execute(
                    "UPDATE outbox SET status = 'failed_transient', sync_result = 'failed_transient',
                         attempts = ?2, next_attempt_at = ?3, last_error = ?4,
                         flags_json = COALESCE(?5, flags_json), updated_at = ?6
                     WHERE transaction_id = ?1",
                    params![r.transaction_id, n, next, r.reason, flags, now_s],
                )?;
                sum.retrying += 1;
            }
        }
    }
    let result = if sum.failed == 0 && sum.retrying == 0 {
        "ok"
    } else if sum.synced == 0 {
        "failed"
    } else {
        "partial"
    };
    db::meta_set(&tx, "last_sync_at", &now_s)?;
    db::meta_set(&tx, "last_sync_result", result)?;
    tx.commit()?;
    Ok(sum)
}

pub fn failed_list(store: &Store) -> Result<Vec<QueueItem>> {
    let sql = format!("SELECT {ITEM_COLS} FROM outbox WHERE status = 'failed' ORDER BY seq");
    let mut st = store.conn.prepare_cached(&sql)?;
    let rows = st.query_map([], item)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Admin: put a failed (or waiting) item back to pending. The last error is kept
/// for the record until the next result replaces it.
pub fn retry_failed(store: &mut Store, transaction_id: &str) -> Result<QueueItem> {
    let tx = store.conn.transaction()?;
    let n = tx.execute(
        "UPDATE outbox SET status = 'pending', attempts = 0, next_attempt_at = NULL, updated_at = ?2
         WHERE transaction_id = ?1 AND status IN ('failed', 'failed_transient')",
        params![transaction_id, now_str()],
    )?;
    if n == 0 {
        return Err(StoreError::not_found(
            "No failed queue item with that transactionId.",
        ));
    }
    db::audit_insert(
        &tx,
        "sync_retry_requested",
        None,
        &json!({ "transactionId": transaction_id }),
    )?;
    let sql = format!("SELECT {ITEM_COLS} FROM outbox WHERE transaction_id = ?1");
    let it = tx.query_row(&sql, [transaction_id], item)?;
    tx.commit()?;
    Ok(it)
}

/// Every unsent item (pending, waiting, failed) for `export_pending`.
pub fn unsent_items(store: &Store) -> Result<Vec<QueueItem>> {
    let sql = format!("SELECT {ITEM_COLS} FROM outbox WHERE status <> 'synced' ORDER BY seq");
    let mut st = store.conn.prepare_cached(&sql)?;
    let rows = st.query_map([], item)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------- status and export

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreStatus {
    pub pending_count: i64,
    pub failed_count: i64,
    pub synced_count: i64,
    pub oldest_pending_at: Option<String>,
    pub last_sync_at: Option<String>,
    pub last_sync_result: Option<String>,
    pub db_size_mb: f64,
    pub storage: crate::store::storage::StorageInfo,
    pub retention_days: i64,
}

/// `pendingCount` counts everything not yet on the server (pending and
/// waiting-to-retry); `failedCount` counts permanent failures.
pub fn status(store: &Store) -> Result<StoreStatus> {
    // Index-only counts (outbox_status covers status), so this stays cheap with
    // tens of thousands of rows.
    let mut pending = 0;
    let mut failed = 0;
    let mut synced = 0;
    {
        let mut st = store
            .conn
            .prepare_cached("SELECT status, count(*) FROM outbox GROUP BY status")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let n: i64 = r.get(1)?;
            match r.get::<_, String>(0)?.as_str() {
                "pending" | "failed_transient" => pending += n,
                "failed" => failed += n,
                "synced" => synced += n,
                _ => {}
            }
        }
    }
    let oldest: Option<String> = store
        .conn
        .query_row(
            "SELECT created_at FROM outbox WHERE seq = (
                 SELECT min(seq) FROM outbox WHERE status IN ('pending', 'failed_transient'))",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(StoreStatus {
        pending_count: pending,
        failed_count: failed,
        synced_count: synced,
        oldest_pending_at: oldest,
        last_sync_at: db::meta_get(&store.conn, "last_sync_at")?,
        last_sync_result: db::meta_get(&store.conn, "last_sync_result")?,
        db_size_mb: store.db_size_mb(),
        storage: crate::store::storage::measure(store)?,
        retention_days: db::retention_days(&store.conn)?,
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportResult {
    pub path: String,
    pub items: u64,
    pub bytes: u64,
}

/// Write every unsent queue item (pending, waiting, failed) to a PLAIN JSON
/// file for support, e.g. before uninstalling (an MSIX uninstall deletes the
/// app's data). The file contains sales data and is NOT encrypted.
pub fn export_pending(store: &mut Store, path: &str) -> Result<ExportResult> {
    let p = std::path::Path::new(path);
    if !p.is_absolute() {
        return Err(StoreError::invalid("path must be absolute."));
    }
    if p.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("json"))
        != Some(true)
    {
        return Err(StoreError::invalid("path must end in .json."));
    }
    if p.is_dir() {
        return Err(StoreError::invalid("path is a folder."));
    }
    let items = unsent_items(store)?;
    let identity: Option<(String, Option<String>, Option<String>)> = store
        .conn
        .query_row(
            "SELECT device_code, branch_id, terminal_id FROM device_identity WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let doc = json!({
        "format": "digitglance-till-export",
        "version": 1,
        "warning": "This file contains sales data (amounts, items, customer ids) and is not encrypted. \
                    Give it only to DigitGlance support or the business owner.",
        "exportedAt": now_str(),
        "ownerId": store.owner_id,
        "deviceId": store.device_id,
        "deviceCode": identity.as_ref().map(|i| i.0.clone()),
        "branchId": identity.as_ref().and_then(|i| i.1.clone()),
        "terminalId": identity.as_ref().and_then(|i| i.2.clone()),
        "count": items.len(),
        "items": items,
    });
    let body = serde_json::to_vec_pretty(&doc)?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = p.as_os_str().to_owned();
    tmp.push(".partial");
    let tmp = std::path::PathBuf::from(tmp);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)?;
    db::audit_insert(
        &store.conn,
        "pending_exported",
        None,
        &json!({ "items": items.len() }),
    )?;
    Ok(ExportResult {
        path: p.display().to_string(),
        items: items.len() as u64,
        bytes: body.len() as u64,
    })
}
