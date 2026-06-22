// Prevent a second console window from opening alongside the app on Windows in
// release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    digitglance_lib::run()
}
