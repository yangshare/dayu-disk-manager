pub mod error;
pub mod models;
pub mod store;
pub mod file_ops;

#[cfg(windows)]
pub mod win32;

#[cfg(windows)]
pub mod junction;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
