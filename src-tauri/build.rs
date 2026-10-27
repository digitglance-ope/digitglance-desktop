// The app's own IPC commands (the offline till's local store, src/store) are
// declared here so Tauri generates an `allow-<command>` permission for each.
// Once declared, a command is callable ONLY from a window/origin that a
// capability grants it to (capabilities/local-store.json: the app's local
// pages in the main window; never the remote web origin).
const COMMANDS: &[&str] = &[
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

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
