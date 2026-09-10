//! Wattson desktop application.
//!
//! **Scaffold: phase 2.** This wires the command surface described in `commands.rs` into a
//! Tauri app; there is no window content yet.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState::default())
        .invoke_handler(tauri::generate_handler![
            commands::list_devices,
            commands::open_capture,
            commands::waveform,
            commands::selection_stats,
            commands::event_table,
            commands::event_marks,
            commands::check_budgets,
            commands::parse_energy,
        ])
        .run(tauri::generate_context!())
        .expect("failed to start the Wattson desktop app");
}
