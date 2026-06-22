// DigitGlance — unified desktop shell.
//
// This is a thin, hardened native wrapper around the live web app
// (https://digitglance.com/app/dashboard — the all-products hub). One app
// covers every product (Invoice, POS, Books) and all AI features: the user
// signs in once and switches products inside the same window, seeing only the
// products they subscribe to.
//
// The window, its start URL and the security policy are declared in
// `tauri.conf.json`; everything the products do — authentication, RLS/tenant
// isolation, saving to the database, reports, AI, notifications, multi-company,
// roles/permissions, the free trial and subscription gating — runs server-side
// in the real web app exactly as it does in a browser. The desktop binary adds
// no business logic and is deliberately given no privileged IPC access to the
// remote origin, so it stays a safe, dedicated window onto the app.

use tauri::Manager;
use tauri_plugin_updater::UpdaterExt;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Single instance: relaunching the app focuses the existing window
        // instead of opening a second one.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        // Auto-update: the shell checks the GitHub release feed on launch and
        // installs a newer, signed build in the background. The web content
        // itself is always live, so the shell changes rarely.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Best-effort: any error (offline, no published release yet,
                // signature mismatch) is ignored so it never blocks launch.
                if let Ok(updater) = handle.updater() {
                    if let Ok(Some(update)) = updater.check().await {
                        let _ = update.download_and_install(|_, _| {}, || {}).await;
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DigitGlance");
}
