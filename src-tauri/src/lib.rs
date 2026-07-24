pub mod app_state;
pub mod commands;
pub mod error;
pub mod file_ops;
pub mod history;
pub mod journal;
pub mod junction;
pub mod mft;
pub mod migrator;
pub mod models;
pub mod process_probe;
pub mod safety;
pub mod scanner;
pub mod store;
pub mod vss;

#[cfg(windows)]
pub mod win32;

use app_state::AppState;
use std::sync::{Arc, Mutex, RwLock};

use crate::scanner::RealScanEngine;
use tauri_plugin_log::{RotationStrategy, Target, TargetKind, TimezoneStrategy};

/// 纯函数：判断启动参数是否包含 --elevated-scan 意图。
pub fn is_elevated_scan_start(args: &[impl AsRef<str>]) -> bool {
    args.iter().any(|a| a.as_ref() == "--elevated-scan")
}

fn report_pending_recovery(journal: &journal::Journal, report: impl FnOnce(usize)) {
    if let Ok(pending) = journal.recover_pending() {
        if !pending.is_empty() {
            report(pending.len());
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let data_dir = win32::local_appdata_dayu_dir().expect("无法解析 %LOCALAPPDATA%");
    let log_dir = data_dir.join("logs");

    let store = store::Store::new(&data_dir).expect("无法初始化 store");
    let journal = journal::Journal::new(data_dir.join("operation_journal.jsonl"))
        .expect("无法初始化 journal");
    let history =
        history::History::new(data_dir.join("history.jsonl")).expect("无法初始化 history");
    let recovery_journal = journal.clone();

    let is_elevated_scan = is_elevated_scan_start(&std::env::args().collect::<Vec<_>>());
    let startup_scan_intent = Arc::new(Mutex::new(Some(is_elevated_scan)));

    let state = AppState {
        store,
        journal,
        history,
        cancel_token: Arc::new(Mutex::new(None)),
        scan_cancel_token: Arc::new(Mutex::new(None)),
        current_scan: Arc::new(RwLock::new(None)),
        scan_engine: Arc::new(RealScanEngine),
        startup_scan_intent,
    };

    tauri::Builder::default()
        .manage(state)
        .plugin(
            tauri_plugin_log::Builder::new()
                .clear_targets()
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::Folder {
                        path: log_dir,
                        file_name: Some("dayu".to_string()),
                    }),
                    Target::new(TargetKind::Webview),
                ])
                .level(log::LevelFilter::Info)
                .max_file_size(5_000_000)
                .rotation_strategy(RotationStrategy::KeepOne)
                .timezone_strategy(TimezoneStrategy::UseLocal)
                .build(),
        )
        // 插件在应用 setup 前完成初始化，确保恢复告警能进入文件日志。
        .setup(move |_app| {
            report_pending_recovery(&recovery_journal, |count| {
                log::warn!("[dayu] 检测到 {count} 个未完成任务，已就绪恢复建议");
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::scan_drive,
            commands::expand_node,
            commands::reveal_node,
            commands::list_recommended,
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
            commands::restart_elevated,
            commands::take_startup_scan_intent,
        ])
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_process::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_elevated_scan_flag_present() {
        assert!(is_elevated_scan_start(&[
            "dayu".to_string(),
            "--elevated-scan".to_string()
        ]));
    }

    #[test]
    fn parse_elevated_scan_flag_absent() {
        assert!(!is_elevated_scan_start(&["dayu".to_string()]));
    }

    #[test]
    fn parse_elevated_scan_flag_mixed() {
        assert!(is_elevated_scan_start(&[
            "dayu".to_string(),
            "--other".to_string(),
            "--elevated-scan".to_string()
        ]));
    }

    #[test]
    fn reports_pending_recovery_once_with_pending_count() {
        let temp = tempfile::tempdir().unwrap();
        let journal = journal::Journal::new(temp.path().join("operation_journal.jsonl")).unwrap();
        journal
            .begin(
                "task-1",
                "migration-1",
                "migrate",
                "C:/source",
                "D:/target",
                "D:/target.tmp",
                "C:/source.old",
            )
            .unwrap();

        let mut reported = None;
        report_pending_recovery(&journal, |count| reported = Some(count));

        assert_eq!(reported, Some(1));
    }
}
