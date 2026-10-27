//! Retention (D3): remove what the server already holds and the retention
//! period has passed. ONLY rows whose queue status is 'synced'. Pending,
//! waiting (failed_transient) and failed items are never touched; the open
//! shift is never touched; audit rows not yet sent are never touched.

use chrono::{Duration, Utc};
use rusqlite::params;
use serde::Serialize;

use crate::store::db::{self, ts, Store};
use crate::store::error::{Result, StoreError};

#[derive(Debug, Default, Serialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CleanupResult {
    /// Sales + other queue items + shifts + audit rows removed.
    pub removed: u64,
    pub sales: u64,
    pub queue_items: u64,
    pub shifts: u64,
    pub audit_rows: u64,
    pub stock_deltas: u64,
    pub retention_days: i64,
    pub vacuumed: bool,
}

/// Reclaim pages when more than this is free inside the file.
const VACUUM_FREE_BYTES: i64 = 64 * 1_048_576;

pub fn run(store: &mut Store, retention_days: Option<i64>, force: bool) -> Result<CleanupResult> {
    let days = match retention_days {
        Some(d) if !(1..=3650).contains(&d) => {
            return Err(StoreError::invalid(
                "retentionDays must be between 1 and 3650.",
            ))
        }
        Some(d) => d,
        None => db::retention_days(&store.conn)?,
    };
    let cutoff = ts(Utc::now() - Duration::days(days));
    let mut r = CleanupResult {
        retention_days: days,
        ..Default::default()
    };

    let tx = store.conn.transaction()?;
    // Sales (lines and payments cascade) whose upload was accepted before the cutoff.
    r.sales = tx.execute(
        "DELETE FROM sales WHERE transaction_id IN (
             SELECT transaction_id FROM outbox
             WHERE kind = 'sale' AND status = 'synced' AND synced_at < ?1)",
        [&cutoff],
    )? as u64;
    // Their stock deltas, but only once a server stock figure includes them.
    r.stock_deltas = tx.execute(
        "DELETE FROM local_stock_delta
         WHERE reconciled_at IS NOT NULL
           AND transaction_id NOT IN (SELECT transaction_id FROM sales)",
        [],
    )? as u64;
    let queue_rows = tx.execute(
        "DELETE FROM outbox WHERE status = 'synced' AND synced_at < ?1",
        [&cutoff],
    )? as u64;
    r.queue_items = queue_rows.saturating_sub(r.sales);
    // A closed shift goes once neither of its queue items nor any of its sales remain.
    r.shifts = tx.execute(
        "DELETE FROM shifts
         WHERE status = 'closed'
           AND open_transaction_id NOT IN (SELECT transaction_id FROM outbox)
           AND (close_transaction_id IS NULL OR close_transaction_id NOT IN (SELECT transaction_id FROM outbox))
           AND shift_local_id NOT IN (SELECT shift_local_id FROM sales WHERE shift_local_id IS NOT NULL)",
        [],
    )? as u64;
    r.audit_rows = tx.execute(
        "DELETE FROM audit_log WHERE sent_at IS NOT NULL AND sent_at < ?1",
        params![cutoff],
    )? as u64;
    r.removed = r.sales + r.queue_items + r.shifts + r.audit_rows;
    if r.removed > 0 {
        db::audit_insert(
            &tx,
            "local_cleanup",
            None,
            &serde_json::json!({ "removed": r.removed, "sales": r.sales, "retentionDays": days }),
        )?;
    }
    tx.commit()?;

    // Give space back to the disk: incremental when lots is free, full on force.
    let page_size: i64 = store.conn.query_row("PRAGMA page_size", [], |x| x.get(0))?;
    let free_pages: i64 = store
        .conn
        .query_row("PRAGMA freelist_count", [], |x| x.get(0))?;
    let pages: i64 = store
        .conn
        .query_row("PRAGMA page_count", [], |x| x.get(0))?;
    let free_bytes = free_pages * page_size;
    if force {
        store.conn.execute_batch("VACUUM;")?;
        r.vacuumed = true;
    } else if free_bytes > VACUUM_FREE_BYTES
        || (pages > 0 && free_pages * 4 > pages && free_bytes > 1_048_576)
    {
        store.conn.execute_batch("PRAGMA incremental_vacuum;")?;
        r.vacuumed = true;
    }
    if r.vacuumed {
        store
            .conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    }
    Ok(r)
}
