use crate::app_state::{recover_pending_decisions, AppState};
use crate::error::AppResult;
use crate::file_ops::RealFileOps;
use crate::migrator::{self, MigratePlan};
use crate::models::*;
use crate::scanner;
use crate::safety::{precheck, Win32Probe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

#[tauri::command]
pub fn scan_drives(state: State<AppState>) -> AppResult<Vec<ScanItem>> {
    let cfg = state.store.load_config()?;
    // 首版扫描根：当前用户目录 + Program Files（受 excludePaths 过滤）
    let mut roots = vec![];
    if let Some(home) = dirs::home_dir() { roots.push(home); }
    roots.push(PathBuf::from("C:/Program Files"));
    let mut items = Vec::new();
    for r in roots {
        items.extend(scanner::scan(&r, &cfg));
    }
    Ok(items)
}

#[tauri::command]
pub fn precheck_migrate(src: String, state: State<AppState>) -> AppResult<PrecheckReport> {
    let cfg = state.store.load_config()?;
    let existing = state.store.load_migrations()?;
    let src_size = scanner::dir_size(std::path::Path::new(&src));
    Ok(precheck(std::path::Path::new(&src), &cfg, &existing, src_size, &Win32Probe))
}

#[tauri::command]
pub async fn start_migrate(
    migration_id: String, src: String, preset_id: Option<String>,
    app: AppHandle, state: State<'_, AppState>,
) -> AppResult<Migration> {
    let cfg = state.store.load_config()?;
    let src_path = PathBuf::from(&src);
    let preset = preset_id.as_ref().and_then(|id| cfg.presets.iter().find(|p| &p.id == id));
    let subdir = preset.map(|p| p.target_subdir.clone()).unwrap_or_else(|| "custom".into());
    let target = format!("{}/{}/{}/data", cfg.repository.trim_end_matches('/'), subdir, migration_id);
    let tmp = format!("{}.tmp", target);
    let old_path = format!("{}.dayu-old-{}", src.replace('/', "\\"), migration_id);
    let task_id = format!("task-{migration_id}");
    let (src_serial, _) = crate::win32::volume_info(&src_path).unwrap_or((String::new(), false));
    let (tgt_serial, _) = crate::win32::volume_info(std::path::Path::new(&target)).unwrap_or((String::new(), false));

    let plan = MigratePlan {
        task_id: task_id.clone(), migration_id: migration_id.clone(),
        src: src_path, target: target.into(), tmp: tmp.into(), old_path: old_path.into(),
        preset_id: preset_id.clone(),
        source_volume_serial: src_serial, target_volume_serial: tgt_serial,
    };
    let cancel = Arc::new(AtomicBool::new(false));
    *state.cancel_token.lock().unwrap() = Some(cancel.clone());
    let app2 = app.clone();
    let result = migrator::migrate(
        &RealFileOps, &state.store, &state.journal, &state.history, &plan,
        &move |e: ProgressEvent| { let _ = app2.emit("dayu://progress", e); },
        &cancel,
    );
    *state.cancel_token.lock().unwrap() = None;
    result
}

#[tauri::command]
pub fn cancel_migrate(state: State<AppState>) -> bool {
    if let Some(tok) = state.cancel_token.lock().unwrap().as_ref() {
        tok.store(true, Ordering::SeqCst);
        return true;
    }
    false
}

#[tauri::command]
pub async fn start_restore(
    migration_id: String, app: AppHandle, state: State<'_, AppState>,
) -> AppResult<bool> {
    let migs = state.store.load_migrations()?;
    let mig = migs.into_iter().find(|m| m.id == migration_id)
        .ok_or_else(|| crate::error::AppError::Store("迁移记录不存在".into()))?;
    let app2 = app.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    *state.cancel_token.lock().unwrap() = Some(cancel.clone());
    let result = migrator::restore(
        &RealFileOps, &state.store, &state.journal, &state.history, &mig,
        &move |e: ProgressEvent| { let _ = app2.emit("dayu://progress", e); },
        &cancel,
    );
    *state.cancel_token.lock().unwrap() = None;
    result?;
    Ok(true)
}

#[tauri::command]
pub fn list_links(state: State<AppState>) -> AppResult<Vec<crate::app_state::LinkItem>> {
    use crate::app_state::LinkItem;
    let migs = state.store.load_migrations()?;
    Ok(migs.into_iter().map(|m| {
        let valid = crate::junction::verify(std::path::Path::new(&m.source));
        let target_exists = std::path::Path::new(&m.target).exists();
        LinkItem {
            id: m.id.clone(), source: m.source.clone(), target: m.target.clone(),
            preset: m.preset.clone(), created_at: m.created_at.clone(),
            status: serde_json::to_string(&m.status).unwrap_or_default().trim_matches('"').into(),
            valid, broken: !target_exists,
        }
    }).collect())
}

#[tauri::command]
pub fn break_link_cmd(migration_id: String, state: State<AppState>) -> AppResult<bool> {
    let migs = state.store.load_migrations()?;
    let mig = migs.into_iter().find(|m| m.id == migration_id)
        .ok_or_else(|| crate::error::AppError::Store("迁移记录不存在".into()))?;
    migrator::break_link(&RealFileOps, &state.store, &state.history, &mig)?;
    Ok(true)
}

#[tauri::command]
pub fn list_history(op: Option<String>, from: Option<String>, to: Option<String>, state: State<AppState>) -> AppResult<Vec<HistoryEntry>> {
    let range = match (from.as_ref(), to.as_ref()) {
        (Some(a), Some(b)) => Some((a.as_str(), b.as_str())),
        _ => None,
    };
    Ok(state.history.list(op.as_deref(), range)?)
}

#[tauri::command]
pub fn get_config(state: State<AppState>) -> AppResult<Config> {
    state.store.load_config()
}

#[tauri::command]
pub fn save_config(config: Config, state: State<AppState>) -> AppResult<()> {
    state.store.save_config(&config)
}

#[tauri::command]
pub fn export_history(state: State<AppState>) -> AppResult<String> {
    state.history.export_all_json()
}

#[tauri::command]
pub fn get_recovery_advice(state: State<AppState>) -> AppResult<Vec<(String, String, String)>> {
    let pending = state.journal.recover_pending()?;
    Ok(recover_pending_decisions(&pending))
}
