//! Recording sales and shifts. Each is ONE SQLite transaction (BEGIN IMMEDIATE,
//! WAL, synchronous=FULL): receipt number, sale, lines, payments, local stock
//! deltas, the outbox row and the audit row land together or not at all. The
//! receipt prints only after `sale_commit` returns, so a printed receipt always
//! has its row on disk.

use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::store::db::{self, failpoint, now_str, Store};
use crate::store::error::{Result, StoreError};
use crate::store::storage::{self, StorageState};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaleInput {
    transaction_id: String,
    kind: String,
    recorded_at: String,
    cashier_id: String,
    #[serde(default)]
    approvals: Vec<Value>,
    branch_id: String,
    terminal_id: String,
    shift_local_id: String,
    #[serde(default)]
    customer_id: Option<String>,
    lines: Vec<LineInput>,
    payments: Vec<PaymentInput>,
    totals: Value,
    #[serde(default)]
    loyalty: Option<Value>,
    #[serde(default)]
    notes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LineInput {
    #[serde(default)]
    product_id: Option<String>,
    product_snapshot: Value,
    qty: f64,
    unit_price: f64,
    #[serde(default)]
    discount: f64,
    #[serde(default)]
    vat_rate: f64,
    #[serde(default)]
    vat_amount: f64,
    line_total: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PaymentInput {
    method: String,
    amount: f64,
    #[serde(default)]
    tendered: Option<f64>,
    #[serde(default)]
    change: Option<f64>,
    confirmation_status: String,
    #[serde(default)]
    reference: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CommitResult {
    pub receipt_number: String,
    pub local_seq: i64,
}

fn finite(name: &str, v: f64) -> Result<()> {
    if v.is_finite() {
        Ok(())
    } else {
        Err(StoreError::invalid(format!(
            "{name} must be a finite number."
        )))
    }
}

fn word(name: &str, v: &str) -> Result<()> {
    let ok = (2..=32).contains(&v.len()) && v.bytes().all(|c| c.is_ascii_lowercase() || c == b'_');
    if ok {
        Ok(())
    } else {
        Err(StoreError::invalid(format!(
            "{name} '{v}' must be 2 to 32 lower-case letters or '_'."
        )))
    }
}

fn validate_uuid(v: &str) -> Result<()> {
    uuid::Uuid::parse_str(v)
        .map(|_| ())
        .map_err(|_| StoreError::invalid("transactionId must be a UUID (v7 from the till)."))
}

fn validate_sale(s: &SaleInput) -> Result<()> {
    validate_uuid(&s.transaction_id)?;
    if s.kind != "sale" {
        return Err(StoreError::invalid("kind must be 'sale'."));
    }
    for (n, v) in [
        ("recordedAt", &s.recorded_at),
        ("cashierId", &s.cashier_id),
        ("branchId", &s.branch_id),
        ("terminalId", &s.terminal_id),
        ("shiftLocalId", &s.shift_local_id),
    ] {
        if v.trim().is_empty() {
            return Err(StoreError::invalid(format!("{n} is required.")));
        }
    }
    if chrono::DateTime::parse_from_rfc3339(&s.recorded_at).is_err() {
        return Err(StoreError::invalid(
            "recordedAt must be an RFC 3339 timestamp.",
        ));
    }
    if s.lines.is_empty() {
        return Err(StoreError::invalid("A sale needs at least one line."));
    }
    for l in &s.lines {
        for (n, v) in [
            ("qty", l.qty),
            ("unitPrice", l.unit_price),
            ("discount", l.discount),
            ("vatRate", l.vat_rate),
            ("vatAmount", l.vat_amount),
            ("lineTotal", l.line_total),
        ] {
            finite(n, v)?;
        }
        if l.qty <= 0.0 {
            return Err(StoreError::invalid("qty must be above zero."));
        }
        if !l.product_snapshot.is_object() {
            return Err(StoreError::invalid("productSnapshot must be an object."));
        }
    }
    for p in &s.payments {
        word("method", &p.method)?;
        word("confirmationStatus", &p.confirmation_status)?;
        finite("amount", p.amount)?;
        if let Some(t) = p.tendered {
            finite("tendered", t)?;
        }
        if let Some(c) = p.change {
            finite("change", c)?;
        }
    }
    if !s.totals.is_object() {
        return Err(StoreError::invalid("totals must be an object."));
    }
    let _ = (&s.approvals, &s.loyalty, &s.notes);
    Ok(())
}

fn existing_receipt(store: &Store, tid: &str) -> Result<Option<CommitResult>> {
    Ok(store
        .conn
        .query_row(
            "SELECT receipt_number, local_seq FROM sales WHERE transaction_id = ?1",
            [tid],
            |r| {
                Ok(CommitResult {
                    receipt_number: r.get(0)?,
                    local_seq: r.get(1)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn device_code(store: &Store) -> Result<String> {
    store
        .conn
        .query_row(
            "SELECT device_code FROM device_identity WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::invalid(
                "Save the device identity (device_identity_save) before recording sales.",
            )
        })
}

/// Before any write that adds data: at critical, clean what retention allows;
/// if nothing could be cleaned, refuse. Nothing is written on refusal.
fn ensure_storage(store: &mut Store) -> Result<()> {
    let info = storage::measure(store)?;
    if info.state != StorageState::Critical {
        return Ok(());
    }
    let removed = crate::store::cleanup::run(store, None, false)?.removed;
    if removed == 0 {
        return Err(StoreError::storage_critical(format!(
            "Storage is critical ({} MB free, {}%) and there is nothing safe to clean. \
             Free disk space on this computer; unsent sales are kept.",
            info.free_mb, info.free_pct
        )));
    }
    Ok(())
}

fn total_of(totals: &Value) -> Option<f64> {
    ["total", "grandTotal", "amountDue"]
        .iter()
        .find_map(|k| totals.get(*k).and_then(Value::as_f64))
}

pub fn sale_commit(store: &mut Store, sale: Value) -> Result<CommitResult> {
    let input: SaleInput = serde_json::from_value(sale.clone())
        .map_err(|e| StoreError::invalid(format!("Sale: {e}")))?;
    validate_sale(&input)?;
    // Idempotent: the till may retry a commit it did not see return.
    if let Some(r) = existing_receipt(store, &input.transaction_id)? {
        return Ok(r);
    }
    let shift_status: Option<String> = store
        .conn
        .query_row(
            "SELECT status FROM shifts WHERE shift_local_id = ?1",
            [&input.shift_local_id],
            |r| r.get(0),
        )
        .optional()?;
    if shift_status.as_deref() == Some("closed") {
        return Err(StoreError::invalid(
            "That shift is closed; open a new shift to sell.",
        ));
    }
    let code = device_code(store)?;
    ensure_storage(store)?;

    let now = now_str();
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let seq: i64 = tx.query_row(
        "UPDATE sequences SET value = value + 1 WHERE name = 'receipt' RETURNING value",
        [],
        |r| r.get(0),
    )?;
    failpoint::hit("sale_after_seq");
    let receipt = format!("{code}-{seq:06}");

    // The upload payload is the till's sale exactly, plus the number it printed.
    let mut payload = sale;
    if let Some(o) = payload.as_object_mut() {
        o.insert("receiptNumber".into(), json!(receipt));
        o.insert("localSeq".into(), json!(seq));
        o.insert("deviceCode".into(), json!(code));
    }
    let payload_json = serde_json::to_string(&payload)?;

    tx.execute(
        "INSERT INTO sales(transaction_id, local_seq, receipt_number, kind, recorded_at, cashier_id,
                           branch_id, terminal_id, shift_local_id, customer_id, total, totals_json,
                           created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            input.transaction_id,
            seq,
            receipt,
            input.kind,
            input.recorded_at,
            input.cashier_id,
            input.branch_id,
            input.terminal_id,
            input.shift_local_id,
            input.customer_id,
            total_of(&input.totals),
            serde_json::to_string(&input.totals)?,
            now,
        ],
    )?;
    failpoint::hit("sale_after_header");
    {
        let mut line = tx.prepare_cached(
            "INSERT INTO sale_lines(transaction_id, line_no, product_id, product_snapshot_json, qty,
                                    unit_price, discount, vat_rate, vat_amount, line_total)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let mut delta = tx.prepare_cached(
            "INSERT INTO local_stock_delta(transaction_id, product_id, qty_delta, created_at)
             VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (i, l) in input.lines.iter().enumerate() {
            line.execute(params![
                input.transaction_id,
                i as i64 + 1,
                l.product_id,
                serde_json::to_string(&l.product_snapshot)?,
                l.qty,
                l.unit_price,
                l.discount,
                l.vat_rate,
                l.vat_amount,
                l.line_total,
            ])?;
            if let Some(pid) = l.product_id.as_deref().filter(|p| !p.is_empty()) {
                delta.execute(params![input.transaction_id, pid, -l.qty, now])?;
            }
        }
    }
    failpoint::hit("sale_after_lines");
    {
        let mut pay = tx.prepare_cached(
            "INSERT INTO sale_payments(transaction_id, idx, method, amount, tendered, change_amount,
                                       confirmation_status, reference)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for (i, p) in input.payments.iter().enumerate() {
            pay.execute(params![
                input.transaction_id,
                i as i64 + 1,
                p.method,
                p.amount,
                p.tendered,
                p.change,
                p.confirmation_status,
                p.reference,
            ])?;
        }
    }
    tx.execute(
        "INSERT INTO outbox(transaction_id, kind, status, payload_json, created_at, updated_at)
         VALUES (?1, 'sale', 'pending', ?2, ?3, ?3)",
        params![input.transaction_id, payload_json, now],
    )?;
    failpoint::hit("sale_after_outbox");
    db::audit_insert(
        &tx,
        "sale_recorded",
        Some(&input.cashier_id),
        &json!({ "transactionId": input.transaction_id, "receiptNumber": receipt,
                 "total": total_of(&input.totals), "offline": true }),
    )?;
    failpoint::hit("sale_before_commit");
    tx.commit()?;
    failpoint::hit("sale_after_commit");
    Ok(CommitResult {
        receipt_number: receipt,
        local_seq: seq,
    })
}

// ---------------------------------------------------------------- shifts

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShiftInput {
    transaction_id: String,
    shift_local_id: String,
    recorded_at: String,
    cashier_id: String,
    #[serde(default)]
    branch_id: Option<String>,
    #[serde(default)]
    terminal_id: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ShiftResult {
    pub transaction_id: String,
    pub shift_local_id: String,
    pub status: String,
}

fn parse_shift(shift: &Value, kind: &str) -> Result<ShiftInput> {
    let s: ShiftInput = serde_json::from_value(shift.clone())
        .map_err(|e| StoreError::invalid(format!("Shift: {e}")))?;
    validate_uuid(&s.transaction_id)?;
    if s.shift_local_id.trim().is_empty() || s.cashier_id.trim().is_empty() {
        return Err(StoreError::invalid(
            "shiftLocalId and cashierId are required.",
        ));
    }
    if chrono::DateTime::parse_from_rfc3339(&s.recorded_at).is_err() {
        return Err(StoreError::invalid(
            "recordedAt must be an RFC 3339 timestamp.",
        ));
    }
    let _ = kind;
    Ok(s)
}

fn queued(store: &Store, tid: &str) -> Result<bool> {
    Ok(store
        .conn
        .query_row(
            "SELECT 1 FROM outbox WHERE transaction_id = ?1",
            [tid],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn with_kind(mut v: Value, kind: &str) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert("kind".into(), json!(kind));
    }
    v
}

/// Opening a shift is the start of selling, so it obeys critical storage too.
pub fn shift_open(store: &mut Store, shift: Value) -> Result<ShiftResult> {
    let s = parse_shift(&shift, "shift_open")?;
    let ok = ShiftResult {
        transaction_id: s.transaction_id.clone(),
        shift_local_id: s.shift_local_id.clone(),
        status: "open".into(),
    };
    if queued(store, &s.transaction_id)? {
        return Ok(ok);
    }
    let open: Option<String> = store
        .conn
        .query_row(
            "SELECT shift_local_id FROM shifts WHERE status = 'open'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(o) = open {
        return Err(StoreError::invalid(format!(
            "Shift {o} is still open on this device; close it first."
        )));
    }
    ensure_storage(store)?;
    let now = now_str();
    let payload = serde_json::to_string(&with_kind(shift, "shift_open"))?;
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO shifts(shift_local_id, status, cashier_id, branch_id, terminal_id, opened_at,
                            open_transaction_id, open_payload_json)
         VALUES (?1, 'open', ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            s.shift_local_id,
            s.cashier_id,
            s.branch_id,
            s.terminal_id,
            s.recorded_at,
            s.transaction_id,
            payload
        ],
    )?;
    tx.execute(
        "INSERT INTO outbox(transaction_id, kind, status, payload_json, created_at, updated_at)
         VALUES (?1, 'shift_open', 'pending', ?2, ?3, ?3)",
        params![s.transaction_id, payload, now],
    )?;
    db::audit_insert(
        &tx,
        "shift_opened",
        Some(&s.cashier_id),
        &json!({ "shiftLocalId": s.shift_local_id }),
    )?;
    tx.commit()?;
    Ok(ok)
}

/// Closing is always allowed (even at critical storage): the drawer must balance.
pub fn shift_close(store: &mut Store, shift: Value) -> Result<ShiftResult> {
    let s = parse_shift(&shift, "shift_close")?;
    let ok = ShiftResult {
        transaction_id: s.transaction_id.clone(),
        shift_local_id: s.shift_local_id.clone(),
        status: "closed".into(),
    };
    if queued(store, &s.transaction_id)? {
        return Ok(ok);
    }
    let status: Option<String> = store
        .conn
        .query_row(
            "SELECT status FROM shifts WHERE shift_local_id = ?1",
            [&s.shift_local_id],
            |r| r.get(0),
        )
        .optional()?;
    match status.as_deref() {
        None => return Err(StoreError::not_found("No such shift on this device.")),
        Some("closed") => return Err(StoreError::invalid("That shift is already closed.")),
        _ => {}
    }
    let now = now_str();
    let payload = serde_json::to_string(&with_kind(shift, "shift_close"))?;
    let tx = store
        .conn
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute(
        "UPDATE shifts SET status = 'closed', closed_at = ?2, close_transaction_id = ?3, close_payload_json = ?4
         WHERE shift_local_id = ?1",
        params![s.shift_local_id, s.recorded_at, s.transaction_id, payload],
    )?;
    tx.execute(
        "INSERT INTO outbox(transaction_id, kind, status, payload_json, created_at, updated_at)
         VALUES (?1, 'shift_close', 'pending', ?2, ?3, ?3)",
        params![s.transaction_id, payload, now],
    )?;
    db::audit_insert(
        &tx,
        "shift_closed",
        Some(&s.cashier_id),
        &json!({ "shiftLocalId": s.shift_local_id }),
    )?;
    tx.commit()?;
    Ok(ok)
}

/// The open shift on this device, if any (its open payload plus status).
pub fn shift_current(store: &Store) -> Result<Option<Value>> {
    let row: Option<(String, String)> = store
        .conn
        .query_row(
            "SELECT shift_local_id, open_payload_json FROM shifts WHERE status = 'open'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((id, p)) => {
            let mut v: Value = serde_json::from_str(&p)?;
            if let Some(o) = v.as_object_mut() {
                o.insert("shiftLocalId".into(), json!(id));
                o.insert("status".into(), json!("open"));
            }
            Some(v)
        }
        None => None,
    })
}

// ---------------------------------------------------------------- reads

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaleSummary {
    pub transaction_id: String,
    pub receipt_number: String,
    pub local_seq: i64,
    pub recorded_at: String,
    pub cashier_id: Option<String>,
    pub customer_id: Option<String>,
    pub shift_local_id: Option<String>,
    pub total: Option<f64>,
    pub sync_status: Option<String>,
    pub server_id: Option<String>,
    pub synced_at: Option<String>,
}

pub fn sales_recent(
    store: &Store,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Vec<SaleSummary>> {
    let limit = limit.unwrap_or(50).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    let mut st = store.conn.prepare_cached(
        "SELECT s.transaction_id, s.receipt_number, s.local_seq, s.recorded_at, s.cashier_id,
                s.customer_id, s.shift_local_id, s.total, o.status, o.server_id, o.synced_at
         FROM sales s LEFT JOIN outbox o ON o.transaction_id = s.transaction_id
         ORDER BY s.local_seq DESC LIMIT ?1 OFFSET ?2",
    )?;
    let rows = st.query_map(params![limit, offset], |r| {
        Ok(SaleSummary {
            transaction_id: r.get(0)?,
            receipt_number: r.get(1)?,
            local_seq: r.get(2)?,
            recorded_at: r.get(3)?,
            cashier_id: r.get(4)?,
            customer_id: r.get(5)?,
            shift_local_id: r.get(6)?,
            total: r.get(7)?,
            sync_status: r.get(8)?,
            server_id: r.get(9)?,
            synced_at: r.get(10)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The sale exactly as committed (for reprint), plus its sync state.
pub fn sale_get(store: &Store, transaction_id: &str) -> Result<Value> {
    type Row = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    // The sale's payload lives once, in its outbox row (same lifetime as the sale).
    let row: Option<Row> = store
        .conn
        .query_row(
            "SELECT o.payload_json, o.status, o.server_id, o.server_receipt_number, o.synced_at,
                    o.flags_json, o.last_error
             FROM sales s JOIN outbox o ON o.transaction_id = s.transaction_id
             WHERE s.transaction_id = ?1",
            [transaction_id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((payload, status, server_id, server_receipt, synced_at, flags, last_error)) = row
    else {
        return Err(StoreError::not_found(
            "No sale with that transactionId on this device.",
        ));
    };
    let mut v: Value = serde_json::from_str(&payload)?;
    if let Some(o) = v.as_object_mut() {
        o.insert(
            "sync".into(),
            json!({
                "status": status,
                "serverId": server_id,
                "serverReceiptNumber": server_receipt,
                "syncedAt": synced_at,
                "flags": flags.and_then(|f| serde_json::from_str::<Value>(&f).ok()),
                "lastError": last_error,
            }),
        );
    }
    Ok(v)
}
