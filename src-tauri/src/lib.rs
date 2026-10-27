// DigitGlance — desktop shell.
//
// This is a thin, hardened native wrapper around the live web app. ONE shell
// builds THREE separate single-product apps, each sold on its own: DigitGlance
// Trade, DigitGlance Books and DigitGlance School. The product-specific window
// title, bundle identifier, icons, landing page and updater feed all come from
// `tauri.<product>.conf.json`, merged over the base `tauri.conf.json` at build
// time (see .github/workflows/release-products.yml).
//
// Product isolation is deliberate and must not regress: the app badged
// "DigitGlance Trade" must never become a second front door into Books. Each
// launcher page under `dist/<product>/` announces its product to the web app
// with `?client=<product>-desktop` on the launch URL, and the platform then
// hides cross-product switching and refuses to render the other product. That
// signal rides in the URL rather than the User-Agent because Tauri REPLACES
// the UA instead of appending to it. School runs on its own origin with its own
// auth, so it needs no help staying isolated, but it carries
// `?client=school-desktop` for a different reason: DigitGlance School is
// licensed to a school as an institution and sold only on the web, so the web
// app uses that signal to hide account creation and every billing surface in
// this shell (Apple 3.1.1 / 3.1.3(c), Google Play billing).
//
// The window, its start URL and the security policy are declared in
// `tauri.conf.json`; everything the products do — authentication, RLS/tenant
// isolation, saving to the database, reports, AI, notifications, multi-company,
// roles/permissions, the free trial and subscription gating — runs server-side
// in the real web app exactly as it does in a browser. The desktop binary adds
// no business logic and is deliberately given no privileged IPC access to the
// remote origin, so it stays a safe, dedicated window onto the app.
//
// The ONE exception is the offline till's local store (`store` module, Trade
// only; unrelated to the `store` Cargo feature, which is the Microsoft Store
// flavour): an encrypted SQLite database plus Tauri commands for the till that
// is bundled locally (till-dist/, DigitGlance Trade's `till` window). Those
// commands, the till's device HTTP and its save dialog are granted by
// capabilities/local-store.json and till.json to that window only; the remote
// origin (the `backoffice` window and every Books/School window) still has no
// IPC at all. See docs/LOCAL-STORE-API.md and src/till.rs.

#[cfg(desktop)]
pub mod store;
#[cfg(desktop)]
pub mod till;

use tauri::Manager;
#[cfg(all(not(feature = "store"), not(desktop)))]
use tauri_plugin_updater::UpdaterExt;

/// Product identifiers that carry the offline till (Trade, and the base dev config).
#[cfg(desktop)]
const STORE_PRODUCTS: &[&str] = &["com.digitglance.trade", "com.digitglance.desktop"];

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default()
        // Single instance: relaunching the app focuses the existing window
        // instead of opening a second one. (It also guarantees one process,
        // so one writer, on the till's database file.)
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Trade opens the till; Books and School the main launcher window.
            let window = app
                .get_webview_window("till")
                .or_else(|| app.get_webview_window("main"));
            if let Some(window) = window {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        // The till's device HTTP (scoped to the API in capabilities/till.json)
        // and its dialogs. Granted to the till window only.
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_dialog::init());

    // Auto-update (direct-download builds only): the shell checks the GitHub
    // release feed on launch and installs a newer, signed build in the
    // background. The web content itself is always live, so the shell changes
    // rarely.
    //
    // Microsoft Store (MSIX) builds are compiled with `--features store` and
    // skip this entirely: the Store delivers updates for packaged apps, and an
    // MSIX install directory is read-only, so a self-update could not apply
    // anyway. See docs/STORE.md.
    #[cfg(not(feature = "store"))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    #[cfg(desktop)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        store::commands::store_open,
        store::commands::store_close,
        store::commands::device_identity_get,
        store::commands::device_identity_save,
        store::commands::device_identity_clear,
        store::commands::device_secret_get,
        store::commands::store_status,
        store::commands::storage_check,
        store::commands::settings_get,
        store::commands::settings_set,
        store::commands::catalog_apply_snapshot,
        store::commands::catalog_apply_changes,
        store::commands::catalog_get_cursor,
        store::commands::catalog_search,
        store::commands::catalog_by_barcode,
        store::commands::catalog_get_products,
        store::commands::categories_list,
        store::commands::customers_search,
        store::commands::customer_get,
        store::commands::staff_list,
        store::commands::pin_verify,
        store::commands::sale_commit,
        store::commands::shift_open,
        store::commands::shift_close,
        store::commands::shift_current,
        store::commands::sales_recent,
        store::commands::sale_get,
        store::commands::queue_next_batch,
        store::commands::queue_mark,
        store::commands::queue_failed_list,
        store::commands::queue_retry_failed,
        store::commands::cleanup_run,
        store::commands::export_pending,
        store::commands::audit_append,
        store::commands::audit_take_unsent,
        store::commands::audit_mark_sent,
        till::open_backoffice,
    ]);

    // Closing the till with unsent sales asks first (src/till.rs).
    #[cfg(desktop)]
    let builder = builder.on_window_event(till::on_window_event);

    builder
        .setup(|app| {
            // Local store state. Nothing is opened or created until the till
            // calls store_open / device_identity_get.
            #[cfg(desktop)]
            {
                let enabled = STORE_PRODUCTS.contains(&app.config().identifier.as_str());
                let data_dir = app.path().app_local_data_dir()?.join("offline-pos");
                let keystore = std::sync::Arc::new(store::crypto_key::OsKeyStore::new(
                    store::crypto_key::SERVICE,
                ));
                app.manage(store::commands::StoreState::new(data_dir, keystore, enabled));
            }

            // Desktop: download, then install only once no shift is open on
            // this till (D17, src/till.rs).
            #[cfg(all(not(feature = "store"), desktop))]
            tauri::async_runtime::spawn(till::update_when_no_shift(app.handle().clone()));

            #[cfg(all(not(feature = "store"), not(desktop)))]
            {
                let handle = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    // Best-effort: any error (offline, no published release
                    // yet, signature mismatch) is ignored so it never blocks
                    // launch.
                    if let Ok(updater) = handle.updater() {
                        if let Ok(Some(update)) = updater.check().await {
                            let _ = update.download_and_install(|_, _| {}, || {}).await;
                        }
                    }
                });
            }
            let _ = &app;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DigitGlance");
}
