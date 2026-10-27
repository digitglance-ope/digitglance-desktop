//! Cached staff (D5, D6): the list without hashes, and offline PIN checks.
//!
//! Lockout: 3 wrong PINs in a row lock that user for 15 minutes on this device.
//! The counter lives in the database, so restarting the app does not reset it.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};

use crate::store::db::{self, now_str, ts, Store};
use crate::store::error::{Result, StoreError};

pub const MAX_PIN_ATTEMPTS: i64 = 3;
pub const LOCKOUT_MINUTES: i64 = 15;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaffMember {
    pub id: String,
    pub membership_id: Option<String>,
    pub name: String,
    pub role: Option<String>,
    pub permissions: Value,
    pub allowed_terminal_ids: Value,
    pub active: bool,
    pub has_pin: bool,
}

pub fn staff_list(store: &Store) -> Result<Vec<StaffMember>> {
    let mut st = store.conn.prepare_cached(
        "SELECT id, membership_id, name, role, permissions_json, allowed_terminal_ids_json, active,
                pin_hash IS NOT NULL
         FROM staff ORDER BY lower(name)",
    )?;
    let rows = st.query_map([], |r| {
        let perms: String = r.get(4)?;
        let allowed: Option<String> = r.get(5)?;
        Ok(StaffMember {
            id: r.get(0)?,
            membership_id: r.get(1)?,
            name: r.get(2)?,
            role: r.get(3)?,
            permissions: serde_json::from_str(&perms).unwrap_or_else(|_| json!([])),
            allowed_terminal_ids: allowed
                .and_then(|a| serde_json::from_str(&a).ok())
                .unwrap_or(Value::Null),
            active: r.get(6)?,
            has_pin: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PinResult {
    pub ok: bool,
    pub permissions: Value,
    /// Why it failed: wrong_pin | locked | unknown_user | inactive | no_pin |
    /// terminal_not_allowed. Absent on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempts_left: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

impl PinResult {
    fn fail(reason: &str) -> Self {
        PinResult {
            ok: false,
            permissions: json!([]),
            reason: Some(reason.to_string()),
            locked_until: None,
            attempts_left: None,
            name: None,
            role: None,
        }
    }
}

struct StaffAuth {
    name: String,
    role: Option<String>,
    pin_hash: Option<String>,
    permissions: String,
    allowed: Option<String>,
    active: bool,
}

pub fn pin_verify(store: &mut Store, user_id: &str, pin: &str) -> Result<PinResult> {
    if user_id.is_empty() || pin.is_empty() || pin.len() > 64 {
        return Err(StoreError::invalid("userId and pin are required."));
    }
    let now = Utc::now();
    let lock: Option<(i64, Option<String>)> = store
        .conn
        .query_row(
            "SELECT failed_count, locked_until FROM pin_attempts WHERE user_id = ?1",
            [user_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((_, Some(until))) = &lock {
        if let Ok(t) = DateTime::parse_from_rfc3339(until) {
            if t.with_timezone(&Utc) > now {
                let mut r = PinResult::fail("locked");
                r.locked_until = Some(until.clone());
                r.attempts_left = Some(0);
                return Ok(r);
            }
        }
    }

    let who: Option<StaffAuth> = store
        .conn
        .query_row(
            "SELECT name, role, pin_hash, permissions_json, allowed_terminal_ids_json, active
             FROM staff WHERE id = ?1",
            [user_id],
            |r| {
                Ok(StaffAuth {
                    name: r.get(0)?,
                    role: r.get(1)?,
                    pin_hash: r.get(2)?,
                    permissions: r.get(3)?,
                    allowed: r.get(4)?,
                    active: r.get(5)?,
                })
            },
        )
        .optional()?;
    let Some(who) = who else {
        return Ok(PinResult::fail("unknown_user"));
    };
    if !who.active {
        return Ok(PinResult::fail("inactive"));
    }
    let Some(hash) = who.pin_hash.as_deref() else {
        return Ok(PinResult::fail("no_pin"));
    };

    // A bad hash counts as a wrong PIN, never as a pass.
    let good = bcrypt::verify(pin, hash).unwrap_or(false);
    if good {
        // Scope rule as on the server (cashier_not_allowed): a member limited
        // to some terminals cannot sign in on another.
        if let Some(allowed) = who.allowed.as_deref() {
            let terminal: Option<String> = store
                .conn
                .query_row(
                    "SELECT terminal_id FROM device_identity WHERE id = 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            let list: Vec<String> = serde_json::from_str(allowed).unwrap_or_default();
            if let Some(t) = terminal {
                if !list.is_empty() && !list.contains(&t) {
                    return Ok(PinResult::fail("terminal_not_allowed"));
                }
            }
        }
        store
            .conn
            .execute("DELETE FROM pin_attempts WHERE user_id = ?1", [user_id])?;
        return Ok(PinResult {
            ok: true,
            permissions: serde_json::from_str(&who.permissions).unwrap_or_else(|_| json!([])),
            reason: None,
            locked_until: None,
            attempts_left: None,
            name: Some(who.name),
            role: who.role,
        });
    }

    let failed = lock.map(|l| l.0).unwrap_or(0) + 1;
    let tx = store.conn.transaction()?;
    let mut r = PinResult::fail("wrong_pin");
    if failed >= MAX_PIN_ATTEMPTS {
        let until = ts(now + Duration::minutes(LOCKOUT_MINUTES));
        tx.execute(
            "INSERT INTO pin_attempts(user_id, failed_count, locked_until, last_failed_at)
             VALUES (?1, 0, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET failed_count = 0, locked_until = ?2, last_failed_at = ?3",
            params![user_id, until, now_str()],
        )?;
        db::audit_insert(
            &tx,
            "pin_lockout",
            Some(user_id),
            &json!({ "lockedUntil": until }),
        )?;
        r.reason = Some("locked".into());
        r.locked_until = Some(until);
        r.attempts_left = Some(0);
    } else {
        tx.execute(
            "INSERT INTO pin_attempts(user_id, failed_count, locked_until, last_failed_at)
             VALUES (?1, ?2, NULL, ?3)
             ON CONFLICT(user_id) DO UPDATE SET failed_count = ?2, locked_until = NULL, last_failed_at = ?3",
            params![user_id, failed, now_str()],
        )?;
        r.attempts_left = Some(MAX_PIN_ATTEMPTS - failed);
    }
    tx.commit()?;
    Ok(r)
}
