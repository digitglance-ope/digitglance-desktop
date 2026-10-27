//! The device's pairing (sync-protocol.md section 1): non-secret fields in the
//! encrypted DB, the device token and device secret in the credential store.
//!
//! `active-device.json` in the data directory names the (ownerId, deviceId) of
//! the last saved identity, so that on launch `device_identity_get` can reopen
//! the right store before the till knows who it is. It holds two ids and
//! nothing secret.

use std::path::Path;
use std::sync::Arc;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::store::crypto_key::{secret_account, token_account, KeyStore};
use crate::store::db::{self, now_str, Store};
use crate::store::error::{Result, StoreError};

pub const POINTER_FILE: &str = "active-device.json";

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentity {
    pub owner_id: String,
    pub device_id: String,
    pub device_code: String,
    pub branch_id: Option<String>,
    pub terminal_id: Option<String>,
    pub device_token: String,
    pub api_base: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeviceIdentityInput {
    pub owner_id: String,
    pub device_id: String,
    pub device_code: String,
    #[serde(default)]
    pub branch_id: Option<String>,
    #[serde(default)]
    pub terminal_id: Option<String>,
    pub device_token: String,
    pub api_base: String,
    pub device_secret: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pointer {
    owner_id: String,
    device_id: String,
}

fn validate(i: &DeviceIdentityInput) -> Result<()> {
    db::validate_id("ownerId", &i.owner_id)?;
    db::validate_id("deviceId", &i.device_id)?;
    if i.device_code.is_empty()
        || i.device_code.len() > 16
        || !i.device_code.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return Err(StoreError::invalid(
            "deviceCode must be 1 to 16 letters or digits.",
        ));
    }
    if i.device_token.is_empty() || i.device_token.len() > 1200 {
        return Err(StoreError::invalid(
            "deviceToken is required (at most 1200 characters).",
        ));
    }
    if i.device_secret.len() < 32 || i.device_secret.len() > 256 {
        return Err(StoreError::invalid(
            "deviceSecret must be 32 to 256 characters.",
        ));
    }
    let base = i.api_base.trim_end_matches('/');
    let local = base.starts_with("http://localhost") || base.starts_with("http://127.0.0.1");
    if !(base.starts_with("https://") || local) {
        return Err(StoreError::invalid("apiBase must be an https:// URL."));
    }
    Ok(())
}

pub fn save(store: &mut Store, data_dir: &Path, i: &DeviceIdentityInput) -> Result<()> {
    validate(i)?;
    if i.owner_id != store.owner_id || i.device_id != store.device_id {
        return Err(StoreError::invalid(
            "The open store belongs to another owner or device; store_open with these ids first.",
        ));
    }
    // Secrets first: if the credential store fails, the DB is left as it was.
    store
        .keystore
        .set(&token_account(&i.owner_id, &i.device_id), &i.device_token)?;
    store
        .keystore
        .set(&secret_account(&i.owner_id, &i.device_id), &i.device_secret)?;
    store.conn.execute(
        "INSERT INTO device_identity(id, owner_id, device_id, device_code, branch_id, terminal_id, api_base, saved_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET owner_id = excluded.owner_id, device_id = excluded.device_id,
             device_code = excluded.device_code, branch_id = excluded.branch_id,
             terminal_id = excluded.terminal_id, api_base = excluded.api_base, saved_at = excluded.saved_at",
        params![
            i.owner_id,
            i.device_id,
            i.device_code,
            i.branch_id,
            i.terminal_id,
            i.api_base.trim_end_matches('/'),
            now_str()
        ],
    )?;
    write_pointer(data_dir, &i.owner_id, &i.device_id)?;
    Ok(())
}

/// The identity of the open store, or None when unpaired or the token is gone.
pub fn get(store: &Store) -> Result<Option<DeviceIdentity>> {
    type Row = (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        String,
    );
    let row: Option<Row> = store
        .conn
        .query_row(
            "SELECT owner_id, device_id, device_code, branch_id, terminal_id, api_base
             FROM device_identity WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((owner_id, device_id, device_code, branch_id, terminal_id, api_base)) = row else {
        return Ok(None);
    };
    let Some(device_token) = store.keystore.get(&token_account(&owner_id, &device_id))? else {
        return Ok(None);
    };
    Ok(Some(DeviceIdentity {
        owner_id,
        device_id,
        device_code,
        branch_id,
        terminal_id,
        device_token,
        api_base,
    }))
}

/// The device secret for the `X-Device-Secret` header (not part of `get`).
pub fn secret(store: &Store) -> Result<Option<String>> {
    store
        .keystore
        .get(&secret_account(&store.owner_id, &store.device_id))
}

/// Forget the pairing: token, secret, identity row and pointer. The DB file and
/// every unsent sale in it are KEPT (export them with export_pending).
pub fn clear(store: &mut Store, data_dir: &Path) -> Result<()> {
    store
        .keystore
        .delete(&token_account(&store.owner_id, &store.device_id))?;
    store
        .keystore
        .delete(&secret_account(&store.owner_id, &store.device_id))?;
    store.conn.execute("DELETE FROM device_identity", [])?;
    let p = data_dir.join(POINTER_FILE);
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

fn write_pointer(data_dir: &Path, owner_id: &str, device_id: &str) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let body = serde_json::to_vec_pretty(&Pointer {
        owner_id: owner_id.into(),
        device_id: device_id.into(),
    })?;
    let tmp = data_dir.join(format!("{POINTER_FILE}.tmp"));
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, data_dir.join(POINTER_FILE))?;
    Ok(())
}

/// On launch: reopen the last paired store named by the pointer, if any.
pub fn open_from_pointer(data_dir: &Path, keystore: Arc<dyn KeyStore>) -> Result<Option<Store>> {
    let p = data_dir.join(POINTER_FILE);
    let Ok(raw) = std::fs::read(&p) else {
        return Ok(None);
    };
    let Ok(ptr) = serde_json::from_slice::<Pointer>(&raw) else {
        return Ok(None);
    };
    if db::validate_id("ownerId", &ptr.owner_id).is_err()
        || db::validate_id("deviceId", &ptr.device_id).is_err()
    {
        return Ok(None);
    }
    if !db::db_path(data_dir, &ptr.owner_id, &ptr.device_id).exists() {
        return Ok(None);
    }
    Ok(Some(Store::open(
        data_dir,
        &ptr.owner_id,
        &ptr.device_id,
        keystore,
    )?))
}
