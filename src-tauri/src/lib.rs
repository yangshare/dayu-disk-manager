pub mod app_state;
pub mod commands;
pub mod error;
pub mod file_ops;
pub mod history;
pub mod journal;
pub mod junction;
pub mod migrator;
pub mod models;
pub mod safety;
pub mod scanner;
pub mod store;
pub mod process_probe;

#[cfg(windows)]
pub mod win32;

use app_state::AppState;
use std::sync::{Arc, Mutex};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let data_dir = win32::local_appdata_dayu_dir().expect("无法解析 %LOCALAPPDATA%");
    let store = store::Store::new(&data_dir).expect("无法初始化 store");
    let journal = journal::Journal::new(data_dir.join("operation_journal.jsonl")).expect("无法初始化 journal");
    let history = history::History::new(data_dir.join("history.jsonl")).expect("无法初始化 history");

    // 启动恢复：读取未完成任务并记录到日志（前端 get_recovery_advice 读取展示）
    if let Ok(pending) = journal.recover_pending() {
        if !pending.is_empty() {
            eprintln!("[dayu] 检测到 {} 个未完成任务，已就绪恢复建议", pending.len());
        }
    }

    let state = AppState {
        store, journal, history,
        cancel_token: Arc::new(Mutex::new(None)),
        scan_cancel_token: Arc::new(Mutex::new(None)),
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::scan_drives,
            commands::cancel_scan,
            commands::precheck_migrate,
            commands::start_migrate,
            commands::cancel_migrate,
            commands::start_restore,
            commands::list_links,
            commands::break_link_cmd,
            commands::list_history,
            commands::get_config,
            commands::save_config,
            commands::export_history,
            commands::get_recovery_advice,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
