//! Local audit trail, uploaded to `POST /pos/devices/events` (kinds match the
//! server's `^[a-z0-9_.-]{2,64}$`). Rows are kept until sent, then until retention.

use rusqlite::params;
use serde::Serialize;
use serde_json::Value;

use crate::store::db::{self, now_str, Store};
use crate::store::error::{Result, StoreError};

pub fn valid_kind(kind: &str) -> bool {
    (2..=64).contains(&kind.len())
        && kind.bytes().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'.' || c == b'-'
        })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub id: i64,
    pub kind: String,
    pub acting_user_id: Option<String>,
    pub device_time: String,
    pub detail: Value,
}

pub fn append(
    store: &Store,
    kind: &str,
    detail: &Value,
    acting_user_id: Option<&str>,
) -> Result<i64> {
    if !valid_kind(kind) {
        return Err(StoreError::invalid(
            "Audit kind must match ^[a-z0-9_.-]{2,64}$.",
        ));
    }
    let detail = if detail.is_null() {
        Value::Object(Default::default())
    } else {
        detail.clone()
    };
    if serde_json::to_string(&detail)?.len() > 16_384 {
        return Err(StoreError::invalid("Audit detail is limited to 16 KB."));
    }
    db::audit_insert(&store.conn, kind, acting_user_id, &detail)
}

/// Oldest unsent first, at most 200 (the server's batch limit).
pub fn take_unsent(store: &Store, limit: Option<i64>) -> Result<Vec<AuditEvent>> {
    let limit = limit.unwrap_or(200).clamp(1, 200);
    let mut st = store.conn.prepare_cached(
        "SELECT id, kind, acting_user_id, device_time, detail_json FROM audit_log
         WHERE sent_at IS NULL ORDER BY id LIMIT ?1",
    )?;
    let rows = st.query_map([limit], |r| {
        let d: String = r.get(4)?;
        Ok(AuditEvent {
            id: r.get(0)?,
            kind: r.get(1)?,
            acting_user_id: r.get(2)?,
            device_time: r.get(3)?,
            detail: serde_json::from_str(&d).unwrap_or(Value::Null),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn mark_sent(store: &mut Store, ids: &[i64]) -> Result<u64> {
    let now = now_str();
    let tx = store.conn.transaction()?;
    let mut n = 0;
    {
        let mut st = tx.prepare_cached(
            "UPDATE audit_log SET sent_at = ?2 WHERE id = ?1 AND sent_at IS NULL",
        )?;
        for id in ids {
            n += st.execute(params![id, now])? as u64;
        }
    }
    tx.commit()?;
    Ok(n)
}
