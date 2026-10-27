//! Free space on the volume holding the DB, and the ok / warning / critical
//! state from the business's thresholds (D11). Critical storage is the only
//! thing that stops a sale.

use std::path::Path;

use rusqlite::Connection;
use serde::Serialize;

use crate::store::db::{self, round2, Store};
use crate::store::error::Result;

/// Returns (free bytes available to this user, total bytes) for a path's volume.
pub type StorageProbe = Box<dyn Fn(&Path) -> std::io::Result<(u64, u64)> + Send>;

/// Windows: GetDiskFreeSpaceExW (via fs4) on the DB's directory.
pub fn os_probe(path: &Path) -> std::io::Result<(u64, u64)> {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    Ok((fs4::available_space(dir)?, fs4::total_space(dir)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageState {
    Ok,
    Warning,
    Critical,
}

impl StorageState {
    pub fn as_str(&self) -> &'static str {
        match self {
            StorageState::Ok => "ok",
            StorageState::Warning => "warning",
            StorageState::Critical => "critical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Thresholds {
    pub warning_pct: f64,
    pub warning_mb: f64,
    pub critical_pct: f64,
    pub critical_mb: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            warning_pct: 15.0,
            warning_mb: 500.0,
            critical_pct: 5.0,
            critical_mb: 150.0,
        }
    }
}

/// From `offlineSettings` (storageWarningPct, storageWarningMb,
/// storageCriticalPct, storageCriticalMb), falling back to the defaults.
pub fn thresholds(conn: &Connection) -> Result<Thresholds> {
    let d = Thresholds::default();
    let get = |f: &str, dflt: f64| -> Result<f64> {
        Ok(db::offline_setting_f64(conn, f)?
            .filter(|v| v.is_finite() && *v >= 0.0)
            .unwrap_or(dflt))
    };
    Ok(Thresholds {
        warning_pct: get("storageWarningPct", d.warning_pct)?,
        warning_mb: get("storageWarningMb", d.warning_mb)?,
        critical_pct: get("storageCriticalPct", d.critical_pct)?,
        critical_mb: get("storageCriticalMb", d.critical_mb)?,
    })
}

/// At or below EITHER the percentage OR the MB figure (as the server does).
pub fn classify(free_bytes: u64, total_bytes: u64, t: &Thresholds) -> StorageState {
    let free_mb = free_bytes as f64 / 1_048_576.0;
    let free_pct = if total_bytes == 0 {
        0.0
    } else {
        free_bytes as f64 * 100.0 / total_bytes as f64
    };
    if free_pct <= t.critical_pct || free_mb <= t.critical_mb {
        StorageState::Critical
    } else if free_pct <= t.warning_pct || free_mb <= t.warning_mb {
        StorageState::Warning
    } else {
        StorageState::Ok
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageInfo {
    pub free_mb: f64,
    pub total_mb: f64,
    pub free_pct: f64,
    pub state: StorageState,
}

pub fn measure(store: &Store) -> Result<StorageInfo> {
    let (free, total) = (store.probe)(&store.path)?;
    let t = thresholds(&store.conn)?;
    Ok(StorageInfo {
        free_mb: round2(free as f64 / 1_048_576.0),
        total_mb: round2(total as f64 / 1_048_576.0),
        free_pct: if total == 0 {
            0.0
        } else {
            round2(free as f64 * 100.0 / total as f64)
        },
        state: classify(free, total, &t),
    })
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCheck {
    #[serde(flatten)]
    pub storage: StorageInfo,
    /// Rows removed by the cleanup this check triggered (0 when none ran).
    pub cleanup_removed: u64,
    pub cleanup_ran: bool,
}

/// `storage_check`: measure; at or below warning run the retention cleanup and
/// measure again. Audits a change of state (not every tick).
pub fn check(store: &mut Store) -> Result<StorageCheck> {
    let before = measure(store)?;
    let mut removed = 0;
    let mut ran = false;
    let after = if before.state != StorageState::Ok {
        removed = crate::store::cleanup::run(store, None, false)?.removed;
        ran = true;
        measure(store)?
    } else {
        before
    };
    if store.last_storage_state != Some(after.state) {
        if store.last_storage_state.is_some() || after.state != StorageState::Ok {
            db::audit_insert(
                &store.conn,
                &format!("storage_{}", after.state.as_str()),
                None,
                &serde_json::json!({ "freeMb": after.free_mb, "freePct": after.free_pct,
                                     "cleanupRemoved": removed }),
            )?;
        }
        store.last_storage_state = Some(after.state);
    }
    Ok(StorageCheck {
        storage: after,
        cleanup_removed: removed,
        cleanup_ran: ran,
    })
}
