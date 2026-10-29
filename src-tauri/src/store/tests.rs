//! Store tests. Run: `cargo test --lib store::` (SQLCipher build, the default).
//! On a machine without a native Perl: `cargo test --lib --no-default-features store::`
//! runs everything except the encryption tests, which say why they are skipped.
//! Timings: `cargo test --lib store::tests::perf -- --ignored --nocapture`.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use rusqlite::Connection;
use serde_json::{json, Value};

use super::crypto_key::{self, DbKey, KeyStore, MemKeyStore, OsKeyStore};
use super::db::{self, failpoint, ts, Store};
use super::error::ErrorCode;
use super::{audit, catalog, cleanup, identity, migrations, queue, sales, staff, storage};

const OWNER: &str = "0b7e5c1a-1111-4aaa-9bbb-000000000001";
const DEVICE: &str = "6f1c0000-2222-4ccc-8ddd-000000000002";
const KEY: DbKey = [7u8; 32];
const GB: u64 = 1 << 30;
const MB: u64 = 1 << 20;

fn ok_probe() -> storage::StorageProbe {
    Box::new(|_| Ok((50 * GB, 100 * GB)))
}
fn critical_probe() -> storage::StorageProbe {
    Box::new(|_| Ok((100 * MB, 100 * GB)))
}

fn identity_input() -> identity::DeviceIdentityInput {
    identity::DeviceIdentityInput {
        owner_id: OWNER.into(),
        device_id: DEVICE.into(),
        device_code: "T01".into(),
        branch_id: Some("branch-ikeja".into()),
        terminal_id: Some("till-1".into()),
        device_token: "eyJhbGciOiJIUzI1NiJ9.test.token".into(),
        api_base: "https://api.digitglance.com".into(),
        device_secret: "s".repeat(48),
    }
}

/// A fresh store in `dir` with the fixed test key and a saved identity.
fn open_test_store(dir: &Path) -> Store {
    let mut s = Store::open_with_key(
        &db::db_path(dir, OWNER, DEVICE),
        OWNER,
        DEVICE,
        &KEY,
        Arc::new(MemKeyStore::default()),
    )
    .expect("open");
    s.set_storage_probe(ok_probe());
    identity::save(&mut s, dir, &identity_input()).expect("identity");
    s
}

fn reopen(dir: &Path) -> Store {
    let mut s = Store::open_with_key(
        &db::db_path(dir, OWNER, DEVICE),
        OWNER,
        DEVICE,
        &KEY,
        Arc::new(MemKeyStore::default()),
    )
    .expect("reopen");
    s.set_storage_probe(ok_probe());
    s
}

fn tid() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn sale(tid: &str, lines: &[(&str, f64)]) -> Value {
    let lines: Vec<Value> = lines
        .iter()
        .map(|(pid, qty)| {
            json!({ "productId": pid, "productSnapshot": { "name": format!("Product {pid}"), "sku": pid },
                    "qty": qty, "unitPrice": 300.0, "discount": 0.0, "vatRate": 7.5,
                    "vatAmount": 22.5 * qty, "lineTotal": 322.5 * qty })
        })
        .collect();
    json!({
        "transactionId": tid, "kind": "sale", "recordedAt": "2026-10-27T09:00:00.000Z",
        "cashierId": "cashier-1", "approvals": [], "branchId": "branch-ikeja", "terminalId": "till-1",
        "shiftLocalId": "shift-1", "customerId": null, "lines": lines,
        "payments": [{ "method": "cash", "amount": 1000.0, "tendered": 1000.0, "change": 0.0,
                       "confirmationStatus": "confirmed" }],
        "totals": { "subtotal": 900.0, "vat": 67.5, "total": 967.5 }, "notes": null
    })
}

fn count(s: &Store, table: &str) -> i64 {
    s.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn receipt_seq(s: &Store) -> i64 {
    s.conn()
        .query_row(
            "SELECT value FROM sequences WHERE name = 'receipt'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn audit_count(s: &Store, kind: &str) -> i64 {
    s.conn()
        .query_row(
            "SELECT count(*) FROM audit_log WHERE kind = ?1",
            [kind],
            |r| r.get(0),
        )
        .unwrap()
}

fn assert_nothing_written(s: &Store) {
    for t in [
        "sales",
        "sale_lines",
        "sale_payments",
        "outbox",
        "local_stock_delta",
    ] {
        assert_eq!(count(s, t), 0, "{t} should be empty");
    }
    assert_eq!(receipt_seq(s), 0, "receipt sequence must not move");
    assert_eq!(audit_count(s, "sale_recorded"), 0);
}

fn mark(s: &mut Store, tid: &str, status: &str) -> queue::MarkSummary {
    queue::mark(
        s,
        &[queue::MarkResult {
            transaction_id: tid.into(),
            status: status.into(),
            server_id: Some(format!("srv-{tid}")),
            receipt_number: None,
            reason: Some("test".into()),
            flags: None,
        }],
    )
    .unwrap()
}

fn set_synced_days_ago(s: &Store, tid: &str, days: i64) {
    let t = ts(Utc::now() - Duration::days(days));
    s.conn()
        .execute(
            "UPDATE outbox SET synced_at = ?2, created_at = ?2 WHERE transaction_id = ?1",
            rusqlite::params![tid, t],
        )
        .unwrap();
}

fn snapshot(products: &[(&str, &str, Option<&str>, f64)]) -> Value {
    let ps: Vec<Value> = products
        .iter()
        .map(|(id, name, barcode, _)| json!({ "id": id, "sku": id.to_uppercase(), "barcode": barcode, "name": name,
                                              "categoryId": "cat-drinks", "sellingPrice": 300, "isActive": true }))
        .collect();
    let stock: Vec<Value> = products
        .iter()
        .map(|(id, _, _, on_hand)| {
            json!({ "id": format!("stk-{id}"), "productId": id, "branchId": "branch-ikeja",
                                           "onHand": on_hand, "reserved": 0 })
        })
        .collect();
    json!({
        "cursor": 100,
        "settings": { "vatRate": 7.5, "currency": "NGN", "receiptFooter": "Thank you" },
        "offlineSettings": { "retentionDays": 30, "storageWarningPct": 15, "storageWarningMb": 500,
                             "storageCriticalPct": 5, "storageCriticalMb": 150 },
        "categories": [{ "id": "cat-drinks", "name": "Drinks", "parentId": null }],
        "products": ps,
        "stock": stock,
        "customers": [{ "id": "cust-1", "name": "Ada Okafor", "phone": "08030000000", "loyaltyNumber": "L-00012",
                        "isActive": true, "loyaltyPoints": 120 }],
        "staff": [{ "id": "cashier-1", "membershipId": "m-1", "name": "Ikeja Cashier", "role": "cashier",
                    "pinHash": bcrypt::hash("1234", 4).unwrap(), "permissions": ["pos.view", "pos.create"],
                    "allowedTerminalIds": null, "active": true }]
    })
}

// ---------------------------------------------------------------- migrations

#[test]
fn migrations_up_from_empty_and_idempotent() {
    let mut c = Connection::open_in_memory().unwrap();
    assert_eq!(
        migrations::migrate(&mut c).unwrap(),
        migrations::latest_version()
    );
    for t in [
        "meta",
        "settings",
        "device_identity",
        "products",
        "product_terms",
        "stock",
        "local_stock_delta",
        "customers",
        "staff",
        "pin_attempts",
        "sequences",
        "shifts",
        "sales",
        "sale_lines",
        "sale_payments",
        "outbox",
        "audit_log",
    ] {
        let n: i64 = c
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [t],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "table {t}");
    }
    // Re-running is a no-op.
    assert_eq!(
        migrations::migrate(&mut c).unwrap(),
        migrations::latest_version()
    );
    let applied: i64 = c
        .query_row("SELECT count(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(applied, migrations::latest_version());
}

#[test]
fn migrations_apply_only_newer_steps_and_refuse_a_newer_db() {
    let mut c = Connection::open_in_memory().unwrap();
    migrations::migrate_with(&mut c, &migrations::MIGRATIONS[..1]).unwrap();
    let next = [
        migrations::Migration {
            version: 1,
            name: "initial",
            sql: "SELECT 1",
        },
        migrations::Migration {
            version: 2,
            name: "add_col",
            sql: "ALTER TABLE meta ADD COLUMN note TEXT;",
        },
    ];
    assert_eq!(migrations::migrate_with(&mut c, &next).unwrap(), 2);
    c.execute(
        "INSERT INTO meta(key, value, note) VALUES ('k', 'v', 'n')",
        [],
    )
    .unwrap();
    // A database written by a newer app is refused, untouched.
    c.execute(
        "INSERT INTO schema_migrations VALUES (99, 'future', 'x')",
        [],
    )
    .unwrap();
    let e = migrations::migrate(&mut c).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
}

#[test]
fn store_open_sets_wal_full_and_binds_tenant() {
    let d = tempfile::tempdir().unwrap();
    let s = open_test_store(d.path());
    let mode: String = s
        .conn()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let sync: i64 = s
        .conn()
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 2, "synchronous=FULL");
    let av: i64 = s
        .conn()
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(av, 2, "auto_vacuum=INCREMENTAL");
    drop(s);
    // The same file opened as another business is refused.
    let e = Store::open_with_key(
        &db::db_path(d.path(), OWNER, DEVICE),
        "another-owner",
        DEVICE,
        &KEY,
        Arc::new(MemKeyStore::default()),
    )
    .err()
    .unwrap();
    assert_eq!(e.code, ErrorCode::Crypto);
    // One file per (owner, device).
    assert_ne!(
        db::db_path(d.path(), "a", "d"),
        db::db_path(d.path(), "b", "d")
    );
    assert!(db::validate_id("ownerId", "../evil").is_err());
}

// ---------------------------------------------------------------- encryption and keys

#[test]
#[cfg_attr(
    not(feature = "sqlcipher"),
    ignore = "needs the SQLCipher build (default feature `sqlcipher`)"
)]
fn encryption_wrong_key_and_copied_file_are_unreadable() {
    let d = tempfile::tempdir().unwrap();
    let path = db::db_path(d.path(), OWNER, DEVICE);
    {
        let mut s = open_test_store(d.path());
        let cipher: String = s
            .conn()
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .unwrap();
        assert!(!cipher.is_empty());
        let mut snap = snapshot(&[("p1", "PLAINTEXT-MARKER Cola", Some("6001"), 5.0)]);
        snap["customers"][0]["name"] = json!("PLAINTEXT-MARKER Customer");
        catalog::apply_snapshot(&mut s, &snap).unwrap();
        s.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
    }
    // Nothing readable on disk.
    for f in [
        path.clone(),
        PathBuf::from(format!("{}-wal", path.display())),
    ] {
        if let Ok(bytes) = std::fs::read(&f) {
            assert!(
                !bytes.windows(16).any(|w| w == b"PLAINTEXT-MARKER"),
                "{} leaks plaintext",
                f.display()
            );
            assert!(
                !bytes.starts_with(b"SQLite format 3"),
                "{} has a plain SQLite header",
                f.display()
            );
        }
    }
    // Wrong key.
    let other: DbKey = [9u8; 32];
    let e = Store::open_with_key(
        &path,
        OWNER,
        DEVICE,
        &other,
        Arc::new(MemKeyStore::default()),
    )
    .err()
    .unwrap();
    assert_eq!(e.code, ErrorCode::Crypto);
    // No key at all (plain SQLite open).
    let plain = Connection::open(&path).unwrap();
    assert!(plain
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r
            .get::<_, i64>(0))
        .is_err());
    drop(plain);
    // A copy taken off the machine, opened with another key.
    let copy = d.path().join("copy.db");
    std::fs::copy(&path, &copy).unwrap();
    let e = Store::open_with_key(
        &copy,
        OWNER,
        DEVICE,
        &other,
        Arc::new(MemKeyStore::default()),
    )
    .err()
    .unwrap();
    assert_eq!(e.code, ErrorCode::Crypto);
    // ... but the right key still opens the original.
    let s = reopen(d.path());
    assert_eq!(count(&s, "products"), 1);
}

#[test]
fn key_is_created_once_and_never_silently_replaced() {
    let d = tempfile::tempdir().unwrap();
    let ks: Arc<MemKeyStore> = Arc::new(MemKeyStore::default());
    let s = Store::open(d.path(), OWNER, DEVICE, ks.clone()).unwrap();
    drop(s);
    let account = crypto_key::db_account(OWNER, DEVICE);
    assert_eq!(account, format!("till-db:{OWNER}:{DEVICE}"));
    let k1 = ks.get(&account).unwrap().unwrap();
    assert_eq!(k1.len(), 64);
    Store::open(d.path(), OWNER, DEVICE, ks.clone()).unwrap();
    assert_eq!(ks.get(&account).unwrap().unwrap(), k1, "same key on reopen");
    // Key gone from the credential store while the file exists: refuse, do not regenerate.
    ks.delete(&account).unwrap();
    let e = Store::open(d.path(), OWNER, DEVICE, ks.clone())
        .err()
        .unwrap();
    assert_eq!(e.code, ErrorCode::Crypto);
    assert!(ks.get(&account).unwrap().is_none());
    // Two keys are never equal (CSPRNG).
    assert_ne!(
        crypto_key::generate_key().unwrap(),
        crypto_key::generate_key().unwrap()
    );
}

#[test]
fn keyring_round_trip_os_credential_store() {
    let service = format!("DigitGlance Trade Test {}", uuid::Uuid::now_v7());
    let ks = OsKeyStore::new(service.clone());
    let account = crypto_key::db_account(OWNER, DEVICE);
    if let Err(e) = ks.set(&account, "probe") {
        eprintln!("SKIPPED keyring_round_trip_os_credential_store: the OS credential store is unavailable here ({e}).");
        return;
    }
    assert_eq!(ks.get(&account).unwrap().as_deref(), Some("probe"));
    ks.delete(&account).unwrap();
    assert_eq!(ks.get(&account).unwrap(), None);
    // The real key path through the real credential store.
    let k = crypto_key::get_or_create_db_key(&ks, OWNER, DEVICE, true).unwrap();
    assert_eq!(
        crypto_key::get_or_create_db_key(&ks, OWNER, DEVICE, false).unwrap(),
        k
    );
    ks.delete(&account).unwrap();
    eprintln!("keyring round trip OK against the OS credential store (service '{service}').");
}

// ---------------------------------------------------------------- identity

#[test]
fn identity_save_get_clear_and_reopen_from_pointer() {
    let d = tempfile::tempdir().unwrap();
    let ks: Arc<MemKeyStore> = Arc::new(MemKeyStore::default());
    let mut s = Store::open(d.path(), OWNER, DEVICE, ks.clone()).unwrap();
    assert!(identity::get(&s).unwrap().is_none());
    let mut bad = identity_input();
    bad.device_secret = "short".into();
    assert_eq!(
        identity::save(&mut s, d.path(), &bad).unwrap_err().code,
        ErrorCode::Invalid
    );
    let mut wrong = identity_input();
    wrong.owner_id = "someone-else".into();
    assert_eq!(
        identity::save(&mut s, d.path(), &wrong).unwrap_err().code,
        ErrorCode::Invalid
    );

    identity::save(&mut s, d.path(), &identity_input()).unwrap();
    let got = identity::get(&s).unwrap().unwrap();
    assert_eq!(got.device_code, "T01");
    assert_eq!(got.device_token, identity_input().device_token);
    assert_eq!(identity::secret(&s).unwrap().unwrap(), "s".repeat(48));
    // Token and secret are in the credential store, not the DB.
    let in_db: i64 = s
        .conn()
        .query_row("SELECT count(*) FROM device_identity WHERE api_base LIKE '%eyJ%' OR device_code LIKE '%sss%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(in_db, 0);
    assert!(ks
        .get(&crypto_key::token_account(OWNER, DEVICE))
        .unwrap()
        .is_some());
    drop(s);

    // Launch: the pointer reopens the right store.
    let s = identity::open_from_pointer(d.path(), ks.clone())
        .unwrap()
        .unwrap();
    assert_eq!(identity::get(&s).unwrap().unwrap(), got);
    let mut s = s;
    identity::clear(&mut s, d.path()).unwrap();
    assert!(identity::get(&s).unwrap().is_none());
    assert!(ks
        .get(&crypto_key::secret_account(OWNER, DEVICE))
        .unwrap()
        .is_none());
    assert!(identity::open_from_pointer(d.path(), ks).unwrap().is_none());
}

// ---------------------------------------------------------------- sales

#[test]
fn sale_commit_writes_everything_once() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let t1 = tid();
    let r1 = sales::sale_commit(&mut s, sale(&t1, &[("p1", 2.0), ("p2", 1.0)])).unwrap();
    assert_eq!(
        r1,
        sales::CommitResult {
            receipt_number: "T01-000001".into(),
            local_seq: 1
        }
    );
    let r2 = sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)])).unwrap();
    assert_eq!(r2.receipt_number, "T01-000002");
    // A retried commit of the same transaction returns the same receipt, writes nothing.
    assert_eq!(
        sales::sale_commit(&mut s, sale(&t1, &[("p1", 2.0)])).unwrap(),
        r1
    );
    assert_eq!(count(&s, "sales"), 2);
    assert_eq!(count(&s, "sale_lines"), 3);
    assert_eq!(count(&s, "sale_payments"), 2);
    assert_eq!(count(&s, "outbox"), 2);
    assert_eq!(count(&s, "local_stock_delta"), 3);
    assert_eq!(audit_count(&s, "sale_recorded"), 2);
    let item = &queue::next_batch(&s, Some(10)).unwrap()[0];
    assert_eq!(item.status, "pending");
    assert_eq!(item.payload["receiptNumber"], "T01-000001");
    assert_eq!(item.payload["transactionId"], t1.as_str());
    assert_eq!(item.payload["lines"][0]["qty"], 2.0);
    let got = sales::sale_get(&s, &t1).unwrap();
    assert_eq!(got["receiptNumber"], "T01-000001");
    assert_eq!(got["sync"]["status"], "pending");
    assert_eq!(
        sales::sales_recent(&s, Some(10), None, None, None, None).unwrap()[0].receipt_number,
        "T01-000002"
    );
    assert_eq!(
        sales::sale_get(&s, &tid()).unwrap_err().code,
        ErrorCode::NotFound
    );
    // Validation.
    let mut bad = sale(&tid(), &[("p1", 1.0)]);
    bad["lines"] = json!([]);
    assert_eq!(
        sales::sale_commit(&mut s, bad).unwrap_err().code,
        ErrorCode::Invalid
    );
    assert_eq!(
        sales::sale_commit(&mut s, sale("not-a-uuid", &[("p1", 1.0)]))
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    assert_eq!(count(&s, "sales"), 2);
}

#[test]
fn sale_commit_is_atomic_when_it_panics_between_statements() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    for fp in [
        "sale_after_seq",
        "sale_after_header",
        "sale_after_lines",
        "sale_after_outbox",
        "sale_before_commit",
    ] {
        failpoint::set_panic(Some(fp));
        let r = catch_unwind(AssertUnwindSafe(|| {
            sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0), ("p2", 3.0)]))
        }));
        failpoint::set_panic(None);
        assert!(r.is_err(), "failpoint {fp} should have fired");
        assert_nothing_written(&s);
    }
    // The store is still usable and the number was not burnt.
    let r = sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)])).unwrap();
    assert_eq!(r.receipt_number, "T01-000001");
}

/// Child half of the crash test: only does anything when the parent set the env.
#[test]
#[ignore = "child process of sale_commit_survives_a_process_crash"]
fn crash_child() {
    let Ok(dir) = std::env::var("DG_CRASH_DIR") else {
        return;
    };
    let mut s = reopen(Path::new(&dir));
    let t = std::env::var("DG_CRASH_TID").unwrap();
    let _ = sales::sale_commit(&mut s, sale(&t, &[("p1", 1.0), ("p2", 2.0)]));
    // A failpoint aborts the process before this line.
    std::process::exit(0);
}

#[test]
fn sale_commit_survives_a_process_crash() {
    let exe = std::env::current_exe().unwrap();
    for (fp, expect_written) in [
        ("sale_after_lines", false),
        ("sale_after_outbox", false),
        ("sale_before_commit", false),
        ("sale_after_commit", true),
    ] {
        let d = tempfile::tempdir().unwrap();
        drop(open_test_store(d.path()));
        let t = tid();
        let out = std::process::Command::new(&exe)
            .args([
                "store::tests::crash_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("DG_CRASH_DIR", d.path())
            .env("DG_CRASH_TID", &t)
            .env("DG_FAILPOINT", fp)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "child should have crashed at {fp}: {out:?}"
        );
        let s = reopen(d.path());
        if expect_written {
            assert_eq!(count(&s, "sales"), 1, "{fp}");
            assert_eq!(count(&s, "sale_lines"), 2);
            assert_eq!(count(&s, "sale_payments"), 1);
            assert_eq!(count(&s, "outbox"), 1);
            assert_eq!(count(&s, "local_stock_delta"), 2);
            assert_eq!(receipt_seq(&s), 1);
            assert_eq!(
                sales::sale_get(&s, &t).unwrap()["receiptNumber"],
                "T01-000001"
            );
        } else {
            assert_nothing_written(&s);
        }
        let ok: String = s
            .conn()
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
    }
}

#[test]
fn storage_critical_refuses_and_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    s.set_storage_probe(critical_probe());
    let audits_before = count(&s, "audit_log");
    let e = sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)])).unwrap_err();
    assert_eq!(e.code, ErrorCode::StorageCritical);
    assert_nothing_written(&s);
    assert_eq!(
        count(&s, "audit_log"),
        audits_before,
        "not even an audit row"
    );
    let shift = json!({ "transactionId": tid(), "shiftLocalId": "shift-9", "recordedAt": "2026-10-27T08:00:00Z",
                        "cashierId": "cashier-1" });
    assert_eq!(
        sales::shift_open(&mut s, shift).unwrap_err().code,
        ErrorCode::StorageCritical
    );
    assert_eq!(count(&s, "shifts"), 0);

    // With an old synced sale to clean, the sale goes through.
    s.set_storage_probe(ok_probe());
    let old = tid();
    sales::sale_commit(&mut s, sale(&old, &[("p1", 1.0)])).unwrap();
    mark(&mut s, &old, "accepted");
    set_synced_days_ago(&s, &old, 40);
    s.set_storage_probe(critical_probe());
    let r = sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)])).unwrap();
    assert_eq!(r.local_seq, 2);
    assert_eq!(
        count(&s, "sales"),
        1,
        "old synced sale cleaned, new one written"
    );
}

#[test]
fn storage_thresholds_and_check_cleanup() {
    let t = storage::Thresholds::default();
    let total = 100 * GB;
    assert_eq!(
        storage::classify(50 * GB, total, &t),
        storage::StorageState::Ok
    );
    assert_eq!(
        storage::classify(14 * GB, total, &t),
        storage::StorageState::Warning
    );
    assert_eq!(
        storage::classify(4 * GB, total, &t),
        storage::StorageState::Critical
    );
    // MB floors apply on small disks too.
    assert_eq!(
        storage::classify(400 * MB, 2 * GB, &t),
        storage::StorageState::Warning
    );
    assert_eq!(
        storage::classify(140 * MB, GB, &t),
        storage::StorageState::Critical
    );

    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    db::settings_set(s.conn(), "offlineSettings",
        &json!({ "storageWarningPct": 60, "storageWarningMb": 500, "storageCriticalPct": 5, "storageCriticalMb": 150 })).unwrap();
    let old = tid();
    sales::sale_commit(&mut s, sale(&old, &[("p1", 1.0)])).unwrap();
    mark(&mut s, &old, "accepted");
    set_synced_days_ago(&s, &old, 31);
    let c = storage::check(&mut s).unwrap();
    assert_eq!(
        c.storage.state,
        storage::StorageState::Warning,
        "50% free is below a 60% warning"
    );
    assert!(c.cleanup_ran);
    assert_eq!(c.cleanup_removed, 1);
    assert_eq!(audit_count(&s, "storage_warning"), 1);
    storage::check(&mut s).unwrap();
    assert_eq!(
        audit_count(&s, "storage_warning"),
        1,
        "state changes are audited once"
    );
}

// ---------------------------------------------------------------- shifts and queue

#[test]
fn shifts_queue_in_order_and_one_open_at_a_time() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let open_t = tid();
    let shift = json!({ "transactionId": open_t, "shiftLocalId": "shift-1", "recordedAt": "2026-10-27T08:00:00Z",
                        "cashierId": "cashier-1", "branchId": "branch-ikeja", "terminalId": "till-1", "openingFloat": 5000 });
    sales::shift_open(&mut s, shift.clone()).unwrap();
    assert_eq!(
        sales::shift_open(&mut s, shift).unwrap().status,
        "open",
        "idempotent"
    );
    let again = json!({ "transactionId": tid(), "shiftLocalId": "shift-2", "recordedAt": "2026-10-27T08:00:00Z", "cashierId": "c" });
    assert_eq!(
        sales::shift_open(&mut s, again).unwrap_err().code,
        ErrorCode::Invalid
    );
    assert_eq!(
        sales::shift_current(&s).unwrap().unwrap()["openingFloat"],
        5000
    );
    let s1 = tid();
    let s2 = tid();
    sales::sale_commit(&mut s, sale(&s1, &[("p1", 1.0)])).unwrap();
    sales::sale_commit(&mut s, sale(&s2, &[("p1", 1.0)])).unwrap();
    let close_t = tid();
    sales::shift_close(
        &mut s,
        json!({ "transactionId": close_t, "shiftLocalId": "shift-1",
        "recordedAt": "2026-10-27T17:00:00Z", "cashierId": "cashier-1", "countedCash": 12000 }),
    )
    .unwrap();
    assert!(sales::shift_current(&s).unwrap().is_none());
    // Selling into a closed shift is refused.
    assert_eq!(
        sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)]))
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );

    let batch = queue::next_batch(&s, Some(10)).unwrap();
    let kinds: Vec<&str> = batch.iter().map(|i| i.kind.as_str()).collect();
    assert_eq!(kinds, ["shift_open", "sale", "sale", "shift_close"]);
    assert_eq!(batch[1].transaction_id, s1);
    assert_eq!(batch[3].payload["countedCash"], 12000);
    assert_eq!(queue::next_batch(&s, Some(2)).unwrap().len(), 2);
}

#[test]
fn queue_mark_transitions() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let ids: Vec<String> = (0..4).map(|_| tid()).collect();
    for t in &ids {
        sales::sale_commit(&mut s, sale(t, &[("p1", 1.0)])).unwrap();
    }
    let res = queue::mark(
        &mut s,
        &[
            queue::MarkResult {
                transaction_id: ids[0].clone(),
                status: "accepted".into(),
                server_id: Some("srv-0".into()),
                receipt_number: Some("T01-000001".into()),
                reason: None,
                flags: Some(json!(["clock_drift"])),
            },
            queue::MarkResult {
                transaction_id: ids[1].clone(),
                status: "duplicate".into(),
                server_id: Some("srv-1".into()),
                receipt_number: None,
                reason: None,
                flags: None,
            },
            queue::MarkResult {
                transaction_id: ids[2].clone(),
                status: "failed_permanent".into(),
                server_id: None,
                receipt_number: None,
                reason: Some("date_too_old".into()),
                flags: None,
            },
            queue::MarkResult {
                transaction_id: ids[3].clone(),
                status: "failed_transient".into(),
                server_id: None,
                receipt_number: None,
                reason: Some("timeout".into()),
                flags: None,
            },
            queue::MarkResult {
                transaction_id: "no-such".into(),
                status: "accepted".into(),
                server_id: None,
                receipt_number: None,
                reason: None,
                flags: None,
            },
        ],
    )
    .unwrap();
    assert_eq!((res.synced, res.failed, res.retrying), (2, 1, 1));
    assert_eq!(res.unknown, vec!["no-such".to_string()]);
    let st = queue::status(&s).unwrap();
    assert_eq!(
        (st.pending_count, st.failed_count, st.synced_count),
        (1, 1, 2)
    );
    assert_eq!(st.last_sync_result.as_deref(), Some("partial"));
    assert!(st.last_sync_at.is_some());
    // Synced rows are kept, with the server's answer.
    let g = sales::sale_get(&s, &ids[0]).unwrap();
    assert_eq!(g["sync"]["status"], "synced");
    assert_eq!(g["sync"]["serverId"], "srv-0");
    assert_eq!(g["sync"]["flags"][0], "clock_drift");
    // The transient one waits; nothing else is eligible.
    assert!(queue::next_batch(&s, None).unwrap().is_empty());
    s.conn().execute("UPDATE outbox SET next_attempt_at = '2000-01-01T00:00:00.000Z' WHERE transaction_id = ?1", [&ids[3]]).unwrap();
    let b = queue::next_batch(&s, None).unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].attempts, 1);
    assert_eq!(b[0].last_error.as_deref(), Some("timeout"));
    // Permanent failure: in the failed list, never sent again until an admin retries.
    let failed = queue::failed_list(&s).unwrap();
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0].last_error.as_deref(), Some("date_too_old"));
    let r = queue::retry_failed(&mut s, &ids[2]).unwrap();
    assert_eq!((r.status.as_str(), r.attempts), ("pending", 0));
    assert_eq!(
        queue::retry_failed(&mut s, &ids[0]).unwrap_err().code,
        ErrorCode::NotFound,
        "synced cannot be retried"
    );
    // A late failure for an already synced item does not undo it.
    assert_eq!(mark(&mut s, &ids[0], "failed_permanent").ignored, 1);
    assert_eq!(
        sales::sale_get(&s, &ids[0]).unwrap()["sync"]["status"],
        "synced"
    );
    assert_eq!(
        queue::mark(
            &mut s,
            &[queue::MarkResult {
                transaction_id: ids[0].clone(),
                status: "bogus".into(),
                server_id: None,
                receipt_number: None,
                reason: None,
                flags: None
            }]
        )
        .unwrap_err()
        .code,
        ErrorCode::Invalid
    );
}

#[test]
fn backoff_is_exponential_jittered_and_capped() {
    assert_eq!(queue::backoff_delay_ms(1, 0.0), 2_000);
    assert_eq!(queue::backoff_delay_ms(2, 0.0), 4_000);
    assert_eq!(queue::backoff_delay_ms(9, 0.0), 512_000);
    assert_eq!(queue::backoff_delay_ms(10, 0.0), 600_000);
    assert_eq!(queue::backoff_delay_ms(1_000_000, 0.0), 600_000);
    for n in 1..=64u32 {
        let step = (2_000u64.saturating_mul(1u64 << (n - 1).min(30))).min(600_000);
        for j in [0.0, 0.25, 0.5, 0.999_999, 1.0, f64::NAN] {
            let d = queue::backoff_delay_ms(n, j);
            assert!(d <= 600_000, "cap n={n}");
            assert!(
                d as f64 >= step as f64 * 0.8 - 1.0 && d <= step,
                "n={n} j={j} d={d} step={step}"
            );
        }
    }
    // Through mark: the first retry is due 1.6 to 2 s from now.
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let t = tid();
    sales::sale_commit(&mut s, sale(&t, &[("p1", 1.0)])).unwrap();
    let before = Utc::now();
    mark(&mut s, &t, "failed_transient");
    let next: String = s
        .conn()
        .query_row(
            "SELECT next_attempt_at FROM outbox WHERE transaction_id = ?1",
            [&t],
            |r| r.get(0),
        )
        .unwrap();
    let next = chrono::DateTime::parse_from_rfc3339(&next)
        .unwrap()
        .with_timezone(&Utc);
    let wait = (next - before).num_milliseconds();
    assert!((1_550..=2_100).contains(&wait), "wait {wait} ms");
}

// ---------------------------------------------------------------- cleanup

#[test]
fn cleanup_removes_only_synced_older_than_retention() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    sales::shift_open(
        &mut s,
        json!({ "transactionId": tid(), "shiftLocalId": "shift-1",
        "recordedAt": "2026-10-27T08:00:00Z", "cashierId": "cashier-1" }),
    )
    .unwrap();
    let (t40, t10, t1, tp, tf, tt) = (tid(), tid(), tid(), tid(), tid(), tid());
    for t in [&t40, &t10, &t1, &tp, &tf, &tt] {
        sales::sale_commit(&mut s, sale(t, &[("p1", 1.0)])).unwrap();
    }
    for t in [&t40, &t10, &t1] {
        mark(&mut s, t, "accepted");
    }
    mark(&mut s, &tf, "failed_permanent");
    mark(&mut s, &tt, "failed_transient");
    set_synced_days_ago(&s, &t40, 40);
    set_synced_days_ago(&s, &t10, 10);
    set_synced_days_ago(&s, &t1, 1);
    // Pending, failed and waiting items that are very old are still never cleaned.
    let ancient = ts(Utc::now() - Duration::days(400));
    s.conn()
        .execute(
            "UPDATE outbox SET created_at = ?1 WHERE transaction_id IN (?2, ?3, ?4)",
            rusqlite::params![ancient, tp, tf, tt],
        )
        .unwrap();
    s.conn()
        .execute(
            "UPDATE sales SET created_at = ?1, recorded_at = ?1",
            [&ancient],
        )
        .unwrap();

    let r = cleanup::run(&mut s, None, false).unwrap();
    assert_eq!((r.sales, r.retention_days), (1, 30));
    assert!(sales::sale_get(&s, &t40).is_err());
    assert!(sales::sale_get(&s, &t10).is_ok());
    // Retention 30 -> 7 (the business setting, then an explicit argument).
    db::settings_set(s.conn(), "offlineSettings", &json!({ "retentionDays": 7 })).unwrap();
    let r = cleanup::run(&mut s, None, false).unwrap();
    assert_eq!((r.sales, r.retention_days), (1, 7));
    assert!(sales::sale_get(&s, &t10).is_err());
    let r = cleanup::run(&mut s, Some(7), true).unwrap();
    assert_eq!(r.sales, 0);
    assert!(r.vacuumed);
    for t in [&t1, &tp, &tf, &tt] {
        assert!(sales::sale_get(&s, t).is_ok(), "{t} must survive");
    }
    assert_eq!(
        count(&s, "sale_lines"),
        4,
        "lines of removed sales are gone with them"
    );
    assert_eq!(count(&s, "shifts"), 1, "the open shift is never cleaned");
    assert_eq!(
        cleanup::run(&mut s, Some(0), false).unwrap_err().code,
        ErrorCode::Invalid
    );
    let st = queue::status(&s).unwrap();
    assert_eq!(
        (st.pending_count, st.failed_count, st.synced_count),
        (3, 1, 1),
        "shift_open + pending + waiting"
    );
}

// ---------------------------------------------------------------- catalogue and stock

#[test]
fn stock_is_server_copy_plus_unsynced_local_deltas() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    catalog::apply_snapshot(
        &mut s,
        &snapshot(&[("p1", "Coke 50cl", Some("6001"), 10.0)]),
    )
    .unwrap();
    let on_hand = |s: &Store| catalog::displayed_on_hand(s.conn(), "p1").unwrap();
    assert_eq!(on_hand(&s), 10.0);

    let a = tid();
    sales::sale_commit(&mut s, sale(&a, &[("p1", 3.0)])).unwrap();
    assert_eq!(on_hand(&s), 7.0);
    let hit = catalog::by_barcode(&s, "6001").unwrap().unwrap();
    assert_eq!(hit["stock"]["onHand"], 7.0);
    assert_eq!(hit["stock"]["serverOnHand"], 10.0);
    assert_eq!(hit["stock"]["localDelta"], -3.0);

    // Accepted, but the server's stock row has not come down yet: still 7.
    mark(&mut s, &a, "accepted");
    assert_eq!(on_hand(&s), 7.0);
    // An old server row (from before the sale) delivered in a delta page would
    // double count if deltas were dropped on acceptance; they are dropped only
    // when a stock row arrives, which by then includes the sale.
    catalog::apply_changes(
        &mut s,
        &[
            json!({ "id": 101, "entity": "stock", "entityId": "stk-p1", "op": "upsert",
        "data": { "id": "stk-p1", "productId": "p1", "branchId": "branch-ikeja", "onHand": 7 } }),
        ],
        &json!(101),
    )
    .unwrap();
    assert_eq!(on_hand(&s), 7.0, "server 7 now includes the sale");

    // A pending sale keeps counting through a server overwrite.
    let b = tid();
    sales::sale_commit(&mut s, sale(&b, &[("p1", 2.0)])).unwrap();
    assert_eq!(on_hand(&s), 5.0);
    catalog::apply_changes(
        &mut s,
        &[
            json!({ "id": 102, "entity": "stock", "entityId": "stk-p1", "op": "upsert",
        "data": { "id": "stk-p1", "productId": "p1", "branchId": "branch-ikeja", "onHand": 6 } }),
        ],
        &json!(102),
    )
    .unwrap();
    assert_eq!(
        on_hand(&s),
        4.0,
        "another till sold one: server 6 + pending -2"
    );
    // A full snapshot does the same.
    catalog::apply_snapshot(&mut s, &snapshot(&[("p1", "Coke 50cl", Some("6001"), 6.0)])).unwrap();
    assert_eq!(on_hand(&s), 4.0);
    mark(&mut s, &b, "accepted");
    catalog::apply_snapshot(&mut s, &snapshot(&[("p1", "Coke 50cl", Some("6001"), 4.0)])).unwrap();
    assert_eq!(on_hand(&s), 4.0);
    // Offline sales may take stock negative (D7); the display follows.
    sales::sale_commit(&mut s, sale(&tid(), &[("p1", 9.0)])).unwrap();
    assert_eq!(on_hand(&s), -5.0);
    // Stock for another branch is ignored.
    catalog::apply_changes(
        &mut s,
        &[json!({ "entity": "stock", "entityId": "x", "op": "upsert",
        "data": { "id": "x", "productId": "p1", "branchId": "branch-lekki", "onHand": 999 } })],
        &json!(103),
    )
    .unwrap();
    assert_eq!(on_hand(&s), -5.0);
    assert_eq!(catalog::get_cursor(&s).unwrap(), json!(103));
}

#[test]
fn snapshot_replaces_in_one_transaction_and_changes_apply() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let r = catalog::apply_snapshot(
        &mut s,
        &snapshot(&[
            ("p1", "Coke 50cl", Some("6001"), 5.0),
            ("p2", "Fanta Orange", None, 1.0),
        ]),
    )
    .unwrap();
    assert_eq!(
        (r.products, r.stock, r.customers, r.staff, r.categories),
        (2, 2, 1, 1, 1)
    );
    assert_eq!(catalog::get_cursor(&s).unwrap(), json!(100));
    assert_eq!(
        db::settings_get(s.conn(), "settings").unwrap().unwrap()["vatRate"],
        7.5
    );
    // A broken snapshot changes nothing.
    let mut broken = snapshot(&[("p9", "Other", None, 1.0)]);
    broken.as_object_mut().unwrap().remove("staff");
    assert_eq!(
        catalog::apply_snapshot(&mut s, &broken).unwrap_err().code,
        ErrorCode::Invalid
    );
    let mut broken2 = snapshot(&[("p9", "Other", None, 1.0)]);
    broken2["customers"] = json!([{ "name": "no id" }]);
    assert!(catalog::apply_snapshot(&mut s, &broken2).is_err());
    assert_eq!(count(&s, "products"), 2);
    assert!(catalog::get_products(&s, &["p9".into()])
        .unwrap()
        .is_empty());
    // Staff: no hash leaves the store.
    let list = staff::staff_list(&s).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].has_pin);
    let json = serde_json::to_string(&list).unwrap();
    assert!(!json.contains("$2"), "no bcrypt hash in staff_list");
    let blob: String = s
        .conn()
        .query_row("SELECT data_json FROM staff", [], |r| r.get(0))
        .unwrap();
    assert!(!blob.contains("pinHash"));

    // Deltas: price, product rename, delete, user removal, settings, unknown entity.
    let res = catalog::apply_changes(&mut s, &[
        json!({ "entity": "price", "entityId": "p1", "op": "upsert", "data": { "id": "p1", "sellingPrice": 350 } }),
        json!({ "entity": "product", "entityId": "p2", "op": "upsert", "data": { "id": "p2", "name": "Fanta Lemon", "sku": "P2", "isActive": true } }),
        json!({ "entity": "user", "entityId": "m-1", "op": "delete", "data": null }),
        json!({ "entity": "offline_settings", "entityId": "o", "op": "upsert", "data": { "retentionDays": 14 } }),
        json!({ "entity": "future_thing", "entityId": "z", "op": "upsert", "data": {} }),
    ], &json!(200)).unwrap();
    assert_eq!((res.applied, res.skipped), (4, 1));
    assert_eq!(
        catalog::get_products(&s, &["p1".into()]).unwrap()[0]["sellingPrice"],
        350.0
    );
    assert_eq!(catalog::search(&s, "lemon", None, None).unwrap().len(), 1);
    assert!(catalog::search(&s, "orange", None, None)
        .unwrap()
        .is_empty());
    assert!(
        staff::staff_list(&s).unwrap().is_empty(),
        "withdrawn at the next sync (D5)"
    );
    assert_eq!(db::retention_days(s.conn()).unwrap(), 14);
    catalog::apply_changes(
        &mut s,
        &[json!({ "entity": "product", "entityId": "p2", "op": "delete", "data": null })],
        &json!(201),
    )
    .unwrap();
    assert!(catalog::get_products(&s, &["p2".into()])
        .unwrap()
        .is_empty());
    assert!(count(&s, "product_terms") > 0);
    let orphan: i64 = s
        .conn()
        .query_row(
            "SELECT count(*) FROM product_terms WHERE product_id = 'p2'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan, 0);
}

#[test]
fn search_lookups_are_prefix_case_insensitive() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let mut snap = snapshot(&[
        ("ck50", "Coke 50cl", Some("5449000000996"), 5.0),
        ("fan", "Fanta Orange 35cl", Some("5449000011527"), 1.0),
        ("rice", "Mama Gold Rice 5kg", None, 3.0),
        ("old", "Coke Zero (old)", None, 0.0),
    ]);
    snap["products"][3]["isActive"] = json!(false);
    catalog::apply_snapshot(&mut s, &snap).unwrap();
    let names = |v: Vec<Value>| {
        v.iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(catalog::search(&s, "co", None, None).unwrap()),
        ["Coke 50cl"]
    );
    assert_eq!(
        names(catalog::search(&s, "COKE", None, None).unwrap()),
        ["Coke 50cl"]
    );
    assert_eq!(
        names(catalog::search(&s, "gold", None, None).unwrap()),
        ["Mama Gold Rice 5kg"],
        "word prefix"
    );
    assert_eq!(
        names(catalog::search(&s, "mama 5kg", None, None).unwrap()),
        ["Mama Gold Rice 5kg"]
    );
    assert_eq!(
        names(catalog::search(&s, "ck5", None, None).unwrap()),
        ["Coke 50cl"],
        "sku prefix"
    );
    assert_eq!(
        names(catalog::search(&s, "54490000", None, None).unwrap()).len(),
        2,
        "barcode prefix"
    );
    assert!(catalog::search(&s, "co", Some("cat-other"), None)
        .unwrap()
        .is_empty());
    assert_eq!(
        catalog::search(&s, "", Some("cat-drinks"), Some(2))
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        catalog::by_barcode(&s, "5449000011527").unwrap().unwrap()["name"],
        "Fanta Orange 35cl"
    );
    assert_eq!(
        catalog::by_barcode(&s, "ck50").unwrap().unwrap()["name"],
        "Coke 50cl",
        "SKU fallback"
    );
    assert!(catalog::by_barcode(&s, "0000").unwrap().is_none());
    assert_eq!(
        catalog::search(&s, "100%_", None, None).unwrap().len(),
        0,
        "LIKE metacharacters are literal"
    );
    assert_eq!(catalog::categories_list(&s).unwrap()[0]["name"], "Drinks");
    assert_eq!(catalog::customers_search(&s, "ada", None).unwrap().len(), 1);
    assert_eq!(
        catalog::customers_search(&s, "okaf", None).unwrap().len(),
        1,
        "surname prefix"
    );
    assert_eq!(
        catalog::customers_search(&s, "0803", None).unwrap().len(),
        1,
        "phone prefix"
    );
    assert_eq!(
        catalog::customers_search(&s, "l-00012", None)
            .unwrap()
            .len(),
        1,
        "loyalty number"
    );
    assert_eq!(
        catalog::customer_get(&s, "cust-1").unwrap().unwrap()["loyaltyPoints"],
        120
    );
    assert!(catalog::customer_get(&s, "nobody").unwrap().is_none());
}

// ---------------------------------------------------------------- PINs

#[test]
fn pin_verify_and_lockout() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    catalog::apply_snapshot(&mut s, &snapshot(&[("p1", "Coke", None, 1.0)])).unwrap();
    let ok = staff::pin_verify(&mut s, "cashier-1", "1234").unwrap();
    assert!(ok.ok);
    assert_eq!(ok.permissions, json!(["pos.view", "pos.create"]));
    let w1 = staff::pin_verify(&mut s, "cashier-1", "0000").unwrap();
    assert_eq!(
        (w1.ok, w1.reason.as_deref(), w1.attempts_left),
        (false, Some("wrong_pin"), Some(2))
    );
    // A success in between resets the counter.
    assert!(staff::pin_verify(&mut s, "cashier-1", "1234").unwrap().ok);
    for left in [2, 1] {
        assert_eq!(
            staff::pin_verify(&mut s, "cashier-1", "9999")
                .unwrap()
                .attempts_left,
            Some(left)
        );
    }
    let third = staff::pin_verify(&mut s, "cashier-1", "9999").unwrap();
    assert_eq!(third.reason.as_deref(), Some("locked"));
    assert!(third.locked_until.is_some());
    // Locked: even the right PIN is refused, and it survives a restart.
    drop(s);
    let mut s = reopen(d.path());
    let r = staff::pin_verify(&mut s, "cashier-1", "1234").unwrap();
    assert_eq!((r.ok, r.reason.as_deref()), (false, Some("locked")));
    assert_eq!(audit_count(&s, "pin_lockout"), 1);
    // 15 minutes later.
    let until: String = s
        .conn()
        .query_row("SELECT locked_until FROM pin_attempts", [], |r| r.get(0))
        .unwrap();
    let until = chrono::DateTime::parse_from_rfc3339(&until)
        .unwrap()
        .with_timezone(&Utc);
    let mins = (until - Utc::now()).num_seconds();
    assert!((14 * 60..=15 * 60).contains(&mins), "{mins}s");
    s.conn()
        .execute(
            "UPDATE pin_attempts SET locked_until = '2000-01-01T00:00:00.000Z'",
            [],
        )
        .unwrap();
    assert!(staff::pin_verify(&mut s, "cashier-1", "1234").unwrap().ok);
    assert_eq!(count(&s, "pin_attempts"), 0);
    assert_eq!(
        staff::pin_verify(&mut s, "ghost", "1234")
            .unwrap()
            .reason
            .as_deref(),
        Some("unknown_user")
    );
    // Scope: a member limited to other terminals is refused on this one.
    s.conn()
        .execute(
            "UPDATE staff SET allowed_terminal_ids_json = '[\"till-9\"]'",
            [],
        )
        .unwrap();
    assert_eq!(
        staff::pin_verify(&mut s, "cashier-1", "1234")
            .unwrap()
            .reason
            .as_deref(),
        Some("terminal_not_allowed")
    );
    s.conn()
        .execute(
            "UPDATE staff SET allowed_terminal_ids_json = NULL, active = 0",
            [],
        )
        .unwrap();
    assert_eq!(
        staff::pin_verify(&mut s, "cashier-1", "1234")
            .unwrap()
            .reason
            .as_deref(),
        Some("inactive")
    );
}

// ---------------------------------------------------------------- audit, export, settings

#[test]
fn audit_append_take_and_mark() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let a = audit::append(
        &s,
        "offline_sign_in",
        &json!({ "method": "pin" }),
        Some("cashier-1"),
    )
    .unwrap();
    let b = audit::append(&s, "receipt.reprint", &Value::Null, None).unwrap();
    assert_eq!(
        audit::append(&s, "Bad Kind!", &json!({}), None)
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    let un = audit::take_unsent(&s, None).unwrap();
    assert_eq!(un.iter().map(|e| e.id).collect::<Vec<_>>(), vec![a, b]);
    assert_eq!(un[0].acting_user_id.as_deref(), Some("cashier-1"));
    assert_eq!(audit::mark_sent(&mut s, &[a]).unwrap(), 1);
    assert_eq!(audit::mark_sent(&mut s, &[a]).unwrap(), 0);
    assert_eq!(audit::take_unsent(&s, Some(10)).unwrap().len(), 1);
}

#[test]
fn export_pending_writes_unsent_items_only() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let (a, b, c) = (tid(), tid(), tid());
    for t in [&a, &b, &c] {
        sales::sale_commit(&mut s, sale(t, &[("p1", 1.0)])).unwrap();
    }
    mark(&mut s, &a, "accepted");
    mark(&mut s, &b, "failed_permanent");
    let out = d.path().join("export").join("till-export.json");
    let r = queue::export_pending(&mut s, out.to_str().unwrap()).unwrap();
    assert_eq!(r.items, 2);
    let doc: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(doc["format"], "digitglance-till-export");
    assert_eq!(doc["deviceCode"], "T01");
    let ids: Vec<&str> = doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["transactionId"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [b.as_str(), c.as_str()]);
    assert!(doc["warning"].as_str().unwrap().contains("not encrypted"));
    assert_eq!(
        queue::export_pending(&mut s, "relative.json")
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
    assert_eq!(
        queue::export_pending(&mut s, d.path().join("x.txt").to_str().unwrap())
            .unwrap_err()
            .code,
        ErrorCode::Invalid
    );
}

#[test]
fn settings_round_trip_and_status() {
    let d = tempfile::tempdir().unwrap();
    let s = open_test_store(d.path());
    db::settings_set(s.conn(), "receiptFooter", &json!("Thank you")).unwrap();
    assert_eq!(
        db::settings_get(s.conn(), "receiptFooter").unwrap(),
        Some(json!("Thank you"))
    );
    assert!(db::settings_get(s.conn(), "nothing").unwrap().is_none());
    assert!(db::settings_set(s.conn(), "bad key", &json!(1)).is_err());
    let st = queue::status(&s).unwrap();
    assert_eq!(st.retention_days, 30);
    assert_eq!(st.storage.state, storage::StorageState::Ok);
    assert_eq!(st.storage.free_pct, 50.0);
    assert!(st.db_size_mb > 0.0);
    assert!(st.oldest_pending_at.is_none());
}

#[test]
fn storage_probe_reads_this_volume() {
    let d = tempfile::tempdir().unwrap();
    let (free, total) = storage::os_probe(&d.path().join("x.db")).unwrap();
    assert!(total > 0 && free <= total);
}

// ---------------------------------------------------------------- returns

/// A return of `original`: (productId, originalLineNo, qty, restock) per line.
fn ret(tid: &str, original: &str, lines: &[(&str, i64, f64, bool)]) -> Value {
    let mut v = sale(
        tid,
        &lines
            .iter()
            .map(|(p, _, q, _)| (*p, *q))
            .collect::<Vec<_>>(),
    );
    v["kind"] = json!("return");
    v["originalTransactionId"] = json!(original);
    v["reason"] = json!("customer_changed_mind");
    for (i, (_, n, _, restock)) in lines.iter().enumerate() {
        v["lines"][i]["originalLineNo"] = json!(n);
        v["lines"][i]["restock"] = json!(restock);
    }
    v
}

fn err_msg(r: super::error::Result<sales::CommitResult>) -> (ErrorCode, String) {
    let e = r.unwrap_err();
    (e.code, e.message)
}

#[test]
fn return_commit_numbers_restocks_and_queues_after_its_sale() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    catalog::apply_snapshot(
        &mut s,
        &snapshot(&[
            ("p1", "Coke 50cl", None, 10.0),
            ("p2", "Fanta 50cl", None, 5.0),
        ]),
    )
    .unwrap();
    let on_hand = |s: &Store, p: &str| catalog::displayed_on_hand(s.conn(), p).unwrap();
    let a = tid();
    sales::sale_commit(&mut s, sale(&a, &[("p1", 3.0), ("p2", 2.0)])).unwrap();
    assert_eq!((on_hand(&s, "p1"), on_hand(&s, "p2")), (7.0, 3.0));

    // Two back on the shelf, one damaged (not restocked).
    let r = tid();
    let body = ret(&r, &a, &[("p1", 1, 2.0, true), ("p2", 2, 1.0, false)]);
    let res = sales::sale_commit(&mut s, body.clone()).unwrap();
    assert_eq!(
        res,
        sales::CommitResult {
            receipt_number: "T01-000002".into(),
            local_seq: 2
        },
        "returns share the receipt sequence"
    );
    assert_eq!((on_hand(&s, "p1"), on_hand(&s, "p2")), (9.0, 3.0));
    let deltas: Vec<(String, f64)> = {
        let mut st = s
            .conn()
            .prepare(
                "SELECT product_id, qty_delta FROM local_stock_delta WHERE transaction_id = ?1",
            )
            .unwrap();
        st.query_map([&r], |x| Ok((x.get(0)?, x.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(deltas, vec![("p1".to_string(), 2.0)]);
    let (kind, orig): (String, Option<String>) = s
        .conn()
        .query_row(
            "SELECT kind, original_transaction_id FROM sales WHERE transaction_id = ?1",
            [&r],
            |x| Ok((x.get(0)?, x.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        (kind.as_str(), orig.as_deref()),
        ("return", Some(a.as_str()))
    );
    let lines: Vec<(i64, i64)> = {
        let mut st = s
            .conn()
            .prepare(
                "SELECT original_line_no, restock FROM sale_lines WHERE transaction_id = ?1 ORDER BY line_no",
            )
            .unwrap();
        st.query_map([&r], |x| Ok((x.get(0)?, x.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    assert_eq!(lines, vec![(1, 1), (2, 0)]);
    assert_eq!(audit_count(&s, "return_recorded"), 1);
    assert_eq!(audit_count(&s, "sale_recorded"), 1);
    let detail: String = s
        .conn()
        .query_row(
            "SELECT detail_json FROM audit_log WHERE kind = 'return_recorded'",
            [],
            |x| x.get(0),
        )
        .unwrap();
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["originalTransactionId"], a.as_str());
    assert_eq!(detail["receiptNumber"], "T01-000002");
    assert_eq!(detail["offline"], true);

    // Idempotent: the same transactionId returns the first result, writes nothing.
    assert_eq!(sales::sale_commit(&mut s, body).unwrap(), res);
    assert_eq!(count(&s, "sales"), 2);
    assert_eq!(count(&s, "local_stock_delta"), 3);
    assert_eq!(audit_count(&s, "return_recorded"), 1);
    // The next sale continues the shared sequence.
    assert_eq!(
        sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)]))
            .unwrap()
            .receipt_number,
        "T01-000003"
    );

    // sale_get works for a return.
    let got = sales::sale_get(&s, &r).unwrap();
    assert_eq!(got["kind"], "return");
    assert_eq!(got["originalTransactionId"], a.as_str());
    assert_eq!(got["lines"][0]["originalLineNo"], 1);
    assert_eq!(got["sync"]["status"], "pending");

    // The queue hands out the return after its sale, payload intact.
    let batch = queue::next_batch(&s, Some(10)).unwrap();
    assert_eq!(batch[0].transaction_id, a);
    assert_eq!(batch[0].kind, "sale");
    assert_eq!(batch[1].transaction_id, r);
    assert_eq!(batch[1].kind, "return");
    assert!(batch[0].seq < batch[1].seq);
    let p = &batch[1].payload;
    assert_eq!(p["kind"], "return");
    assert_eq!(p["originalTransactionId"], a.as_str());
    assert_eq!(p["lines"][0]["originalLineNo"], 1);
    assert_eq!(p["lines"][0]["restock"], true);
    assert_eq!(p["lines"][1]["originalLineNo"], 2);
    assert_eq!(p["lines"][1]["restock"], false);
    assert_eq!(p["receiptNumber"], "T01-000002");
    assert_eq!(p["localSeq"], 2);
    assert_eq!(p["deviceCode"], "T01");
    assert_eq!(
        p["reason"], "customer_changed_mind",
        "unknown keys are kept"
    );

    // Accepting the return marks its stock deltas synced, as for a sale.
    assert_eq!(mark(&mut s, &r, "accepted").synced, 1);
    let unsynced: i64 = s
        .conn()
        .query_row(
            "SELECT count(*) FROM local_stock_delta WHERE transaction_id = ?1 AND synced_at IS NULL",
            [&r],
            |x| x.get(0),
        )
        .unwrap();
    assert_eq!(unsynced, 0);
    assert_eq!(
        on_hand(&s, "p1"),
        8.0,
        "synced deltas still count until reconciled"
    );

    // Validation.
    let mut bad = ret(&tid(), &a, &[("p1", 1, 1.0, true)]);
    bad["kind"] = json!("refund");
    assert_eq!(
        err_msg(sales::sale_commit(&mut s, bad)),
        (
            ErrorCode::Invalid,
            "kind must be 'sale' or 'return'.".into()
        )
    );
    let mut bad = ret(&tid(), &a, &[("p1", 1, 1.0, true)]);
    bad.as_object_mut().unwrap().remove("originalTransactionId");
    assert_eq!(
        err_msg(sales::sale_commit(&mut s, bad)).0,
        ErrorCode::Invalid
    );
    let mut bad = ret(&tid(), &a, &[("p1", 1, 1.0, true)]);
    bad["lines"][0]
        .as_object_mut()
        .unwrap()
        .remove("originalLineNo");
    assert_eq!(
        err_msg(sales::sale_commit(&mut s, bad)).0,
        ErrorCode::Invalid
    );
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p1", 0, 1.0, true)])
        ))
        .0,
        ErrorCode::Invalid
    );
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p1", 9, 1.0, true)])
        )),
        (
            ErrorCode::Invalid,
            "Line 1: that line is not on the original sale.".into()
        )
    );
    // restock may be left out (false: no stock delta).
    let mut no_restock = ret(&tid(), &a, &[("p2", 2, 1.0, false)]);
    no_restock["lines"][0]
        .as_object_mut()
        .unwrap()
        .remove("restock");
    let before = count(&s, "local_stock_delta");
    sales::sale_commit(&mut s, no_restock).unwrap();
    assert_eq!(count(&s, "local_stock_delta"), before);
}

#[test]
fn return_cannot_refund_more_than_sold_or_an_unknown_or_refused_sale() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let a = tid();
    sales::sale_commit(&mut s, sale(&a, &[("p1", 3.0), ("p2", 1.5)])).unwrap();
    let nothing_more = |s: &Store, n_sales: i64, seq: i64| {
        assert_eq!(count(s, "sales"), n_sales);
        assert_eq!(receipt_seq(s), seq, "a refused return uses no number");
    };
    let only = |line: usize, left: &str, sold: &str| {
        (
            ErrorCode::Invalid,
            format!("Line {line}: only {left} of {sold} can still be returned."),
        )
    };
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p1", 1, 4.0, true)])
        )),
        only(1, "3", "3")
    );
    // Two lines naming the same original line count together.
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p1", 1, 2.0, true), ("p1", 1, 2.0, false)])
        )),
        only(2, "3", "3")
    );
    nothing_more(&s, 1, 1);
    // A partial return, then a second one beyond what is left.
    sales::sale_commit(&mut s, ret(&tid(), &a, &[("p1", 1, 2.0, true)])).unwrap();
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p1", 1, 2.0, true)])
        )),
        only(1, "1", "3")
    );
    sales::sale_commit(&mut s, ret(&tid(), &a, &[("p1", 1, 1.0, true)])).unwrap();
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p1", 1, 0.5, true)])
        )),
        only(1, "0", "3")
    );
    // Fractional quantities, within the 1e-9 tolerance (0.1 + 0.2 + 1.2 = 1.5).
    sales::sale_commit(&mut s, ret(&tid(), &a, &[("p2", 2, 0.1, false)])).unwrap();
    sales::sale_commit(&mut s, ret(&tid(), &a, &[("p2", 2, 0.2, false)])).unwrap();
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &a, &[("p2", 2, 1.3, false)])
        )),
        only(1, "1.2", "1.5")
    );
    sales::sale_commit(&mut s, ret(&tid(), &a, &[("p2", 2, 1.2, false)])).unwrap();
    nothing_more(&s, 6, 6);

    // Unknown original, and a return is not an original.
    let unknown = (
        ErrorCode::NotFound,
        "The original sale is not on this till.".to_string(),
    );
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &tid(), &[("p1", 1, 1.0, true)])
        )),
        unknown
    );
    let (b, r) = (tid(), tid());
    sales::sale_commit(&mut s, sale(&b, &[("p1", 1.0)])).unwrap();
    sales::sale_commit(&mut s, ret(&r, &b, &[("p1", 1, 1.0, true)])).unwrap();
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &r, &[("p1", 1, 1.0, true)])
        )),
        unknown
    );
    // The server refused the original sale.
    let c = tid();
    sales::sale_commit(&mut s, sale(&c, &[("p1", 1.0)])).unwrap();
    mark(&mut s, &c, "failed_permanent");
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &c, &[("p1", 1, 1.0, true)])
        )),
        (
            ErrorCode::Invalid,
            "The server refused the original sale, so it cannot be returned on this till.".into()
        )
    );
    nothing_more(&s, 9, 9);
}

#[test]
fn return_commit_is_atomic_when_it_panics_between_statements() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let a = tid();
    sales::sale_commit(&mut s, sale(&a, &[("p1", 2.0), ("p2", 1.0)])).unwrap();
    for fp in [
        "sale_after_seq",
        "sale_after_header",
        "sale_after_lines",
        "sale_after_outbox",
        "sale_before_commit",
    ] {
        failpoint::set_panic(Some(fp));
        let r = catch_unwind(AssertUnwindSafe(|| {
            sales::sale_commit(
                &mut s,
                ret(&tid(), &a, &[("p1", 1, 2.0, true), ("p2", 2, 1.0, true)]),
            )
        }));
        failpoint::set_panic(None);
        assert!(r.is_err(), "failpoint {fp} should have fired");
        assert_eq!(count(&s, "sales"), 1);
        assert_eq!(count(&s, "sale_lines"), 2);
        assert_eq!(count(&s, "sale_payments"), 1);
        assert_eq!(count(&s, "outbox"), 1);
        assert_eq!(count(&s, "local_stock_delta"), 2);
        assert_eq!(receipt_seq(&s), 1);
        assert_eq!(audit_count(&s, "return_recorded"), 0);
    }
    let r = sales::sale_commit(&mut s, ret(&tid(), &a, &[("p1", 1, 2.0, true)])).unwrap();
    assert_eq!(r.receipt_number, "T01-000002");
}

#[test]
fn sales_recent_filters_by_kind_original_and_receipt() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let (a, b, r1, r2, r3) = (tid(), tid(), tid(), tid(), tid());
    sales::sale_commit(&mut s, sale(&a, &[("p1", 3.0)])).unwrap(); // T01-000001
    sales::sale_commit(&mut s, sale(&b, &[("p1", 3.0)])).unwrap(); // 2
    sales::sale_commit(&mut s, ret(&r1, &a, &[("p1", 1, 1.0, true)])).unwrap(); // 3
    sales::sale_commit(&mut s, ret(&r2, &a, &[("p1", 1, 1.0, true)])).unwrap(); // 4
    sales::sale_commit(&mut s, ret(&r3, &b, &[("p1", 1, 1.0, true)])).unwrap(); // 5
    let ids = |k: Option<&str>, o: Option<&str>, rn: Option<&str>| -> Vec<String> {
        sales::sales_recent(
            &s,
            None,
            None,
            k.map(str::to_string),
            o.map(str::to_string),
            rn.map(str::to_string),
        )
        .unwrap()
        .into_iter()
        .map(|x| x.transaction_id)
        .collect()
    };
    assert_eq!(
        ids(None, None, None),
        vec![r3.clone(), r2.clone(), r1.clone(), b.clone(), a.clone()]
    );
    assert_eq!(
        ids(Some("return"), None, None),
        vec![r3.clone(), r2.clone(), r1.clone()]
    );
    assert_eq!(ids(Some("sale"), None, None), vec![b.clone(), a.clone()]);
    assert_eq!(ids(None, Some(&a), None), vec![r2.clone(), r1.clone()]);
    assert_eq!(ids(Some("return"), Some(&b), None), vec![r3.clone()]);
    assert!(ids(Some("sale"), Some(&a), None).is_empty());
    assert_eq!(ids(None, None, Some("t01-000001")), vec![a.clone()]);
    assert_eq!(ids(None, None, Some("T01-000004")), vec![r2.clone()]);
    assert!(ids(Some("sale"), None, Some("T01-000004")).is_empty());
    assert!(ids(None, None, Some("T01-999999")).is_empty());
    let e = sales::sales_recent(&s, None, None, Some("refund".into()), None, None).unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    // Limit and offset still apply after the filters.
    let page =
        sales::sales_recent(&s, Some(1), Some(1), Some("return".into()), None, None).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].transaction_id, r2);

    // Summary fields, camelCase on the wire.
    let all = sales::sales_recent(&s, Some(10), None, None, None, None).unwrap();
    assert_eq!(all[0].kind, "return");
    assert_eq!(all[0].original_transaction_id.as_deref(), Some(b.as_str()));
    assert_eq!(all[4].kind, "sale");
    assert_eq!(all[4].original_transaction_id, None);
    let wire = serde_json::to_value(&all[0]).unwrap();
    assert_eq!(wire["kind"], "return");
    assert_eq!(wire["originalTransactionId"], b.as_str());
    assert_eq!(wire["receiptNumber"], "T01-000005");
}

#[test]
fn migration_v1_to_v2_keeps_queue_rows_seq_and_payloads() {
    let d = tempfile::tempdir().unwrap();
    let path = db::db_path(d.path(), OWNER, DEVICE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let a = tid();
    let (synced, failed, gone) = (tid(), tid(), tid());
    let a_payload = json!({ "transactionId": a, "kind": "sale", "receiptNumber": "T01-000001",
                            "localSeq": 1, "deviceCode": "T01", "lines": [{ "qty": 2.0 }],
                            "somethingNew": { "kept": true } });
    {
        // A version-1 database, opened the way the v1 app opened it.
        let mut c = Connection::open(&path).unwrap();
        let hex: String = KEY.iter().map(|b| format!("{b:02x}")).collect();
        c.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";"))
            .unwrap();
        c.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")
            .unwrap();
        let _: String = c
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            migrations::migrate_with(&mut c, &migrations::MIGRATIONS[..1]).unwrap(),
            1
        );
        let now = db::now_str();
        c.execute_batch(&format!(
            "INSERT INTO meta(key, value) VALUES ('owner_id', '{OWNER}'), ('device_id', '{DEVICE}');
             INSERT INTO device_identity VALUES (1, '{OWNER}', '{DEVICE}', 'T01', 'branch-ikeja', 'till-1',
                                                 'https://api.digitglance.com', '{now}');
             UPDATE sequences SET value = 3 WHERE name = 'receipt';
             INSERT INTO shifts(shift_local_id, status, cashier_id, opened_at, open_transaction_id, open_payload_json)
                 VALUES ('shift-1', 'open', 'cashier-1', '{now}', 'open-tx', '{{}}');"
        ))
        .unwrap();
        let ob =
            "INSERT INTO outbox(transaction_id, kind, status, payload_json, attempts, last_error,
                                     server_id, flags_json, created_at, updated_at, synced_at)
                  VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9, ?10)";
        let none = None::<String>;
        c.execute(
            ob,
            rusqlite::params![
                "open-tx",
                "shift_open",
                "pending",
                "{\"kind\":\"shift_open\"}",
                0,
                none,
                none,
                none,
                now,
                none
            ],
        )
        .unwrap();
        type V1Row<'a> = (
            &'a String,
            i64,
            &'a str,
            i64,
            Option<&'a str>,
            Option<&'a str>,
            Option<&'a str>,
            Option<String>,
        );
        let rows: [V1Row; 3] = [
            (&a, 1, "pending", 0, None, None, None, None),
            (
                &synced,
                2,
                "synced",
                0,
                None,
                Some("srv-1"),
                Some("[\"clock_drift\"]"),
                Some(now.clone()),
            ),
            (
                &failed,
                3,
                "failed",
                1,
                Some("date_too_old"),
                None,
                None,
                None,
            ),
        ];
        for (t, seq, status, attempts, err, srv, flags, at) in rows {
            c.execute(
                "INSERT INTO sales(transaction_id, local_seq, receipt_number, kind, recorded_at, cashier_id,
                                   shift_local_id, total, totals_json, created_at)
                 VALUES (?1, ?2, ?3, 'sale', ?4, 'cashier-1', 'shift-1', 645, '{}', ?4)",
                rusqlite::params![t, seq, format!("T01-{seq:06}"), now],
            )
            .unwrap();
            c.execute(
                "INSERT INTO sale_lines(transaction_id, line_no, product_id, product_snapshot_json, qty,
                                        unit_price, line_total) VALUES (?1, 1, 'p1', '{}', 2, 300, 645)",
                [t],
            )
            .unwrap();
            c.execute(
                "INSERT INTO local_stock_delta(transaction_id, product_id, qty_delta, created_at)
                 VALUES (?1, 'p1', -2, ?2)",
                rusqlite::params![t, now],
            )
            .unwrap();
            let payload = if t == &a {
                a_payload.to_string()
            } else {
                json!({ "transactionId": t, "kind": "sale" }).to_string()
            };
            c.execute(
                ob,
                rusqlite::params![t, "sale", status, payload, attempts, err, srv, flags, now, at],
            )
            .unwrap();
        }
        // A synced row cleanup already removed: its seq (5) must never come back.
        c.execute(
            ob,
            rusqlite::params![
                gone,
                "shift_close",
                "synced",
                "{}",
                0,
                none,
                none,
                none,
                now,
                now
            ],
        )
        .unwrap();
        c.execute("DELETE FROM outbox WHERE transaction_id = ?1", [&gone])
            .unwrap();
    }

    let mut s = reopen(d.path());
    assert_eq!(
        migrations::current_version(s.conn()).unwrap(),
        migrations::latest_version()
    );
    let ddl: String = s
        .conn()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'outbox'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(ddl.contains("'return'"), "{ddl}");
    for idx in ["outbox_status", "outbox_synced", "sales_original"] {
        let n: i64 = s
            .conn()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [idx],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "index {idx}");
    }
    let batch = queue::next_batch(&s, Some(10)).unwrap();
    assert_eq!(batch.len(), 2);
    assert_eq!((batch[0].seq, batch[0].kind.as_str()), (1, "shift_open"));
    assert_eq!(batch[1].seq, 2);
    assert_eq!(batch[1].transaction_id, a);
    assert_eq!(batch[1].payload, a_payload, "payload intact");
    type Kept = (String, i64, Option<String>, Option<String>, Option<String>);
    let row = |t: &str| -> Kept {
        s.conn()
            .query_row(
                "SELECT status, attempts, last_error, server_id, flags_json FROM outbox WHERE transaction_id = ?1",
                [t],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap()
    };
    assert_eq!(
        row(&synced),
        (
            "synced".into(),
            0,
            None,
            Some("srv-1".into()),
            Some("[\"clock_drift\"]".into())
        )
    );
    assert_eq!(
        row(&failed),
        ("failed".into(), 1, Some("date_too_old".into()), None, None)
    );
    assert_eq!(queue::failed_list(&s).unwrap().len(), 1);
    // Existing rows read with the new columns' defaults.
    let (orig, restock): (Option<String>, i64) = s
        .conn()
        .query_row(
            "SELECT s.original_transaction_id, l.restock FROM sales s JOIN sale_lines l USING (transaction_id)
             WHERE s.transaction_id = ?1",
            [&a],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((orig, restock), (None, 0));
    // New items continue above every seq ever used (5 was deleted).
    let res = sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0)])).unwrap();
    assert_eq!(res.receipt_number, "T01-000004");
    sales::sale_commit(&mut s, ret(&tid(), &a, &[("p1", 1, 1.0, true)])).unwrap();
    let seqs: Vec<i64> = queue::next_batch(&s, Some(10))
        .unwrap()
        .iter()
        .map(|i| i.seq)
        .collect();
    assert_eq!(seqs, vec![1, 2, 6, 7]);
    // The failed v1 sale cannot be returned; the synced one can.
    assert_eq!(
        err_msg(sales::sale_commit(
            &mut s,
            ret(&tid(), &failed, &[("p1", 1, 1.0, true)])
        ))
        .0,
        ErrorCode::Invalid
    );
    sales::sale_commit(&mut s, ret(&tid(), &synced, &[("p1", 1, 2.0, false)])).unwrap();
}

#[test]
fn loyalty_settings_are_stored_and_unknown_entities_skipped() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let mut snap = snapshot(&[("p1", "Coke 50cl", None, 10.0)]);
    snap["loyalty"] = json!({ "enabled": true, "pointsPerNaira": 0.01, "nairaPerPoint": 1 });
    catalog::apply_snapshot(&mut s, &snap).unwrap();
    assert_eq!(
        db::settings_get(s.conn(), "loyalty").unwrap(),
        Some(json!({ "enabled": true, "pointsPerNaira": 0.01, "nairaPerPoint": 1 }))
    );
    let r = catalog::apply_changes(
        &mut s,
        &[
            json!({ "id": 201, "entity": "loyalty_settings", "entityId": "loyalty", "op": "upsert",
                    "data": { "enabled": false, "pointsPerNaira": 0.02 } }),
            json!({ "id": 202, "entity": "gift_card", "entityId": "g1", "op": "upsert",
                    "data": { "id": "g1" } }),
            json!({ "id": 203, "entity": "gift_card", "entityId": "g1", "op": "delete" }),
        ],
        &json!(203),
    )
    .unwrap();
    assert_eq!((r.applied, r.skipped), (1, 2));
    assert_eq!(catalog::get_cursor(&s).unwrap(), json!(203));
    assert_eq!(
        db::settings_get(s.conn(), "loyalty").unwrap(),
        Some(json!({ "enabled": false, "pointsPerNaira": 0.02 }))
    );
    let r = catalog::apply_changes(
        &mut s,
        &[json!({ "id": 204, "entity": "loyalty_settings", "entityId": "loyalty", "op": "delete" })],
        &json!(204),
    )
    .unwrap();
    assert_eq!(r.applied, 1);
    assert_eq!(db::settings_get(s.conn(), "loyalty").unwrap(), None);
}

// ------------------------------------------------- item type and fractions

/// `(itemType, soldByFraction)` as the read commands return them.
fn kind_of(p: &Value) -> (Value, Value) {
    (p["itemType"].clone(), p["soldByFraction"].clone())
}

#[test]
fn migration_v2_to_v3_gives_existing_products_the_defaults() {
    let d = tempfile::tempdir().unwrap();
    let path = db::db_path(d.path(), OWNER, DEVICE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    {
        // A version-2 database with products, opened the way the v2 app opened it.
        let mut c = Connection::open(&path).unwrap();
        let hex: String = KEY.iter().map(|b| format!("{b:02x}")).collect();
        c.execute_batch(&format!("PRAGMA key = \"x'{hex}'\";"))
            .unwrap();
        c.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")
            .unwrap();
        let _: String = c
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            migrations::migrate_with(&mut c, &migrations::MIGRATIONS[..2]).unwrap(),
            2
        );
        let now = db::now_str();
        c.execute_batch(&format!(
            "INSERT INTO meta(key, value) VALUES ('owner_id', '{OWNER}'), ('device_id', '{DEVICE}');
             INSERT INTO device_identity VALUES (1, '{OWNER}', '{DEVICE}', 'T01', 'branch-ikeja', 'till-1',
                                                 'https://api.digitglance.com', '{now}');"
        ))
        .unwrap();
        let ins = "INSERT INTO products(id, name, name_lc, sku, sku_lc, is_active, data_json)
                   VALUES (?1, ?2, lower(?2), ?3, lower(?3), 1, ?4)";
        // An older server's product (no new fields), and two whose stored objects
        // already carry them (a newer feed applied by the v2 app).
        let rows = [
            json!({ "id": "p1", "name": "Coke 50cl", "sku": "CK50" }),
            json!({ "id": "p2", "name": "Rice per kg", "sku": "RICE", "itemType": "stock",
                    "soldByFraction": true }),
            json!({ "id": "p3", "name": "Delivery", "sku": "DLV", "itemType": "service",
                    "soldByFraction": false }),
        ];
        for p in &rows {
            c.execute(
                ins,
                rusqlite::params![
                    p["id"].as_str().unwrap(),
                    p["name"].as_str().unwrap(),
                    p["sku"].as_str().unwrap(),
                    p.to_string()
                ],
            )
            .unwrap();
        }
    }

    let mut s = reopen(d.path());
    assert_eq!(migrations::current_version(s.conn()).unwrap(), 3);
    let ps = catalog::get_products(&s, &["p1".into(), "p2".into(), "p3".into()]).unwrap();
    assert_eq!(ps.len(), 3);
    assert_eq!(kind_of(&ps[0]), (json!("stock"), json!(false)));
    assert_eq!(kind_of(&ps[1]), (json!("stock"), json!(true)));
    assert_eq!(kind_of(&ps[2]), (json!("service"), json!(false)));
    // The CHECK holds on the migrated table.
    assert!(s
        .conn()
        .execute(
            "UPDATE products SET item_type = 'bundle' WHERE id = 'p1'",
            []
        )
        .is_err());
    // A later feed row still updates a migrated product.
    catalog::apply_changes(
        &mut s,
        &[
            json!({ "entity": "product", "entityId": "p1", "op": "upsert",
                  "data": { "id": "p1", "name": "Coke 50cl", "sku": "CK50", "isActive": true,
                            "itemType": "service" } }),
        ],
        &json!(1),
    )
    .unwrap();
    let p1 = catalog::get_products(&s, &["p1".into()]).unwrap();
    assert_eq!(kind_of(&p1[0]), (json!("service"), json!(false)));
}

#[test]
fn item_type_and_fraction_are_stored_updated_and_returned() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let mut snap = snapshot(&[
        ("p1", "Coke 50cl", Some("6001"), 10.0),
        ("p2", "Rice per kg", Some("6002"), 50.0),
        ("p3", "Delivery fee", None, 0.0),
    ]);
    // p1 as an older server sends it (no fields); p2 fractional; p3 a service.
    snap["products"][1]["itemType"] = json!("stock");
    snap["products"][1]["soldByFraction"] = json!(true);
    snap["products"][2]["itemType"] = json!("service");
    snap["products"][2]["soldByFraction"] = json!(false);
    catalog::apply_snapshot(&mut s, &snap).unwrap();
    let stored: Vec<(String, String, i64)> = {
        let mut st = s
            .conn()
            .prepare("SELECT id, item_type, sold_by_fraction FROM products ORDER BY id")
            .unwrap();
        let rows = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.collect::<rusqlite::Result<_>>().unwrap()
    };
    assert_eq!(
        stored,
        vec![
            ("p1".into(), "stock".into(), 0),
            ("p2".into(), "stock".into(), 1),
            ("p3".into(), "service".into(), 0),
        ]
    );
    // Every read command returns both fields, camelCase.
    let ps = catalog::get_products(&s, &["p1".into(), "p2".into(), "p3".into()]).unwrap();
    assert_eq!(kind_of(&ps[0]), (json!("stock"), json!(false)));
    assert_eq!(kind_of(&ps[1]), (json!("stock"), json!(true)));
    assert_eq!(kind_of(&ps[2]), (json!("service"), json!(false)));
    let hit = catalog::by_barcode(&s, "6002").unwrap().unwrap();
    assert_eq!(kind_of(&hit), (json!("stock"), json!(true)));
    let found = catalog::search(&s, "delivery", None, None).unwrap();
    assert_eq!(kind_of(&found[0]), (json!("service"), json!(false)));
    let all = catalog::search(&s, "", None, None).unwrap();
    assert_eq!(all.len(), 3);
    for p in all {
        assert!(
            p["itemType"].is_string() && p["soldByFraction"].is_boolean(),
            "{p}"
        );
    }

    // Changes: p1 becomes fractional, p2 loses the flags (an older server's row
    // means the defaults), p3 sends an unknown kind and a non-boolean flag (both
    // fall back to the defaults); a price row keeps what the product has.
    let r = catalog::apply_changes(
        &mut s,
        &[
            json!({ "entity": "product", "entityId": "p1", "op": "upsert",
                    "data": { "id": "p1", "name": "Coke 50cl", "barcode": "6001", "isActive": true,
                              "itemType": "stock", "soldByFraction": true } }),
            json!({ "entity": "product", "entityId": "p2", "op": "upsert",
                    "data": { "id": "p2", "name": "Rice per kg", "barcode": "6002", "isActive": true } }),
            json!({ "entity": "product", "entityId": "p3", "op": "upsert",
                    "data": { "id": "p3", "name": "Delivery fee", "isActive": true,
                              "itemType": "bundle", "soldByFraction": "yes" } }),
            json!({ "entity": "price", "entityId": "p1", "op": "upsert",
                    "data": { "id": "p1", "sellingPrice": 320 } }),
        ],
        &json!(300),
    )
    .unwrap();
    assert_eq!((r.applied, r.skipped), (4, 0));
    let ps = catalog::get_products(&s, &["p1".into(), "p2".into(), "p3".into()]).unwrap();
    assert_eq!(kind_of(&ps[0]), (json!("stock"), json!(true)));
    assert_eq!(ps[0]["sellingPrice"], 320.0);
    assert_eq!(kind_of(&ps[1]), (json!("stock"), json!(false)));
    assert_eq!(kind_of(&ps[2]), (json!("stock"), json!(false)));
    catalog::apply_changes(
        &mut s,
        &[
            json!({ "entity": "product", "entityId": "p3", "op": "upsert",
                  "data": { "id": "p3", "name": "Delivery fee", "isActive": true,
                            "itemType": "service" } }),
        ],
        &json!(301),
    )
    .unwrap();
    let p3 = catalog::get_products(&s, &["p3".into()]).unwrap();
    assert_eq!(kind_of(&p3[0]), (json!("service"), json!(false)));
}

#[test]
fn cleanup_removes_old_synced_returns_and_keeps_pending_ones() {
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let (a, r_old, r_pending) = (tid(), tid(), tid());
    sales::sale_commit(&mut s, sale(&a, &[("p1", 3.0)])).unwrap();
    sales::sale_commit(&mut s, ret(&r_old, &a, &[("p1", 1, 1.0, true)])).unwrap();
    sales::sale_commit(&mut s, ret(&r_pending, &a, &[("p1", 1, 1.0, true)])).unwrap();
    mark(&mut s, &a, "accepted");
    mark(&mut s, &r_old, "accepted");
    set_synced_days_ago(&s, &a, 40);
    set_synced_days_ago(&s, &r_old, 40);
    let ancient = ts(Utc::now() - Duration::days(400));
    s.conn()
        .execute(
            "UPDATE outbox SET created_at = ?1 WHERE transaction_id = ?2",
            rusqlite::params![ancient, r_pending],
        )
        .unwrap();
    let r = cleanup::run(&mut s, None, false).unwrap();
    assert_eq!(
        (r.sales, r.queue_items),
        (2, 0),
        "the sale and its synced return"
    );
    assert!(sales::sale_get(&s, &a).is_err());
    assert!(sales::sale_get(&s, &r_old).is_err());
    let kept = sales::sale_get(&s, &r_pending).unwrap();
    assert_eq!(kept["sync"]["status"], "pending");
    assert_eq!(count(&s, "sales"), 1);
    assert_eq!(
        count(&s, "sale_lines"),
        1,
        "lines of removed returns are gone"
    );
    assert_eq!(count(&s, "sale_payments"), 1);
    assert_eq!(queue::next_batch(&s, Some(10)).unwrap().len(), 1);
}

// ---------------------------------------------------------------- contract consistency

#[test]
fn command_lists_agree_with_build_rs_capability_lib_and_docs() {
    let build = include_str!("../../build.rs");
    let cap = include_str!("../../capabilities/local-store.json");
    let lib = include_str!("../lib.rs");
    let docs = include_str!("../../../docs/LOCAL-STORE-API.md");
    let caps: Value = serde_json::from_str(cap).unwrap();
    assert!(
        caps.get("remote").is_none(),
        "the remote origin must never be granted the store"
    );
    let perms: Vec<&str> = caps["permissions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p.as_str().unwrap())
        .collect();
    assert_eq!(perms.len(), super::commands::COMMANDS.len());
    for c in super::commands::COMMANDS {
        assert!(
            build.contains(&format!("\"{c}\"")),
            "{c} missing from build.rs"
        );
        assert!(
            perms.contains(&format!("allow-{}", c.replace('_', "-")).as_str()),
            "{c} missing from capability"
        );
        assert!(
            lib.contains(&format!("store::commands::{c},")),
            "{c} missing from generate_handler!"
        );
        assert!(
            docs.contains(&format!("`{c}`")),
            "{c} missing from docs/LOCAL-STORE-API.md"
        );
    }
}

// ---------------------------------------------------------------- performance

fn timed<T>(runs: u32, mut f: impl FnMut() -> T) -> (f64, f64) {
    let mut worst = 0f64;
    let start = Instant::now();
    for _ in 0..runs {
        let t = Instant::now();
        std::hint::black_box(f());
        worst = worst.max(t.elapsed().as_secs_f64() * 1000.0);
    }
    (start.elapsed().as_secs_f64() * 1000.0 / runs as f64, worst)
}

#[test]
#[ignore = "perf: cargo test --lib store::tests::perf -- --ignored --nocapture"]
fn perf_search_with_20k_products_and_50k_sales() {
    const WORDS: &[&str] = &[
        "coke",
        "fanta",
        "sprite",
        "rice",
        "beans",
        "garri",
        "indomie",
        "peak",
        "milo",
        "bournvita",
        "golden",
        "penny",
        "dangote",
        "sugar",
        "salt",
        "oil",
        "kings",
        "power",
        "pasta",
        "semovita",
        "honeywell",
        "flour",
        "tomato",
        "paste",
        "sardine",
        "titus",
        "geisha",
        "malt",
        "maltina",
        "water",
        "eva",
        "nestle",
    ];
    let d = tempfile::tempdir().unwrap();
    let mut s = open_test_store(d.path());
    let t0 = Instant::now();
    let mut products = Vec::with_capacity(20_000);
    let mut stock = Vec::with_capacity(20_000);
    for i in 0..20_000usize {
        let name = format!(
            "{} {} {}g #{i}",
            WORDS[i % WORDS.len()],
            WORDS[(i / 7) % WORDS.len()],
            50 + i % 950
        );
        products.push(json!({ "id": format!("p{i}"), "sku": format!("SKU{i:05}"), "barcode": format!("600{i:010}"),
            "name": name, "categoryId": format!("cat{}", i % 40), "sellingPrice": 100 + i % 5000, "isActive": true }));
        stock.push(json!({ "id": format!("s{i}"), "productId": format!("p{i}"), "branchId": "branch-ikeja", "onHand": 100 }));
    }
    let mut snap = snapshot(&[]);
    snap["products"] = Value::Array(products);
    snap["stock"] = Value::Array(stock);
    catalog::apply_snapshot(&mut s, &snap).unwrap();
    let snap_ms = t0.elapsed().as_millis();

    // 50,000 sales through sale_commit (synchronous=OFF for the seeding only).
    s.conn().execute_batch("PRAGMA synchronous = OFF;").unwrap();
    let t1 = Instant::now();
    for i in 0..50_000usize {
        let a = format!("p{}", (i * 7) % 20_000);
        let b = format!("p{}", (i * 13 + 5) % 20_000);
        sales::sale_commit(
            &mut s,
            sale(&tid(), &[(a.as_str(), 1.0), (b.as_str(), 2.0)]),
        )
        .unwrap();
    }
    let seed_ms = t1.elapsed().as_millis();
    s.conn()
        .execute_batch("PRAGMA synchronous = FULL;")
        .unwrap();

    // Commit latency with synchronous=FULL (what the till sees).
    let (commit_avg, commit_worst) = timed(50, || {
        sales::sale_commit(&mut s, sale(&tid(), &[("p1", 1.0), ("p2", 1.0)])).unwrap()
    });
    let (s1_avg, s1_worst) = timed(200, || catalog::search(&s, "co", None, Some(30)).unwrap());
    let (s2_avg, s2_worst) = timed(200, || {
        catalog::search(&s, "golden rice", None, Some(30)).unwrap()
    });
    let (s3_avg, s3_worst) = timed(200, || {
        catalog::search(&s, "sku1234", None, Some(30)).unwrap()
    });
    let (s4_avg, s4_worst) = timed(200, || {
        catalog::search(&s, "m", Some("cat7"), Some(50)).unwrap()
    });
    let (b_avg, b_worst) = timed(1000, || {
        catalog::by_barcode(&s, "6000000012345").unwrap().unwrap()
    });
    let (q_avg, q_worst) = timed(100, || queue::next_batch(&s, Some(50)).unwrap());
    let (st_avg, st_worst) = timed(50, || queue::status(&s).unwrap());
    assert_eq!(
        catalog::search(&s, "sku1234", None, Some(30))
            .unwrap()
            .len(),
        10
    );

    println!(
        "PERF 20,000 products / 50,000 sales (100,000 lines, 100,000 stock deltas); db {:.1} MB",
        s.db_size_mb()
    );
    println!("PERF snapshot apply 20k products: {snap_ms} ms; seed 50k sale_commit (sync OFF): {seed_ms} ms");
    println!("PERF sale_commit (sync FULL)             avg {commit_avg:.2} ms, worst {commit_worst:.2} ms");
    println!("PERF catalog_search 'co'                 avg {s1_avg:.2} ms, worst {s1_worst:.2} ms");
    println!("PERF catalog_search 'golden rice'        avg {s2_avg:.2} ms, worst {s2_worst:.2} ms");
    println!("PERF catalog_search 'sku1234'            avg {s3_avg:.2} ms, worst {s3_worst:.2} ms");
    println!("PERF catalog_search 'm' + category       avg {s4_avg:.2} ms, worst {s4_worst:.2} ms");
    println!("PERF catalog_by_barcode                  avg {b_avg:.3} ms, worst {b_worst:.3} ms");
    println!("PERF queue_next_batch(50)                avg {q_avg:.2} ms, worst {q_worst:.2} ms");
    println!("PERF store_status                        avg {st_avg:.2} ms, worst {st_worst:.2} ms");
    for (name, avg) in [
        ("search co", s1_avg),
        ("search golden rice", s2_avg),
        ("search sku", s3_avg),
        ("search category", s4_avg),
        ("by_barcode", b_avg),
    ] {
        assert!(avg < 50.0, "{name} averaged {avg:.2} ms (target < 50 ms)");
    }
}
