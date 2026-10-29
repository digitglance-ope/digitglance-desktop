//! Catalogue, branch stock, customers and staff: the snapshot (replace all, one
//! transaction), the delta feed (upserts and deletes), and the till's lookups.
//!
//! Stock: `stock.on_hand` is ONLY ever the server's figure. Local sales write
//! `local_stock_delta` rows; the till shows `on_hand + SUM(open deltas)`. When a
//! server stock row arrives after the sale was accepted, that row already
//! includes the sale, so the accepted deltas for that product are marked
//! reconciled and drop out of the sum. Pending deltas always count.

use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Serialize;
use serde_json::{json, Value};

use crate::store::db::{self, now_str, Store};
use crate::store::error::{Result, StoreError};

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotResult {
    pub products: u64,
    pub categories: u64,
    pub stock: u64,
    pub customers: u64,
    pub staff: u64,
    pub cursor: Value,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangesResult {
    pub applied: u64,
    pub skipped: u64,
    pub cursor: Value,
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str)
}
fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(Value::as_f64)
}
fn b(v: &Value, k: &str, dflt: bool) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(dflt)
}
fn req_id<'a>(v: &'a Value, what: &str) -> Result<&'a str> {
    s(v, "id")
        .filter(|x| !x.is_empty())
        .ok_or_else(|| StoreError::invalid(format!("A {what} row has no id.")))
}
fn lc(x: Option<&str>) -> Option<String> {
    x.map(|v| v.trim().to_lowercase()).filter(|v| !v.is_empty())
}

/// Words of a name, lower case, split on anything that is not a letter or digit.
pub fn terms_of(name: &str) -> Vec<String> {
    let mut out: Vec<String> = name
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 'service' or 'stock'; anything else, or missing (an older server), is 'stock'.
fn item_type_of(p: &Value) -> &'static str {
    match s(p, "itemType") {
        Some("service") => "service",
        _ => "stock",
    }
}

fn upsert_product(tx: &Connection, p: &Value) -> Result<()> {
    let id = req_id(p, "product")?;
    let name = s(p, "name").unwrap_or("").to_string();
    let name_lc = name.to_lowercase();
    let sku = s(p, "sku");
    let barcode = s(p, "barcode");
    tx.execute(
        "INSERT INTO products(id, name, name_lc, sku, sku_lc, barcode, barcode_lc, category_id,
                              selling_price, is_active, data_json, updated_at, item_type,
                              sold_by_fraction)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, name_lc = excluded.name_lc,
             sku = excluded.sku, sku_lc = excluded.sku_lc, barcode = excluded.barcode,
             barcode_lc = excluded.barcode_lc, category_id = excluded.category_id,
             selling_price = excluded.selling_price, is_active = excluded.is_active,
             data_json = excluded.data_json, updated_at = excluded.updated_at,
             item_type = excluded.item_type, sold_by_fraction = excluded.sold_by_fraction",
        params![
            id,
            name,
            name_lc,
            sku,
            lc(sku),
            barcode,
            lc(barcode),
            s(p, "categoryId"),
            f(p, "sellingPrice"),
            b(p, "isActive", true) as i64,
            serde_json::to_string(p)?,
            s(p, "updatedAt"),
            item_type_of(p),
            // Only a JSON `true` sells part units; missing (older server) is whole units.
            b(p, "soldByFraction", false) as i64,
        ],
    )?;
    tx.execute("DELETE FROM product_terms WHERE product_id = ?1", [id])?;
    let mut terms = terms_of(&name);
    terms.extend(lc(sku));
    terms.extend(lc(barcode));
    let mut ins =
        tx.prepare_cached("INSERT OR IGNORE INTO product_terms(term, product_id) VALUES (?1, ?2)")?;
    for t in terms {
        ins.execute(params![t, id])?;
    }
    Ok(())
}

fn upsert_category(tx: &Connection, c: &Value) -> Result<()> {
    let id = req_id(c, "category")?;
    let name = s(c, "name").unwrap_or("");
    tx.execute(
        "INSERT INTO categories(id, name, parent_id, sort_name, data_json) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, parent_id = excluded.parent_id,
             sort_name = excluded.sort_name, data_json = excluded.data_json",
        params![id, name, s(c, "parentId"), name.to_lowercase(), serde_json::to_string(c)?],
    )?;
    Ok(())
}

/// Server stock row: overwrite the server copy, then retire the accepted local
/// deltas it now includes. Rows for another branch are ignored (defence in depth).
fn upsert_stock(tx: &Connection, st: &Value, device_branch: Option<&str>) -> Result<bool> {
    let product_id = s(st, "productId")
        .filter(|x| !x.is_empty())
        .ok_or_else(|| StoreError::invalid("A stock row has no productId."))?;
    if let (Some(dev), Some(row)) = (device_branch, s(st, "branchId")) {
        if dev != row {
            return Ok(false);
        }
    }
    tx.execute(
        "INSERT INTO stock(product_id, stock_id, branch_id, on_hand, reserved, reorder_level, data_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(product_id) DO UPDATE SET stock_id = excluded.stock_id, branch_id = excluded.branch_id,
             on_hand = excluded.on_hand, reserved = excluded.reserved, reorder_level = excluded.reorder_level,
             data_json = excluded.data_json, updated_at = excluded.updated_at",
        params![
            product_id,
            s(st, "id"),
            s(st, "branchId"),
            f(st, "onHand").unwrap_or(0.0),
            f(st, "reserved").unwrap_or(0.0),
            f(st, "reorderLevel"),
            serde_json::to_string(st)?,
            s(st, "updatedAt"),
        ],
    )?;
    tx.execute(
        "UPDATE local_stock_delta SET reconciled_at = ?2
         WHERE product_id = ?1 AND synced_at IS NOT NULL AND reconciled_at IS NULL",
        params![product_id, now_str()],
    )?;
    Ok(true)
}

fn upsert_customer(tx: &Connection, c: &Value) -> Result<()> {
    let id = req_id(c, "customer")?;
    let name = s(c, "name").unwrap_or("");
    tx.execute(
        "INSERT INTO customers(id, name, name_lc, phone, loyalty_number_lc, is_active, data_json, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET name = excluded.name, name_lc = excluded.name_lc,
             phone = excluded.phone, loyalty_number_lc = excluded.loyalty_number_lc,
             is_active = excluded.is_active, data_json = excluded.data_json, updated_at = excluded.updated_at",
        params![
            id,
            name,
            name.to_lowercase(),
            s(c, "phone").map(|p| p.trim().to_string()),
            lc(s(c, "loyaltyNumber")),
            b(c, "isActive", true) as i64,
            serde_json::to_string(c)?,
            s(c, "updatedAt"),
        ],
    )?;
    Ok(())
}

fn upsert_staff(tx: &Connection, m: &Value) -> Result<()> {
    let id = req_id(m, "staff")?;
    let perms = m.get("permissions").cloned().unwrap_or_else(|| json!([]));
    let allowed = m.get("allowedTerminalIds").filter(|v| !v.is_null());
    tx.execute(
        "INSERT INTO staff(id, membership_id, name, role, pin_hash, permissions_json,
                           allowed_terminal_ids_json, active, data_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(id) DO UPDATE SET membership_id = excluded.membership_id, name = excluded.name,
             role = excluded.role, pin_hash = excluded.pin_hash, permissions_json = excluded.permissions_json,
             allowed_terminal_ids_json = excluded.allowed_terminal_ids_json, active = excluded.active,
             data_json = excluded.data_json",
        params![
            id,
            s(m, "membershipId"),
            s(m, "name").unwrap_or(""),
            s(m, "role"),
            s(m, "pinHash").filter(|h| !h.is_empty()),
            serde_json::to_string(&perms)?,
            allowed.map(serde_json::to_string).transpose()?,
            b(m, "active", true) as i64,
            serde_json::to_string(&strip_hash(m))?,
        ],
    )?;
    Ok(())
}

/// Never keep a second copy of the PIN hash in the JSON blob.
fn strip_hash(m: &Value) -> Value {
    let mut c = m.clone();
    if let Some(o) = c.as_object_mut() {
        o.remove("pinHash");
    }
    c
}

fn device_branch(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT branch_id FROM device_identity WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}

fn arr<'a>(snap: &'a Value, k: &str) -> Result<&'a Vec<Value>> {
    snap.get(k).and_then(Value::as_array).ok_or_else(|| {
        StoreError::invalid(format!(
            "The snapshot has no '{k}' array; nothing was replaced."
        ))
    })
}

/// Settings blocks the snapshot carries, stored under the same key names.
pub const SNAPSHOT_SETTING_KEYS: &[&str] = &[
    "settings",
    "offlineSettings",
    "approvalSettings",
    "business",
    "branch",
    "terminal",
    "device",
    "loyalty",
];

pub fn apply_snapshot(store: &mut Store, snap: &Value) -> Result<SnapshotResult> {
    if !snap.is_object() {
        return Err(StoreError::invalid("The snapshot must be an object."));
    }
    let products = arr(snap, "products")?;
    let categories = arr(snap, "categories")?;
    let stock = arr(snap, "stock")?;
    let customers = arr(snap, "customers")?;
    let staff = arr(snap, "staff")?;
    let cursor = snap.get("cursor").cloned().unwrap_or(Value::Null);

    let tx = store.conn.transaction()?;
    let branch = device_branch(&tx)?;
    tx.execute_batch(
        "DELETE FROM product_terms; DELETE FROM products; DELETE FROM categories;
         DELETE FROM stock; DELETE FROM customers; DELETE FROM staff;",
    )?;
    let mut r = SnapshotResult {
        cursor: cursor.clone(),
        ..Default::default()
    };
    for c in categories {
        upsert_category(&tx, c)?;
        r.categories += 1;
    }
    for p in products {
        upsert_product(&tx, p)?;
        r.products += 1;
    }
    for st in stock {
        if upsert_stock(&tx, st, branch.as_deref())? {
            r.stock += 1;
        }
    }
    // The snapshot is server truth taken after everything already accepted.
    tx.execute(
        "UPDATE local_stock_delta SET reconciled_at = ?1 WHERE synced_at IS NOT NULL AND reconciled_at IS NULL",
        [now_str()],
    )?;
    for c in customers {
        upsert_customer(&tx, c)?;
        r.customers += 1;
    }
    for m in staff {
        upsert_staff(&tx, m)?;
        r.staff += 1;
    }
    for k in SNAPSHOT_SETTING_KEYS {
        if let Some(v) = snap.get(*k).filter(|v| !v.is_null()) {
            db::settings_set(&tx, k, v)?;
        }
    }
    db::meta_set(&tx, "catalog_cursor", &serde_json::to_string(&cursor)?)?;
    db::meta_set(&tx, "last_snapshot_at", &now_str())?;
    tx.commit()?;
    Ok(r)
}

/// Setting-type entities in the delta feed, and the settings key each goes to.
fn settings_key_for(entity: &str) -> Option<&'static str> {
    match entity {
        "settings" => Some("settings"),
        "vat" => Some("vat"),
        "approval_settings" => Some("approvalSettings"),
        "offline_settings" => Some("offlineSettings"),
        "loyalty_settings" => Some("loyalty"),
        _ => None,
    }
}

fn apply_change(tx: &Transaction, row: &Value, branch: Option<&str>) -> Result<bool> {
    let entity = s(row, "entity").unwrap_or("");
    let op = s(row, "op").unwrap_or("upsert");
    let entity_id = row
        .get("entityId")
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_default();
    let data = row.get("data").filter(|d| !d.is_null());
    if op == "delete" {
        match entity {
            "product" => {
                tx.execute("DELETE FROM products WHERE id = ?1", [&entity_id])?;
            }
            "category" => {
                tx.execute("DELETE FROM categories WHERE id = ?1", [&entity_id])?;
            }
            "stock" => {
                tx.execute(
                    "DELETE FROM stock WHERE stock_id = ?1 OR product_id = ?1",
                    [&entity_id],
                )?;
            }
            "customer" => {
                tx.execute("DELETE FROM customers WHERE id = ?1", [&entity_id])?;
            }
            "user" => {
                tx.execute(
                    "DELETE FROM staff WHERE id = ?1 OR membership_id = ?1",
                    [&entity_id],
                )?;
            }
            "price" => {}
            e if settings_key_for(e).is_some() => {
                tx.execute("DELETE FROM settings WHERE key = ?1", [settings_key_for(e)])?;
            }
            _ => return Ok(false),
        }
        return Ok(true);
    }
    if op != "upsert" {
        return Ok(false);
    }
    let Some(data) = data else { return Ok(false) };
    match entity {
        "product" => upsert_product(tx, data)?,
        "category" => upsert_category(tx, data)?,
        "stock" => return upsert_stock(tx, data, branch),
        "customer" => upsert_customer(tx, data)?,
        "user" => upsert_staff(tx, data)?,
        "price" => {
            let id = s(data, "id").map(str::to_string).unwrap_or(entity_id);
            if let Some(price) = f(data, "sellingPrice") {
                tx.execute(
                    "UPDATE products SET selling_price = ?2,
                         data_json = json_set(data_json, '$.sellingPrice', ?2)
                     WHERE id = ?1",
                    params![id, price],
                )?;
            }
        }
        e => match settings_key_for(e) {
            Some(k) => db::settings_set(tx, k, data)?,
            None => return Ok(false),
        },
    }
    Ok(true)
}

/// One delta page, in ONE transaction, and the cursor stored with it (so a
/// crash cannot store a cursor for changes that were not applied).
pub fn apply_changes(store: &mut Store, rows: &[Value], cursor: &Value) -> Result<ChangesResult> {
    let tx = store.conn.transaction()?;
    let branch = device_branch(&tx)?;
    let mut r = ChangesResult {
        cursor: cursor.clone(),
        ..Default::default()
    };
    for row in rows {
        if apply_change(&tx, row, branch.as_deref())? {
            r.applied += 1;
        } else {
            r.skipped += 1;
        }
    }
    if !cursor.is_null() {
        db::meta_set(&tx, "catalog_cursor", &serde_json::to_string(cursor)?)?;
    }
    db::meta_set(&tx, "last_changes_at", &now_str())?;
    tx.commit()?;
    Ok(r)
}

pub fn get_cursor(store: &Store) -> Result<Value> {
    Ok(match db::meta_get(&store.conn, "catalog_cursor")? {
        Some(s) => serde_json::from_str(&s)?,
        None => Value::Null,
    })
}

// ---------------------------------------------------------------- lookups

/// Upper bound for a prefix range scan: every string starting with `p` is
/// >= p and < p + U+10FFFF (UTF-8 byte order, BINARY collation).
fn prefix_hi(p: &str) -> String {
    let mut s = p.to_string();
    s.push('\u{10FFFF}');
    s
}

const PRODUCT_COLS: &str = "p.data_json, p.selling_price,
    s.on_hand, s.reserved, s.reorder_level,
    (SELECT COALESCE(SUM(d.qty_delta), 0) FROM local_stock_delta d
      WHERE d.product_id = p.id AND d.reconciled_at IS NULL) AS local_delta,
    s.product_id IS NOT NULL AS has_stock, p.item_type, p.sold_by_fraction";

fn product_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let data: String = r.get(0)?;
    let mut v: Value = serde_json::from_str(&data).unwrap_or_else(|_| json!({}));
    let server: Option<f64> = r.get(2)?;
    let delta: f64 = r.get(5)?;
    let has_stock: bool = r.get(6)?;
    if let Some(o) = v.as_object_mut() {
        if let Some(price) = r.get::<_, Option<f64>>(1)? {
            o.insert("sellingPrice".into(), json!(price));
        }
        // Always present (the columns, not the blob), so the till never guesses.
        o.insert("itemType".into(), json!(r.get::<_, String>(7)?));
        o.insert("soldByFraction".into(), json!(r.get::<_, bool>(8)?));
        o.insert(
            "stock".into(),
            json!({
                "onHand": server.unwrap_or(0.0) + delta,
                "serverOnHand": server,
                "localDelta": delta,
                "reserved": r.get::<_, Option<f64>>(3)?,
                "reorderLevel": r.get::<_, Option<f64>>(4)?,
                "tracked": has_stock,
            }),
        );
    }
    Ok(v)
}

pub fn clamp_limit(limit: Option<i64>, dflt: i64, max: i64) -> i64 {
    limit.unwrap_or(dflt).clamp(1, max)
}

/// Prefix search on name words, SKU and barcode (case-insensitive, indexed).
/// Extra words narrow the result (each must appear in the name, SKU or barcode).
/// Exact barcode / SKU matches sort first, then by name. Active products only.
pub fn search(
    store: &Store,
    q: &str,
    category_id: Option<&str>,
    limit: Option<i64>,
) -> Result<Vec<Value>> {
    let limit = clamp_limit(limit, 30, 200);
    let ql = q.trim().to_lowercase();
    // Same tokenising as the stored terms, so "coke-50" finds "Coke 50cl".
    let words: Vec<&str> = ql
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let conn = &store.conn;
    if words.is_empty() {
        let sql = format!(
            "SELECT {PRODUCT_COLS} FROM products p LEFT JOIN stock s ON s.product_id = p.id
             WHERE p.is_active = 1 AND (?1 IS NULL OR p.category_id = ?1)
             ORDER BY p.name_lc LIMIT ?2"
        );
        let mut st = conn.prepare_cached(&sql)?;
        let rows = st.query_map(params![category_id, limit], product_row)?;
        return Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?);
    }
    // The first word drives the index; a SKU/barcode may contain spaces, so the
    // whole query is also tried as one term.
    let first = words[0];
    let mut extra_sql = String::new();
    let mut extra: Vec<String> = Vec::new();
    for (i, w) in words.iter().enumerate().skip(1) {
        let n = 5 + i;
        extra_sql.push_str(&format!(
            " AND (p.name_lc LIKE ?{n} ESCAPE '\\' OR p.sku_lc LIKE ?{n} ESCAPE '\\' OR p.barcode_lc LIKE ?{n} ESCAPE '\\')"
        ));
        extra.push(format!("%{}%", like_escape(w)));
    }
    // Pick and order the ids first, THEN compute stock for the page only.
    let sql = format!(
        "SELECT {PRODUCT_COLS} FROM (
             SELECT p.id AS hid, (p.barcode_lc = ?3) AS k1, (p.sku_lc = ?3) AS k2, p.name_lc AS k3
             FROM products p
             WHERE p.id IN (SELECT product_id FROM product_terms WHERE term >= ?1 AND term < ?2
                            UNION SELECT id FROM products WHERE barcode_lc = ?3 OR sku_lc = ?3)
               AND p.is_active = 1 AND (?4 IS NULL OR p.category_id = ?4) {extra_sql}
             ORDER BY k1 DESC, k2 DESC, k3
             LIMIT ?5) h
         JOIN products p ON p.id = h.hid
         LEFT JOIN stock s ON s.product_id = p.id
         ORDER BY h.k1 DESC, h.k2 DESC, h.k3"
    );
    let mut st = conn.prepare_cached(&sql)?;
    let hi = prefix_hi(first);
    let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&first, &hi, &ql, &category_id, &limit];
    for e in &extra {
        bind.push(e);
    }
    let rows = st.query_map(bind.as_slice(), product_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Exact barcode (case-insensitive), falling back to an exact SKU. Null if none.
pub fn by_barcode(store: &Store, code: &str) -> Result<Option<Value>> {
    let c = code.trim().to_lowercase();
    if c.is_empty() {
        return Ok(None);
    }
    let sql = format!(
        "SELECT {PRODUCT_COLS} FROM products p LEFT JOIN stock s ON s.product_id = p.id
         WHERE p.barcode_lc = ?1 OR p.sku_lc = ?1
         ORDER BY (p.barcode_lc = ?1) DESC, p.is_active DESC LIMIT 1"
    );
    let mut st = store.conn.prepare_cached(&sql)?;
    Ok(st.query_row([&c], product_row).optional()?)
}

/// Products by id, in the order asked, skipping unknown ids. Includes inactive.
pub fn get_products(store: &Store, ids: &[String]) -> Result<Vec<Value>> {
    if ids.len() > 1000 {
        return Err(StoreError::invalid("At most 1000 ids per call."));
    }
    let sql = format!(
        "SELECT {PRODUCT_COLS} FROM products p LEFT JOIN stock s ON s.product_id = p.id WHERE p.id = ?1"
    );
    let mut st = store.conn.prepare_cached(&sql)?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(v) = st.query_row([id], product_row).optional()? {
            out.push(v);
        }
    }
    Ok(out)
}

pub fn categories_list(store: &Store) -> Result<Vec<Value>> {
    let mut st = store
        .conn
        .prepare_cached("SELECT data_json FROM categories ORDER BY sort_name")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(serde_json::from_str(&r?)?);
    }
    Ok(out)
}

/// Name prefix (or word prefix), phone prefix, or exact loyalty number.
pub fn customers_search(store: &Store, q: &str, limit: Option<i64>) -> Result<Vec<Value>> {
    let limit = clamp_limit(limit, 20, 100);
    let ql = q.trim().to_lowercase();
    let mut st;
    let rows: Vec<String> = if ql.is_empty() {
        st = store.conn.prepare_cached(
            "SELECT data_json FROM customers WHERE is_active = 1 ORDER BY name_lc LIMIT ?1",
        )?;
        st.query_map([limit], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    } else {
        st = store.conn.prepare_cached(
            "SELECT data_json FROM customers
             WHERE is_active = 1 AND (
                   (name_lc >= ?1 AND name_lc < ?2)
                OR (phone >= ?3 AND phone < ?4)
                OR loyalty_number_lc = ?1
                OR name_lc LIKE ?5 ESCAPE '\\')
             ORDER BY (name_lc >= ?1 AND name_lc < ?2) DESC, name_lc LIMIT ?6",
        )?;
        let raw = q.trim();
        st.query_map(
            params![
                ql,
                prefix_hi(&ql),
                raw,
                prefix_hi(raw),
                format!("% {}%", like_escape(&ql)),
                limit
            ],
            |r| r.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?
    };
    rows.iter().map(|s| Ok(serde_json::from_str(s)?)).collect()
}

pub fn customer_get(store: &Store, id: &str) -> Result<Option<Value>> {
    let raw: Option<String> = store
        .conn
        .query_row("SELECT data_json FROM customers WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(match raw {
        Some(s) => Some(serde_json::from_str(&s)?),
        None => None,
    })
}

/// Server copy + open local deltas, for one product (tests and diagnostics).
pub fn displayed_on_hand(conn: &Connection, product_id: &str) -> Result<f64> {
    Ok(conn.query_row(
        "SELECT COALESCE((SELECT on_hand FROM stock WHERE product_id = ?1), 0)
              + COALESCE((SELECT SUM(qty_delta) FROM local_stock_delta
                          WHERE product_id = ?1 AND reconciled_at IS NULL), 0)",
        [product_id],
        |r| r.get(0),
    )?)
}
