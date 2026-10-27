//! Opening the encrypted database, and the small helpers every module shares.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::store::crypto_key::{self, to_hex, DbKey, KeyStore};
use crate::store::error::{Result, StoreError};
use crate::store::migrations;
use crate::store::storage::{self, StorageProbe, StorageState};

/// The one open store. Lives behind a mutex in Tauri state (single writer).
pub struct Store {
    pub(crate) conn: Connection,
    pub owner_id: String,
    pub device_id: String,
    pub path: PathBuf,
    pub schema_version: i64,
    pub(crate) keystore: Arc<dyn KeyStore>,
    pub(crate) probe: StorageProbe,
    pub(crate) last_storage_state: Option<StorageState>,
}

/// Ids are used in file names and credential names: keep them boring.
pub fn validate_id(what: &str, id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
    if ok {
        Ok(())
    } else {
        Err(StoreError::invalid(format!(
            "{what} must be 1 to 128 characters of letters, digits and '-'."
        )))
    }
}

/// `till_<ownerId>_<deviceId>.db`: a device signed in to business A can never
/// open business B's file, because B's file has another name AND another key.
pub fn db_path(data_dir: &Path, owner_id: &str, device_id: &str) -> PathBuf {
    data_dir.join(format!("till_{owner_id}_{device_id}.db"))
}

pub fn now_str() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn ts(t: chrono::DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl Store {
    /// Open (creating if needed) the store for (owner, device) in `data_dir`,
    /// with its key from the credential store.
    pub fn open(
        data_dir: &Path,
        owner_id: &str,
        device_id: &str,
        keystore: Arc<dyn KeyStore>,
    ) -> Result<Store> {
        validate_id("ownerId", owner_id)?;
        validate_id("deviceId", device_id)?;
        std::fs::create_dir_all(data_dir)?;
        let path = db_path(data_dir, owner_id, device_id);
        let exists = path.exists() && std::fs::metadata(&path)?.len() > 0;
        let key =
            crypto_key::get_or_create_db_key(keystore.as_ref(), owner_id, device_id, !exists)?;
        Self::open_with_key(&path, owner_id, device_id, &key, keystore)
    }

    /// Open a specific file with a specific key (tests and `open`).
    pub fn open_with_key(
        path: &Path,
        owner_id: &str,
        device_id: &str,
        key: &DbKey,
        keystore: Arc<dyn KeyStore>,
    ) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let is_new = !path.exists() || std::fs::metadata(path)?.len() == 0;
        let mut conn = Connection::open(path)?;
        // Never store sales unencrypted: a build without SQLCipher (only ever
        // made for logic tests, `--no-default-features`) refuses outside tests.
        let cipher: Option<String> = conn
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .optional()?;
        if cipher.is_none() && !cfg!(test) {
            return Err(StoreError::crypto(
                "This build has no SQLCipher encryption; refusing to open the local store.",
            ));
        }
        // Raw 256-bit key (x'..'): no passphrase derivation, the key is random.
        conn.execute_batch(&format!("PRAGMA key = \"x'{}'\";", to_hex(key)))?;
        // The first read is where a wrong key shows up (SQLITE_NOTADB).
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|_| {
            StoreError::crypto("The local database could not be decrypted with this device's key.")
        })?;
        if is_new {
            // Must be set before the first table exists.
            conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
        }
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(StoreError::io(format!(
                "Could not switch the database to WAL (got {mode})."
            )));
        }
        conn.execute_batch(
            "PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; PRAGMA temp_store = MEMORY;",
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let schema_version = migrations::migrate(&mut conn)?;

        // Bind the file to its tenant: a renamed or swapped file is refused.
        let bound_owner = meta_get(&conn, "owner_id")?;
        let bound_device = meta_get(&conn, "device_id")?;
        match (bound_owner, bound_device) {
            (None, None) => {
                meta_set(&conn, "owner_id", owner_id)?;
                meta_set(&conn, "device_id", device_id)?;
                meta_set(&conn, "created_at", &now_str())?;
            }
            (Some(o), Some(d)) if o == owner_id && d == device_id => {}
            _ => {
                return Err(StoreError::crypto(
                    "This database file belongs to another business or device.",
                ))
            }
        }

        Ok(Store {
            conn,
            owner_id: owner_id.to_string(),
            device_id: device_id.to_string(),
            path: path.to_path_buf(),
            schema_version,
            keystore,
            probe: Box::new(storage::os_probe),
            last_storage_state: None,
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Replace the free-space probe (tests simulate a full disk with it).
    pub fn set_storage_probe(&mut self, probe: StorageProbe) {
        self.probe = probe;
    }

    /// Size of the DB plus its WAL, in MB.
    pub fn db_size_mb(&self) -> f64 {
        let mut bytes = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let mut wal = self.path.clone().into_os_string();
        wal.push("-wal");
        bytes += std::fs::metadata(PathBuf::from(wal))
            .map(|m| m.len())
            .unwrap_or(0);
        round2(bytes as f64 / 1_048_576.0)
    }
}

pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
        .optional()?)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

pub fn validate_setting_key(key: &str) -> Result<()> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'-');
    if ok {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "Setting keys are 1 to 64 characters of letters, digits, '_', '.', '-'.",
        ))
    }
}

pub fn settings_get(conn: &Connection, key: &str) -> Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value_json FROM settings WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match raw {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    })
}

pub fn settings_set(conn: &Connection, key: &str, value: &Value) -> Result<()> {
    validate_setting_key(key)?;
    conn.execute(
        "INSERT INTO settings(key, value_json, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at = excluded.updated_at",
        params![key, serde_json::to_string(value)?, now_str()],
    )?;
    Ok(())
}

/// A number from `offlineSettings.<field>` (the snapshot's shape), if set.
pub fn offline_setting_f64(conn: &Connection, field: &str) -> Result<Option<f64>> {
    Ok(settings_get(conn, "offlineSettings")?.and_then(|v| v.get(field).and_then(Value::as_f64)))
}

pub const DEFAULT_RETENTION_DAYS: i64 = 30;

pub fn retention_days(conn: &Connection) -> Result<i64> {
    Ok(offline_setting_f64(conn, "retentionDays")?
        .map(|d| d.round() as i64)
        .filter(|d| (1..=3650).contains(d))
        .unwrap_or(DEFAULT_RETENTION_DAYS))
}

/// Insert one audit row inside whatever transaction `conn` is in.
pub fn audit_insert(
    conn: &Connection,
    kind: &str,
    acting_user_id: Option<&str>,
    detail: &Value,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO audit_log(kind, acting_user_id, detail_json, device_time) VALUES (?1, ?2, ?3, ?4)",
        params![kind, acting_user_id, serde_json::to_string(detail)?, now_str()],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Test-only crash hooks. `DG_FAILPOINT=<name>` aborts the process at that
/// point (a real crash: no unwinding, no rollback code runs); `set_panic(name)`
/// panics there on the current thread instead.
#[cfg(test)]
pub mod failpoint {
    use std::cell::RefCell;
    thread_local! {
        static PANIC_AT: RefCell<Option<String>> = const { RefCell::new(None) };
    }
    pub fn set_panic(name: Option<&str>) {
        PANIC_AT.with(|p| *p.borrow_mut() = name.map(str::to_string));
    }
    pub fn hit(name: &str) {
        if std::env::var("DG_FAILPOINT").ok().as_deref() == Some(name) {
            std::process::abort();
        }
        let panic_here = PANIC_AT.with(|p| p.borrow().as_deref() == Some(name));
        if panic_here {
            panic!("failpoint {name}");
        }
    }
}

#[cfg(not(test))]
pub mod failpoint {
    #[inline(always)]
    pub fn hit(_name: &str) {}
}
