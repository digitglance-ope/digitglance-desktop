//! Tauri commands: the contract in docs/LOCAL-STORE-API.md. Names are snake_case,
//! JSON fields camelCase, every command returns `Result<T, StoreError>`.
//!
//! Single writer: one connection behind one mutex. Commands are short and run
//! on the blocking pool so a slow disk never stalls the async runtime; a
//! command that cannot get the lock within 30 s returns `busy`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;
use tauri::State;

use crate::store::crypto_key::KeyStore;
use crate::store::error::{Result, StoreError};
use crate::store::{audit, catalog, cleanup, db, identity, queue, sales, staff, storage, Store};

pub struct StoreState {
    slot: Arc<Mutex<Option<Store>>>,
    data_dir: PathBuf,
    keystore: Arc<dyn KeyStore>,
    /// The store is a DigitGlance Trade feature; other product shells refuse.
    enabled: bool,
}

impl StoreState {
    pub fn new(data_dir: PathBuf, keystore: Arc<dyn KeyStore>, enabled: bool) -> Self {
        StoreState {
            slot: Arc::new(Mutex::new(None)),
            data_dir,
            keystore,
            enabled,
        }
    }
}

const LOCK_WAIT: Duration = Duration::from_secs(30);

fn lock(m: &Mutex<Option<Store>>) -> Result<MutexGuard<'_, Option<Store>>> {
    let start = Instant::now();
    loop {
        match m.try_lock() {
            Ok(g) => return Ok(g),
            // A panic mid-command rolled its SQLite transaction back on unwind;
            // the connection itself is still sound.
            Err(TryLockError::Poisoned(p)) => return Ok(p.into_inner()),
            Err(TryLockError::WouldBlock) if start.elapsed() < LOCK_WAIT => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(TryLockError::WouldBlock) => {
                return Err(StoreError::busy("The local store is busy; try again."))
            }
        }
    }
}

struct Ctx {
    slot: Arc<Mutex<Option<Store>>>,
    data_dir: PathBuf,
    keystore: Arc<dyn KeyStore>,
}

/// Run `f` with the slot (open or not) on the blocking pool.
async fn with_slot<T, F>(state: &StoreState, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Option<Store>, &Ctx) -> Result<T> + Send + 'static,
{
    if !state.enabled {
        return Err(StoreError::invalid(
            "The local store is only available in DigitGlance Trade.",
        ));
    }
    let ctx = Ctx {
        slot: state.slot.clone(),
        data_dir: state.data_dir.clone(),
        keystore: state.keystore.clone(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let mut g = lock(&ctx.slot)?;
        f(&mut g, &ctx)
    })
    .await
    .map_err(|e| StoreError::io(format!("The store task failed: {e}")))?
}

/// Run `f` with the open store.
async fn with_store<T, F>(state: &StoreState, f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Store, &Ctx) -> Result<T> + Send + 'static,
{
    with_slot(state, move |slot, ctx| match slot.as_mut() {
        Some(s) => f(s, ctx),
        None => Err(StoreError::not_open()),
    })
    .await
}

// ---------------------------------------------------------------- lifecycle

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenResult {
    pub opened: bool,
    pub schema_version: i64,
}

#[tauri::command]
pub async fn store_open(
    state: State<'_, StoreState>,
    owner_id: String,
    device_id: String,
) -> Result<OpenResult> {
    with_slot(&state, move |slot, ctx| {
        if let Some(s) = slot.as_ref() {
            if s.owner_id == owner_id && s.device_id == device_id {
                return Ok(OpenResult {
                    opened: true,
                    schema_version: s.schema_version,
                });
            }
        }
        *slot = None; // close any other tenant's store first
        let s = Store::open(&ctx.data_dir, &owner_id, &device_id, ctx.keystore.clone())?;
        let v = s.schema_version;
        *slot = Some(s);
        Ok(OpenResult {
            opened: true,
            schema_version: v,
        })
    })
    .await
}

#[tauri::command]
pub async fn store_close(state: State<'_, StoreState>) -> Result<()> {
    with_slot(&state, |slot, _| {
        if let Some(s) = slot.take() {
            let _ = s.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
        }
        Ok(())
    })
    .await
}

// ---------------------------------------------------------------- identity

/// Reopens the last paired store on launch (see identity.rs), so the till can
/// call this first thing. Null when the device is not paired.
#[tauri::command]
pub async fn device_identity_get(
    state: State<'_, StoreState>,
) -> Result<Option<identity::DeviceIdentity>> {
    with_slot(&state, |slot, ctx| {
        if slot.is_none() {
            *slot = identity::open_from_pointer(&ctx.data_dir, ctx.keystore.clone())?;
        }
        match slot.as_ref() {
            Some(s) => identity::get(s),
            None => Ok(None),
        }
    })
    .await
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
pub async fn device_identity_save(
    state: State<'_, StoreState>,
    owner_id: String,
    device_id: String,
    device_code: String,
    branch_id: Option<String>,
    terminal_id: Option<String>,
    device_token: String,
    api_base: String,
    device_secret: String,
) -> Result<()> {
    let input = identity::DeviceIdentityInput {
        owner_id,
        device_id,
        device_code,
        branch_id,
        terminal_id,
        device_token,
        api_base,
        device_secret,
    };
    with_store(&state, move |s, ctx| {
        identity::save(s, &ctx.data_dir, &input)
    })
    .await
}

#[tauri::command]
pub async fn device_identity_clear(state: State<'_, StoreState>) -> Result<()> {
    with_store(&state, |s, ctx| identity::clear(s, &ctx.data_dir)).await
}

/// Addition to the contract: the `X-Device-Secret` value, kept out of
/// `device_identity_get` so it is fetched only where a request is built.
#[tauri::command]
pub async fn device_secret_get(state: State<'_, StoreState>) -> Result<Option<String>> {
    with_store(&state, |s, _| identity::secret(s)).await
}

// ---------------------------------------------------------------- status, settings

#[tauri::command]
pub async fn store_status(state: State<'_, StoreState>) -> Result<queue::StoreStatus> {
    with_store(&state, |s, _| queue::status(s)).await
}

#[tauri::command]
pub async fn storage_check(state: State<'_, StoreState>) -> Result<storage::StorageCheck> {
    with_store(&state, |s, _| storage::check(s)).await
}

#[tauri::command]
pub async fn settings_get(state: State<'_, StoreState>, key: String) -> Result<Option<Value>> {
    with_store(&state, move |s, _| {
        db::validate_setting_key(&key)?;
        db::settings_get(&s.conn, &key)
    })
    .await
}

#[tauri::command]
pub async fn settings_set(
    state: State<'_, StoreState>,
    key: String,
    value_json: Value,
) -> Result<()> {
    with_store(&state, move |s, _| {
        db::settings_set(&s.conn, &key, &value_json)
    })
    .await
}

// ---------------------------------------------------------------- catalogue

#[tauri::command]
pub async fn catalog_apply_snapshot(
    state: State<'_, StoreState>,
    snapshot: Value,
) -> Result<catalog::SnapshotResult> {
    with_store(&state, move |s, _| catalog::apply_snapshot(s, &snapshot)).await
}

#[tauri::command]
pub async fn catalog_apply_changes(
    state: State<'_, StoreState>,
    rows: Vec<Value>,
    cursor: Value,
) -> Result<catalog::ChangesResult> {
    with_store(&state, move |s, _| {
        catalog::apply_changes(s, &rows, &cursor)
    })
    .await
}

#[tauri::command]
pub async fn catalog_get_cursor(state: State<'_, StoreState>) -> Result<Value> {
    with_store(&state, |s, _| catalog::get_cursor(s)).await
}

#[tauri::command]
pub async fn catalog_search(
    state: State<'_, StoreState>,
    q: String,
    category_id: Option<String>,
    limit: Option<i64>,
) -> Result<Vec<Value>> {
    with_store(&state, move |s, _| {
        catalog::search(s, &q, category_id.as_deref(), limit)
    })
    .await
}

#[tauri::command]
pub async fn catalog_by_barcode(
    state: State<'_, StoreState>,
    code: String,
) -> Result<Option<Value>> {
    with_store(&state, move |s, _| catalog::by_barcode(s, &code)).await
}

#[tauri::command]
pub async fn catalog_get_products(
    state: State<'_, StoreState>,
    ids: Vec<String>,
) -> Result<Vec<Value>> {
    with_store(&state, move |s, _| catalog::get_products(s, &ids)).await
}

#[tauri::command]
pub async fn categories_list(state: State<'_, StoreState>) -> Result<Vec<Value>> {
    with_store(&state, |s, _| catalog::categories_list(s)).await
}

#[tauri::command]
pub async fn customers_search(
    state: State<'_, StoreState>,
    q: String,
    limit: Option<i64>,
) -> Result<Vec<Value>> {
    with_store(&state, move |s, _| catalog::customers_search(s, &q, limit)).await
}

#[tauri::command]
pub async fn customer_get(state: State<'_, StoreState>, id: String) -> Result<Option<Value>> {
    with_store(&state, move |s, _| catalog::customer_get(s, &id)).await
}

// ---------------------------------------------------------------- staff

#[tauri::command]
pub async fn staff_list(state: State<'_, StoreState>) -> Result<Vec<staff::StaffMember>> {
    with_store(&state, |s, _| staff::staff_list(s)).await
}

#[tauri::command]
pub async fn pin_verify(
    state: State<'_, StoreState>,
    user_id: String,
    pin: String,
) -> Result<staff::PinResult> {
    with_store(&state, move |s, _| staff::pin_verify(s, &user_id, &pin)).await
}

// ---------------------------------------------------------------- sales, shifts

#[tauri::command]
pub async fn sale_commit(state: State<'_, StoreState>, sale: Value) -> Result<sales::CommitResult> {
    with_store(&state, move |s, _| sales::sale_commit(s, sale)).await
}

#[tauri::command]
pub async fn shift_open(state: State<'_, StoreState>, shift: Value) -> Result<sales::ShiftResult> {
    with_store(&state, move |s, _| sales::shift_open(s, shift)).await
}

#[tauri::command]
pub async fn shift_close(state: State<'_, StoreState>, shift: Value) -> Result<sales::ShiftResult> {
    with_store(&state, move |s, _| sales::shift_close(s, shift)).await
}

/// Addition to the contract: the open shift after a restart, or null.
#[tauri::command]
pub async fn shift_current(state: State<'_, StoreState>) -> Result<Option<Value>> {
    with_store(&state, |s, _| sales::shift_current(s)).await
}

#[tauri::command]
pub async fn sales_recent(
    state: State<'_, StoreState>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<sales::SaleSummary>> {
    with_store(&state, move |s, _| sales::sales_recent(s, limit, offset)).await
}

#[tauri::command]
pub async fn sale_get(state: State<'_, StoreState>, transaction_id: String) -> Result<Value> {
    with_store(&state, move |s, _| sales::sale_get(s, &transaction_id)).await
}

// ---------------------------------------------------------------- queue

#[tauri::command]
pub async fn queue_next_batch(
    state: State<'_, StoreState>,
    limit: Option<i64>,
) -> Result<Vec<queue::QueueItem>> {
    with_store(&state, move |s, _| queue::next_batch(s, limit)).await
}

#[tauri::command]
pub async fn queue_mark(
    state: State<'_, StoreState>,
    results: Vec<queue::MarkResult>,
) -> Result<queue::MarkSummary> {
    with_store(&state, move |s, _| queue::mark(s, &results)).await
}

#[tauri::command]
pub async fn queue_failed_list(state: State<'_, StoreState>) -> Result<Vec<queue::QueueItem>> {
    with_store(&state, |s, _| queue::failed_list(s)).await
}

#[tauri::command]
pub async fn queue_retry_failed(
    state: State<'_, StoreState>,
    id: String,
) -> Result<queue::QueueItem> {
    with_store(&state, move |s, _| queue::retry_failed(s, &id)).await
}

// ---------------------------------------------------------------- housekeeping

#[tauri::command]
pub async fn cleanup_run(
    state: State<'_, StoreState>,
    retention_days: Option<i64>,
    force: Option<bool>,
) -> Result<cleanup::CleanupResult> {
    with_store(&state, move |s, _| {
        cleanup::run(s, retention_days, force.unwrap_or(false))
    })
    .await
}

#[tauri::command]
pub async fn export_pending(
    state: State<'_, StoreState>,
    path: String,
) -> Result<queue::ExportResult> {
    with_store(&state, move |s, _| queue::export_pending(s, &path)).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendResult {
    pub id: i64,
}

#[tauri::command]
pub async fn audit_append(
    state: State<'_, StoreState>,
    kind: String,
    detail: Option<Value>,
    acting_user_id: Option<String>,
) -> Result<AppendResult> {
    with_store(&state, move |s, _| {
        let id = audit::append(
            s,
            &kind,
            &detail.unwrap_or(Value::Null),
            acting_user_id.as_deref(),
        )?;
        Ok(AppendResult { id })
    })
    .await
}

#[tauri::command]
pub async fn audit_take_unsent(
    state: State<'_, StoreState>,
    limit: Option<i64>,
) -> Result<Vec<audit::AuditEvent>> {
    with_store(&state, move |s, _| audit::take_unsent(s, limit)).await
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkedResult {
    pub marked: u64,
}

#[tauri::command]
pub async fn audit_mark_sent(state: State<'_, StoreState>, ids: Vec<i64>) -> Result<MarkedResult> {
    with_store(&state, move |s, _| {
        Ok(MarkedResult {
            marked: audit::mark_sent(s, &ids)?,
        })
    })
    .await
}

/// Every command, for `generate_handler!` in lib.rs and the ACL in build.rs.
pub const COMMANDS: &[&str] = &[
    "store_open",
    "store_close",
    "device_identity_get",
    "device_identity_save",
    "device_identity_clear",
    "device_secret_get",
    "store_status",
    "storage_check",
    "settings_get",
    "settings_set",
    "catalog_apply_snapshot",
    "catalog_apply_changes",
    "catalog_get_cursor",
    "catalog_search",
    "catalog_by_barcode",
    "catalog_get_products",
    "categories_list",
    "customers_search",
    "customer_get",
    "staff_list",
    "pin_verify",
    "sale_commit",
    "shift_open",
    "shift_close",
    "shift_current",
    "sales_recent",
    "sale_get",
    "queue_next_batch",
    "queue_mark",
    "queue_failed_list",
    "queue_retry_failed",
    "cleanup_run",
    "export_pending",
    "audit_append",
    "audit_take_unsent",
    "audit_mark_sent",
];
