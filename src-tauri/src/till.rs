//! DigitGlance Trade's two windows and the guards around them.
//!
//! - `till`: the bundled offline till (`till-dist/`, the Trade config's
//!   frontendDist). The only window granted the local store, the HTTP plugin
//!   (device API) and the save dialog (capabilities/till.json,
//!   capabilities/local-store.json).
//! - `backoffice`: the web back office, opened from the till by
//!   `open_backoffice`. It loads the local launcher page
//!   (`till-dist/backoffice/index.html`, the connectivity gate), which then
//!   navigates to https://digitglance.com. It has core permissions only; no
//!   capability names it for the store, so web pages can never reach it.
//!
//! Closing the till with sales not yet uploaded asks first (an MSIX
//! uninstall deletes the app's data, so the till also offers Export unsent
//! sales). The self-updater never installs while a shift is open (D17).

use std::sync::atomic::{AtomicBool, Ordering};

use tauri::{AppHandle, Manager, Runtime, WebviewUrl, WebviewWindowBuilder, Window, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

use crate::store::commands::StoreState;

pub const TILL_LABEL: &str = "till";
pub const BACKOFFICE_LABEL: &str = "backoffice";

/// Set once the person has agreed to close with unsent sales (or there were none).
static CLOSE_CONFIRMED: AtomicBool = AtomicBool::new(false);

/// Opens the web back office in its own window, or brings it to the front.
#[tauri::command]
pub async fn open_backoffice<R: Runtime>(app: AppHandle<R>) -> Result<(), String> {
    if let Some(w) = app.get_webview_window(BACKOFFICE_LABEL) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    WebviewWindowBuilder::new(
        &app,
        BACKOFFICE_LABEL,
        WebviewUrl::App("backoffice/index.html".into()),
    )
    .title("DigitGlance Trade - Back office")
    .inner_size(1320.0, 880.0)
    .min_inner_size(960.0, 640.0)
    .center()
    .build()
    .map(|_| ())
    .map_err(|e| format!("Could not open the back office: {e}"))
}

/// The sentence the close guard shows.
pub fn unsent_message(n: i64) -> String {
    let what = if n == 1 {
        "1 sale or shift on this till is".to_string()
    } else {
        format!("{n} sales or shifts on this till are")
    };
    format!(
        "{what} not uploaded to DigitGlance yet.\n\n\
         They are safe on this computer and upload the next time the till is open and online. \
         Do not uninstall the app or reset this computer until they have uploaded \
         (or use Export unsent sales in the till first).\n\nClose the till anyway?"
    )
}

/// Window events: the till's close guard. Closing the till ends the app
/// (the back office window goes with it), so it is the one place to ask.
pub fn on_window_event<R: Runtime>(window: &Window<R>, event: &WindowEvent) {
    if window.label() != TILL_LABEL {
        return;
    }
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    if CLOSE_CONFIRMED.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_close();
    let app = window.app_handle().clone();
    tauri::async_runtime::spawn(async move {
        let unsent = match app.try_state::<StoreState>() {
            Some(state) => state.unsent_count().await.unwrap_or(0),
            None => 0,
        };
        if unsent <= 0 {
            CLOSE_CONFIRMED.store(true, Ordering::SeqCst);
            app.exit(0);
            return;
        }
        let parent = app.get_webview_window(TILL_LABEL);
        let mut dialog = app
            .dialog()
            .message(unsent_message(unsent))
            .title("Sales not uploaded yet")
            .kind(MessageDialogKind::Warning)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Close anyway".into(),
                "Keep the till open".into(),
            ));
        if let Some(w) = parent.as_ref() {
            dialog = dialog.parent(w);
        }
        let handle = app.clone();
        // Non-blocking: the answer arrives on the callback, the event loop keeps running.
        dialog.show(move |close| {
            if close {
                CLOSE_CONFIRMED.store(true, Ordering::SeqCst);
                handle.exit(0);
            }
        });
    });
}

/// How long the updater waits before looking at the shift again.
#[cfg(not(feature = "store"))]
const SHIFT_RECHECK: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// Direct-download builds: check the feed, download, then install only when
/// no shift is open on this till (D17). A shift open at download time
/// postpones the install; it is re-checked every five minutes until the
/// shift closes (or the app quits, and the next launch starts over).
#[cfg(not(feature = "store"))]
pub async fn update_when_no_shift<R: Runtime>(app: AppHandle<R>) {
    use tauri_plugin_updater::UpdaterExt;
    let Ok(updater) = app.updater() else { return };
    let Ok(Some(update)) = updater.check().await else {
        return;
    };
    let Ok(bytes) = update.download(|_, _| {}, || {}).await else {
        return;
    };
    loop {
        let open = match app.try_state::<StoreState>() {
            Some(state) => state.shift_is_open().await,
            None => Ok(false),
        };
        // An unreadable store is not proof there is no open shift: wait.
        if matches!(open, Ok(false)) {
            break;
        }
        sleep(SHIFT_RECHECK).await;
    }
    let _ = update.install(bytes);
}

#[cfg(not(feature = "store"))]
async fn sleep(d: std::time::Duration) {
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(d)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsent_message_counts() {
        assert!(unsent_message(1).starts_with("1 sale or shift on this till is not uploaded"));
        assert!(unsent_message(3).starts_with("3 sales or shifts on this till are not uploaded"));
        assert!(unsent_message(3).contains("Export unsent sales"));
    }

    #[test]
    fn till_capabilities_keep_the_store_off_remote_pages() {
        let till: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/till.json")).unwrap();
        let store: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/local-store.json")).unwrap();
        let default: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        for c in [&till, &store] {
            assert!(c.get("remote").is_none(), "no remote origin may get the till's IPC");
            assert_eq!(c["windows"], serde_json::json!([TILL_LABEL]));
        }
        assert!(default.get("remote").is_none());
        assert_eq!(default["permissions"], serde_json::json!(["core:default"]));
        // The production HTTP scope is the API only.
        let http = till["permissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p.get("identifier").and_then(|i| i.as_str()) == Some("http:default"))
            .expect("http:default");
        assert_eq!(
            http["allow"],
            serde_json::json!([{ "url": "https://api.digitglance.com/*" }])
        );
        // The shipped configs list their capabilities, so the dev localhost scope is opt-in.
        let base: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let caps = base["app"]["security"]["capabilities"].as_array().unwrap();
        assert!(!caps.iter().any(|c| c == "till-dev-http"));
        let trade: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.trade.conf.json")).unwrap();
        assert_eq!(trade["build"]["frontendDist"], "../till-dist");
        assert_eq!(trade["app"]["windows"][0]["label"], TILL_LABEL);
    }
}
