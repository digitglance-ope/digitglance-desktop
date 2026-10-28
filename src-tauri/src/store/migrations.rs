//! Versioned schema migrations. Each migration runs in its own transaction and
//! is recorded in `schema_migrations`; a database from a NEWER app version is
//! refused rather than guessed at. Never edit a shipped migration: add one.

use chrono::Utc;
use rusqlite::{params, Connection};

use crate::store::error::{Result, StoreError};

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial",
        sql: V1,
    },
    Migration {
        version: 2,
        name: "returns",
        sql: V2,
    },
];

pub fn latest_version() -> i64 {
    MIGRATIONS.last().map(|m| m.version).unwrap_or(0)
}

pub fn migrate(conn: &mut Connection) -> Result<i64> {
    migrate_with(conn, MIGRATIONS)
}

pub fn current_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |r| r.get(0),
    )?)
}

pub fn migrate_with(conn: &mut Connection, migrations: &[Migration]) -> Result<i64> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at TEXT NOT NULL
        );",
    )?;
    let current = current_version(conn)?;
    let latest = migrations.last().map(|m| m.version).unwrap_or(0);
    if current > latest {
        return Err(StoreError::invalid(format!(
            "The local database is schema version {current}, newer than this app understands \
             ({latest}). Update the app; the data has not been touched."
        )));
    }
    for m in migrations.iter().filter(|m| m.version > current) {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations(version, name, applied_at) VALUES (?1, ?2, ?3)",
            params![m.version, m.name, Utc::now().to_rfc3339()],
        )?;
        tx.commit()?;
    }
    current_version(conn)
}

/// Schema 1. Times are RFC 3339 UTC with milliseconds (sortable as text).
const V1: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Non-secret half of the pairing. Token and secret live in the credential store.
CREATE TABLE device_identity (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    owner_id    TEXT NOT NULL,
    device_id   TEXT NOT NULL,
    device_code TEXT NOT NULL,
    branch_id   TEXT,
    terminal_id TEXT,
    api_base    TEXT NOT NULL,
    saved_at    TEXT NOT NULL
);

CREATE TABLE categories (
    id        TEXT PRIMARY KEY,
    name      TEXT NOT NULL,
    parent_id TEXT,
    sort_name TEXT NOT NULL,
    data_json TEXT NOT NULL
);

CREATE TABLE products (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    name_lc       TEXT NOT NULL,
    sku           TEXT,
    sku_lc        TEXT,
    barcode       TEXT,
    barcode_lc    TEXT,
    category_id   TEXT,
    selling_price REAL,
    is_active     INTEGER NOT NULL DEFAULT 1,
    data_json     TEXT NOT NULL,
    updated_at    TEXT
);
CREATE INDEX products_name_lc    ON products(name_lc);
CREATE INDEX products_sku_lc     ON products(sku_lc);
CREATE INDEX products_barcode_lc ON products(barcode_lc);
CREATE INDEX products_category   ON products(category_id, name_lc);

-- Search terms: each word of the name, plus the whole SKU and barcode, lower case.
CREATE TABLE product_terms (
    term       TEXT NOT NULL,
    product_id TEXT NOT NULL REFERENCES products(id) ON DELETE CASCADE,
    PRIMARY KEY (term, product_id)
) WITHOUT ROWID;
CREATE INDEX product_terms_product ON product_terms(product_id);

-- Server truth for the device's branch. Never decremented locally.
CREATE TABLE stock (
    product_id    TEXT PRIMARY KEY,
    stock_id      TEXT,
    branch_id     TEXT,
    on_hand       REAL NOT NULL DEFAULT 0,
    reserved      REAL NOT NULL DEFAULT 0,
    reorder_level REAL,
    data_json     TEXT NOT NULL,
    updated_at    TEXT
);
CREATE INDEX stock_stock_id ON stock(stock_id);

-- Local movements not yet reflected in `stock`. Displayed quantity =
-- stock.on_hand + SUM(qty_delta) over rows with reconciled_at IS NULL.
-- synced_at: the server accepted the sale; reconciled_at: a server stock row
-- that includes it has since been applied.
CREATE TABLE local_stock_delta (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id TEXT NOT NULL,
    product_id     TEXT NOT NULL,
    qty_delta      REAL NOT NULL,
    created_at     TEXT NOT NULL,
    synced_at      TEXT,
    reconciled_at  TEXT
);
CREATE INDEX local_stock_delta_open ON local_stock_delta(product_id) WHERE reconciled_at IS NULL;
CREATE INDEX local_stock_delta_tx   ON local_stock_delta(transaction_id);

CREATE TABLE customers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    name_lc           TEXT NOT NULL,
    phone             TEXT,
    loyalty_number_lc TEXT,
    is_active         INTEGER NOT NULL DEFAULT 1,
    data_json         TEXT NOT NULL,
    updated_at        TEXT
);
CREATE INDEX customers_name_lc ON customers(name_lc);
CREATE INDEX customers_phone   ON customers(phone);
CREATE INDEX customers_loyalty ON customers(loyalty_number_lc);

CREATE TABLE staff (
    id                        TEXT PRIMARY KEY,
    membership_id             TEXT,
    name                      TEXT NOT NULL,
    role                      TEXT,
    pin_hash                  TEXT,
    permissions_json          TEXT NOT NULL DEFAULT '[]',
    allowed_terminal_ids_json TEXT,
    active                    INTEGER NOT NULL DEFAULT 1,
    data_json                 TEXT NOT NULL
);
CREATE INDEX staff_membership ON staff(membership_id);

CREATE TABLE pin_attempts (
    user_id        TEXT PRIMARY KEY,
    failed_count   INTEGER NOT NULL DEFAULT 0,
    locked_until   TEXT,
    last_failed_at TEXT
);

CREATE TABLE sequences (
    name  TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT INTO sequences(name, value) VALUES ('receipt', 0);

CREATE TABLE shifts (
    shift_local_id       TEXT PRIMARY KEY,
    status               TEXT NOT NULL CHECK (status IN ('open', 'closed')),
    cashier_id           TEXT,
    branch_id            TEXT,
    terminal_id          TEXT,
    opened_at            TEXT NOT NULL,
    closed_at            TEXT,
    open_transaction_id  TEXT NOT NULL,
    close_transaction_id TEXT,
    open_payload_json    TEXT NOT NULL,
    close_payload_json   TEXT
);
CREATE UNIQUE INDEX shifts_one_open ON shifts(status) WHERE status = 'open';

CREATE TABLE sales (
    transaction_id TEXT PRIMARY KEY,
    local_seq      INTEGER NOT NULL UNIQUE,
    receipt_number TEXT NOT NULL UNIQUE,
    kind           TEXT NOT NULL,
    recorded_at    TEXT NOT NULL,
    cashier_id     TEXT,
    branch_id      TEXT,
    terminal_id    TEXT,
    shift_local_id TEXT,
    customer_id    TEXT,
    total          REAL,
    totals_json    TEXT NOT NULL,
    created_at     TEXT NOT NULL
);
CREATE INDEX sales_shift ON sales(shift_local_id);

CREATE TABLE sale_lines (
    transaction_id        TEXT NOT NULL REFERENCES sales(transaction_id) ON DELETE CASCADE,
    line_no               INTEGER NOT NULL,
    product_id            TEXT,
    product_snapshot_json TEXT NOT NULL,
    qty                   REAL NOT NULL,
    unit_price            REAL NOT NULL,
    discount              REAL NOT NULL DEFAULT 0,
    vat_rate              REAL NOT NULL DEFAULT 0,
    vat_amount            REAL NOT NULL DEFAULT 0,
    line_total            REAL NOT NULL,
    PRIMARY KEY (transaction_id, line_no)
) WITHOUT ROWID;

CREATE TABLE sale_payments (
    transaction_id      TEXT NOT NULL REFERENCES sales(transaction_id) ON DELETE CASCADE,
    idx                 INTEGER NOT NULL,
    method              TEXT NOT NULL,
    amount              REAL NOT NULL,
    tendered            REAL,
    change_amount       REAL,
    confirmation_status TEXT NOT NULL,
    reference           TEXT,
    PRIMARY KEY (transaction_id, idx)
) WITHOUT ROWID;

-- The upload queue. `seq` is creation order. Rows are deleted ONLY by cleanup,
-- and only when status = 'synced' and older than retention.
CREATE TABLE outbox (
    seq                   INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id        TEXT NOT NULL UNIQUE,
    kind                  TEXT NOT NULL CHECK (kind IN ('sale', 'shift_open', 'shift_close')),
    status                TEXT NOT NULL CHECK (status IN ('pending', 'failed_transient', 'synced', 'failed')),
    payload_json          TEXT NOT NULL,
    attempts              INTEGER NOT NULL DEFAULT 0,
    next_attempt_at       TEXT,
    last_error            TEXT,
    sync_result           TEXT,
    server_id             TEXT,
    server_receipt_number TEXT,
    flags_json            TEXT,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    synced_at             TEXT
);
CREATE INDEX outbox_status ON outbox(status, seq);
CREATE INDEX outbox_synced ON outbox(synced_at) WHERE status = 'synced';

CREATE TABLE audit_log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    kind           TEXT NOT NULL,
    acting_user_id TEXT,
    detail_json    TEXT NOT NULL,
    device_time    TEXT NOT NULL,
    sent_at        TEXT
);
CREATE INDEX audit_unsent ON audit_log(id) WHERE sent_at IS NULL;
"#;

/// Schema 2: returns (refunds). The outbox `kind` CHECK gains 'return'; SQLite
/// cannot alter a CHECK, so the table is rebuilt with every row and its `seq`.
/// The AUTOINCREMENT counter is carried over explicitly: cleanup may have
/// deleted the highest rows, and a new item must never reuse a `seq`.
const V2: &str = r#"
CREATE TABLE outbox_new (
    seq                   INTEGER PRIMARY KEY AUTOINCREMENT,
    transaction_id        TEXT NOT NULL UNIQUE,
    kind                  TEXT NOT NULL CHECK (kind IN ('sale', 'shift_open', 'shift_close', 'return')),
    status                TEXT NOT NULL CHECK (status IN ('pending', 'failed_transient', 'synced', 'failed')),
    payload_json          TEXT NOT NULL,
    attempts              INTEGER NOT NULL DEFAULT 0,
    next_attempt_at       TEXT,
    last_error            TEXT,
    sync_result           TEXT,
    server_id             TEXT,
    server_receipt_number TEXT,
    flags_json            TEXT,
    created_at            TEXT NOT NULL,
    updated_at            TEXT NOT NULL,
    synced_at             TEXT
);
INSERT INTO outbox_new(seq, transaction_id, kind, status, payload_json, attempts, next_attempt_at,
                       last_error, sync_result, server_id, server_receipt_number, flags_json,
                       created_at, updated_at, synced_at)
SELECT seq, transaction_id, kind, status, payload_json, attempts, next_attempt_at,
       last_error, sync_result, server_id, server_receipt_number, flags_json,
       created_at, updated_at, synced_at
FROM outbox;
DELETE FROM sqlite_sequence WHERE name = 'outbox_new';
INSERT INTO sqlite_sequence(name, seq)
SELECT 'outbox_new', MAX(v) FROM (
    SELECT seq AS v FROM sqlite_sequence WHERE name = 'outbox'
    UNION ALL SELECT MAX(seq) FROM outbox_new
) HAVING MAX(v) IS NOT NULL;
DROP TABLE outbox;
ALTER TABLE outbox_new RENAME TO outbox;
CREATE INDEX outbox_status ON outbox(status, seq);
CREATE INDEX outbox_synced ON outbox(synced_at) WHERE status = 'synced';

-- A return points at the local sale it refunds, and its lines at that sale's lines.
ALTER TABLE sales ADD COLUMN original_transaction_id TEXT;
CREATE INDEX sales_original ON sales(original_transaction_id) WHERE original_transaction_id IS NOT NULL;
ALTER TABLE sale_lines ADD COLUMN original_line_no INTEGER;
ALTER TABLE sale_lines ADD COLUMN restock INTEGER NOT NULL DEFAULT 0;
"#;
