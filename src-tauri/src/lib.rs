pub mod error;
pub mod models;
pub mod store;

#[cfg(windows)]
pub mod win32;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
