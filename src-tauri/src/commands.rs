use crate::app_state::{recover_pending_decisions, AppState};
use crate::error::{AppError, AppResult};
use crate::file_ops::RealFileOps;
use crate::migrator::{self, MigratePlan};
use crate::models::*;
use crate::scanner::{self, ScanDriveError, TreeStore};
use crate::safety::{migration_conflict, precheck, Win32Probe};
use crate::win32::{ElevationOutcome, VolumeError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// 在单个 write lock 内原子失效当前扫描树。
///
/// 取出 store（take），从取出的 store 读 source 决定 auto_rescan，
/// 清 current_scan，emit 失效事件。即使 emit 失败，current_scan 也已清（旧 scan id stale）。
///
/// 调用方按 outcome.source_changed 决定**是否调**本函数：source_changed=false 不调。
/// 本函数内部不做 source_changed 判断（它只管 take+emit）。
fn invalidate_scan_tree(app: &AppHandle, state: &AppState, outcome: &OperationOutcome) {
    let taken = {
        let mut guard = state.current_scan.write().unwrap();
        guard.take()
    };
    let auto_rescan = match taken {
        Some(store) => store.source() == ScanSource::Mft,
        None => false,
    };
    // emit 在锁外（避免 emit 阻塞持锁）。即使事件投递失败，旧 scan id 也已 stale。
    let _ = app.emit(
        "dayu://scan-invalidated",
        ScanInvalidatedEvent {
            reason: outcome.reason.clone(),
            auto_rescan,
        },
    );
}

/// 纯函数版失效辅助：用于单元测试注入 take_fn/emit_fn。
///
/// take_fn 在调用方提供的 current_scan 上取 Option<Arc<TreeStore>>；
/// emit_fn 接收 (reason, auto_rescan) 元组用于断言（emit 抛错时 fn 内部决定）。
#[cfg(test)]
fn invalidate_scan_tree_impl<F, G>(
    outcome: &OperationOutcome,
    take_fn: F,
    mut emit_fn: G,
) where
    F: FnOnce() -> Option<Arc<TreeStore>>,
    G: FnMut(&str, bool) -> Result<(), ()>,
{
    let taken = take_fn();
    let auto_rescan = match taken {
        Some(store) => store.source() == ScanSource::Mft,
        None => false,
    };
    let _ = emit_fn(&outcome.reason, auto_rescan);
}

/// 把 migrator 返回的 Result<_, (AppError, OperationOutcome)> 转 AppError，
/// 失败时若 outcome.source_changed=true 同步触发扫描树失效。
///
/// 调用方只接 AppError 向上抛；outcome 用于本地副作用。
fn propagate_migrator_error(
    app: &AppHandle,
    state: &AppState,
    err: (AppError, OperationOutcome),
) -> AppError {
    let (app_err, outcome) = err;
    apply_outcome(&outcome, |o| invalidate_scan_tree(app, state, o));
    app_err
}

/// 纯函数版 outcome 应用：仅当 outcome.source_changed=true 时调 on_invalidate_changed。
/// 用于单元测试覆盖命令接线的成功/失败/部分失败三态。
fn apply_outcome<F>(outcome: &OperationOutcome, mut on_invalidate_changed: F)
where
    F: FnMut(&OperationOutcome),
{
    if outcome.source_changed {
        on_invalidate_changed(outcome);
    }
}

#[tauri::command]
pub async fn scan_drive(
    mode: ScanMode,
    app: AppHandle,
    state: State<'_, AppState>,
) -> AppResult<ScanDriveResult> {
    let emitter = Arc::new(move |evt: ScanProgressEvent| {
        let _ = app.emit("dayu://scan-progress", evt);
    });
    scan_drive_impl(mode, &state, emitter).await
}

async fn scan_drive_impl(
    mode: ScanMode,
    state: &AppState,
    emit_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
) -> AppResult<ScanDriveResult> {
    let cfg = state.store.load_config()?;
    let migrations = state.store.load_migrations()?;
    let excluded_paths = cfg.scan.exclude_paths.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_slot = state.scan_cancel_token.clone();
    {
        let mut active = scan_slot.lock().unwrap();
        if active.is_some() {
            return Err(AppError::Conflict("扫描任务已在运行".into()));
        }
        *active = Some(cancel.clone());
    }

    let engine = state.scan_engine.clone();
    let task_cancel = cancel.clone();
    let task_result = tauri::async_runtime::spawn_blocking(move || {
        engine.run(
            mode,
            'C',
            cfg,
            migrations,
            excluded_paths,
            task_cancel,
            emit_progress,
        )
    })
    .await
    .map_err(|e| AppError::Store(format!("扫描任务失败: {e}")));

    {
        let mut active = scan_slot.lock().unwrap();
        if active.as_ref().is_some_and(|current| Arc::ptr_eq(current, &cancel)) {
            *active = None;
        }
    }

    match task_result {
        Ok(Ok(outcome)) => {
            // 先发布 store，再从同一 store 构造 snapshot。
            {
                let mut current = state.current_scan.write().unwrap();
                *current = Some(outcome.store.clone());
            }
            let snapshot = ScanSnapshot {
                scan_id: outcome.store.scan_id().to_string(),
                source: outcome.store.source(),
                roots: outcome.store.roots(),
                filtered_root_count: outcome.store.filtered_root_count(),
                root_file_summary: outcome.store.root_file_summary().clone(),
                diagnostics: outcome.diagnostics,
            };
            Ok(ScanDriveResult::Complete { snapshot })
        }
        Ok(Err(ScanDriveError::NeedsElevation)) => Ok(ScanDriveResult::NeedsElevation),
        Ok(Err(ScanDriveError::FastScanFailure(f))) => {
            Ok(ScanDriveResult::FastScanUnavailable { reason: f })
        }
        Ok(Err(ScanDriveError::Cancelled)) => Err(AppError::Cancelled),
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub fn cancel_scan(state: State<AppState>) -> bool {
    if let Some(token) = state.scan_cancel_token.lock().unwrap().as_ref() {
        token.store(true, Ordering::Relaxed);
        return true;
    }
    false
}

#[tauri::command]
pub async fn expand_node(
    scan_id: String,
    path: String,
    offset: u32,
    limit: u32,
    state: State<'_, AppState>,
) -> AppResult<ChildPage> {
    expand_node_impl(&state, &scan_id, &path, offset, limit)
}

fn expand_node_impl(
    state: &AppState,
    scan_id: &str,
    path: &str,
    offset: u32,
    limit: u32,
) -> AppResult<ChildPage> {
    let store = acquire_store(state, scan_id)?;
    Ok(store.children_page(path, offset, limit))
}

#[tauri::command]
pub async fn reveal_node(
    scan_id: String,
    path: String,
    limit: u32,
    state: State<'_, AppState>,
) -> AppResult<Vec<RevealLevel>> {
    reveal_node_impl(&state, &scan_id, &path, limit)
}

fn reveal_node_impl(
    state: &AppState,
    scan_id: &str,
    path: &str,
    limit: u32,
) -> AppResult<Vec<RevealLevel>> {
    let store = acquire_store(state, scan_id)?;
    store.reveal_pages(path, limit)
}

#[tauri::command]
pub async fn list_recommended(
    scan_id: String,
    state: State<'_, AppState>,
) -> AppResult<Vec<TreeNode>> {
    list_recommended_impl(&state, &scan_id)
}

fn list_recommended_impl(state: &AppState, scan_id: &str) -> AppResult<Vec<TreeNode>> {
    let store = acquire_store(state, scan_id)?;
    Ok(store.recommended())
}

fn acquire_store(state: &AppState, scan_id: &str) -> AppResult<Arc<TreeStore>> {
    let guard = state.current_scan.read().unwrap();
    match guard.as_ref() {
        Some(store) if store.scan_id() == scan_id => Ok(store.clone()),
        _ => Err(AppError::StaleScan),
    }
}

#[tauri::command]
pub async fn precheck_migrate(src: String, state: State<'_, AppState>) -> AppResult<PrecheckReport> {
    let cfg = state.store.load_config()?;
    let existing = state.store.load_migrations()?;
    tauri::async_runtime::spawn_blocking(move || {
        let src_size = scanner::dir_size(std::path::Path::new(&src));
        Ok(precheck(std::path::Path::new(&src), &cfg, &existing, src_size, &Win32Probe))
    })
    .await
    .map_err(|e| AppError::Store(format!("预检任务失败: {e}")))?
}

#[tauri::command]
pub async fn start_migrate(
    migration_id: String, src: String, preset_id: Option<String>,
    app: AppHandle, state: State<'_, AppState>,
) -> AppResult<Migration> {
    let cfg = state.store.load_config()?;
    let src_path = PathBuf::from(&src);
    let existing = state.store.load_migrations()?;
    if let Some(conflict) = migration_conflict(&src_path, &existing) {
        return Err(AppError::Conflict(conflict));
    }
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
    let cancel_slot = state.cancel_token.clone();
    {
        let mut active = cancel_slot.lock().unwrap();
        if active.is_some() {
            return Err(AppError::Conflict("已有迁移或还原任务正在运行".into()));
        }
        *active = Some(cancel.clone());
    }
    let app2 = app.clone();
    let task_cancel = cancel.clone();
    let store = state.store.clone();
    let journal = state.journal.clone();
    let history = state.history.clone();
    let task_result = tauri::async_runtime::spawn_blocking(move || {
        migrator::migrate(
            &RealFileOps, &store, &journal, &history, &plan,
            &move |e: ProgressEvent| { let _ = app2.emit("dayu://progress", e); },
            &task_cancel,
        )
    })
    .await
    .map_err(|e| AppError::Store(format!("迁移任务失败: {e}")));

    {
        let mut active = cancel_slot.lock().unwrap();
        if active.as_ref().is_some_and(|current| Arc::ptr_eq(current, &cancel)) {
            *active = None;
        }
    }

    match task_result {
        Ok(Ok((migration, outcome))) => {
            if outcome.source_changed {
                invalidate_scan_tree(&app, &state, &outcome);
            }
            Ok(migration)
        }
        Ok(Err(err)) => Err(propagate_migrator_error(&app, &state, err)),
        Err(join_err) => Err(AppError::Store(format!("迁移任务失败: {join_err}"))),
    }
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
        .ok_or_else(|| AppError::Store("迁移记录不存在".into()))?;
    let app2 = app.clone();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_slot = state.cancel_token.clone();
    {
        let mut active = cancel_slot.lock().unwrap();
        if active.is_some() {
            return Err(AppError::Conflict("已有迁移或还原任务正在运行".into()));
        }
        *active = Some(cancel.clone());
    }
    let task_cancel = cancel.clone();
    let store = state.store.clone();
    let journal = state.journal.clone();
    let history = state.history.clone();
    let task_result = tauri::async_runtime::spawn_blocking(move || {
        migrator::restore(
            &RealFileOps, &store, &journal, &history, &mig,
            &move |e: ProgressEvent| { let _ = app2.emit("dayu://progress", e); },
            &task_cancel,
        )
    })
    .await
    .map_err(|e| AppError::Store(format!("还原任务失败: {e}")));

    {
        let mut active = cancel_slot.lock().unwrap();
        if active.as_ref().is_some_and(|current| Arc::ptr_eq(current, &cancel)) {
            *active = None;
        }
    }

    match task_result {
        Ok(Ok(outcome)) => {
            if outcome.source_changed {
                invalidate_scan_tree(&app, &state, &outcome);
            }
            Ok(true)
        }
        Ok(Err(err)) => Err(propagate_migrator_error(&app, &state, err)),
        Err(join_err) => Err(AppError::Store(format!("还原任务失败: {join_err}"))),
    }
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
pub fn break_link_cmd(
    migration_id: String,
    app: AppHandle,
    state: State<AppState>,
) -> AppResult<bool> {
    let migs = state.store.load_migrations()?;
    let mig = migs.into_iter().find(|m| m.id == migration_id)
        .ok_or_else(|| AppError::Store("迁移记录不存在".into()))?;
    match migrator::break_link(&RealFileOps, &state.store, &state.history, &mig) {
        Ok(outcome) => {
            if outcome.source_changed {
                invalidate_scan_tree(&app, &state, &outcome);
            }
            Ok(true)
        }
        Err(err) => Err(propagate_migrator_error(&app, &state, err)),
    }
}

#[tauri::command]
pub fn list_history(op: Option<String>, from: Option<String>, to: Option<String>, state: State<AppState>) -> AppResult<Vec<HistoryEntry>> {
    let range = match (from.as_ref(), to.as_ref()) {
        (Some(a), Some(b)) => Some((a.as_str(), b.as_str())),
        _ => None,
    };
    state.history.list(op.as_deref(), range)
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

#[tauri::command]
pub async fn restart_elevated(_state: State<'_, AppState>) -> AppResult<bool> {
    restart_elevated_impl(crate::win32::request_elevation).await
}

async fn restart_elevated_impl(
    elevation_fn: impl Fn(&str) -> Result<ElevationOutcome, VolumeError> + Send + 'static,
) -> AppResult<bool> {
    let outcome = tauri::async_runtime::spawn_blocking(move || elevation_fn("--elevated-scan"))
        .await
        .map_err(|e| AppError::Store(format!("提权任务失败: {e}")))?
        .map_err(|e| AppError::Win32(e.to_string()))?;

    match outcome {
        ElevationOutcome::Launched => Ok(true),
        ElevationOutcome::Cancelled => Err(AppError::Win32("用户取消 UAC 提权".into())),
        ElevationOutcome::Failed { code } => Err(AppError::Win32(format!("UAC 提权启动失败，code={code}"))),
    }
    // 关键：后端任何分支都不退出当前进程。"成功后关闭旧窗口"由前端 T11 处理。
}

#[tauri::command]
pub async fn take_startup_scan_intent(state: State<'_, AppState>) -> AppResult<bool> {
    take_startup_scan_intent_impl(&state)
}

fn take_startup_scan_intent_impl(state: &AppState) -> AppResult<bool> {
    let mut guard = state.startup_scan_intent.lock().unwrap();
    let intent = guard.unwrap_or(false);
    *guard = Some(false);
    Ok(intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::{ScanEngine, ScanOutcome};
    use crate::models::{RootFileSummary, ScanSource, ScanDiagnostics};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, RwLock};

    struct MockScanEngine {
        result: Mutex<Option<Result<ScanOutcome, ScanDriveError>>>,
    }

    impl MockScanEngine {
        fn success(store: Arc<TreeStore>, diagnostics: ScanDiagnostics) -> Self {
            Self {
                result: Mutex::new(Some(Ok(ScanOutcome { store, diagnostics }))),
            }
        }

        fn needs_elevation() -> Self {
            Self {
                result: Mutex::new(Some(Err(ScanDriveError::NeedsElevation))),
            }
        }

        fn fast_scan_failure(f: FastScanFailure) -> Self {
            Self {
                result: Mutex::new(Some(Err(ScanDriveError::FastScanFailure(f)))),
            }
        }

        fn cancelled() -> Self {
            Self {
                result: Mutex::new(Some(Err(ScanDriveError::Cancelled))),
            }
        }
    }

    impl ScanEngine for MockScanEngine {
        fn run(
            &self,
            _mode: ScanMode,
            _root_drive: char,
            _cfg: Config,
            _migrations: Vec<Migration>,
            _excluded_paths: Vec<String>,
            _cancel: Arc<AtomicBool>,
            _on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
        ) -> Result<ScanOutcome, ScanDriveError> {
            self.result.lock().unwrap().take().expect("MockScanEngine 只能使用一次")
        }
    }

    fn empty_store(scan_id: &str) -> Arc<TreeStore> {
        let summary = RootFileSummary {
            direct_file_size_bytes: 0,
            direct_file_count: 0,
            system_metadata_size_bytes: None,
            total_known_size_bytes: 0,
            incomplete: false,
        };
        Arc::new(TreeStore::from_parts(
            scan_id.into(),
            ScanSource::Mft,
            summary,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            Vec::new(),
            0,
            Vec::new(),
        ))
    }

    fn temp_app_state(engine: Arc<dyn ScanEngine>) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::store::Store::new(dir.path()).unwrap();
        let journal = crate::journal::Journal::new(dir.path().join("journal.jsonl")).unwrap();
        let history = crate::history::History::new(dir.path().join("history.jsonl")).unwrap();
        AppState {
            store,
            journal,
            history,
            cancel_token: Arc::new(Mutex::new(None)),
            scan_cancel_token: Arc::new(Mutex::new(None)),
            current_scan: Arc::new(RwLock::new(None)),
            scan_engine: engine,
            startup_scan_intent: Arc::new(Mutex::new(Some(false))),
        }
    }

    fn no_op_progress() -> Arc<dyn Fn(ScanProgressEvent) + Send + Sync> {
        Arc::new(|_| {})
    }

    #[test]
    fn needs_elevation_returns_before_lock_released() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        let result = tauri::async_runtime::block_on(scan_drive_impl(
            ScanMode::Auto,
            &state,
            no_op_progress(),
        ))
        .unwrap();
        assert!(matches!(result, ScanDriveResult::NeedsElevation));
        assert!(state.scan_cancel_token.lock().unwrap().is_none());
    }

    #[test]
    fn fast_scan_failure_preserves_old_snapshot() {
        let old_store = empty_store("old");
        let state = temp_app_state(Arc::new(MockScanEngine::fast_scan_failure(
            FastScanFailure::UnsupportedFilesystem { actual: "fat32".into() },
        )));
        *state.current_scan.write().unwrap() = Some(old_store.clone());

        let result = tauri::async_runtime::block_on(scan_drive_impl(
            ScanMode::Auto,
            &state,
            no_op_progress(),
        ))
        .unwrap();

        assert!(
            matches!(result, ScanDriveResult::FastScanUnavailable { reason: FastScanFailure::UnsupportedFilesystem { .. } })
        );
        assert_eq!(
            state.current_scan.read().unwrap().as_ref().map(|s| s.scan_id().to_string()),
            Some("old".to_string())
        );
        assert!(state.scan_cancel_token.lock().unwrap().is_none());
    }

    #[test]
    fn cancelled_preserves_old_snapshot() {
        let old_store = empty_store("old");
        let state = temp_app_state(Arc::new(MockScanEngine::cancelled()));
        *state.current_scan.write().unwrap() = Some(old_store.clone());

        let result = tauri::async_runtime::block_on(scan_drive_impl(
            ScanMode::Auto,
            &state,
            no_op_progress(),
        ));

        assert!(matches!(result, Err(AppError::Cancelled)));
        assert_eq!(
            state.current_scan.read().unwrap().as_ref().map(|s| s.scan_id().to_string()),
            Some("old".to_string())
        );
    }

    #[test]
    fn success_atomically_replaces_snapshot() {
        let old_store = empty_store("old");
        let new_store = empty_store("new");
        let diagnostics = ScanDiagnostics {
            scanned_records: 1,
            scanned_dirs: 2,
            scanned_files: 3,
            skipped_records: 0,
            orphan_entries: 0,
            hard_link_entries: 0,
        };
        let state = temp_app_state(Arc::new(MockScanEngine::success(
            new_store.clone(),
            diagnostics.clone(),
        )));
        *state.current_scan.write().unwrap() = Some(old_store);

        let result = tauri::async_runtime::block_on(scan_drive_impl(
            ScanMode::Auto,
            &state,
            no_op_progress(),
        ))
        .unwrap();

        match result {
            ScanDriveResult::Complete { snapshot } => {
                assert_eq!(snapshot.scan_id, "new");
                assert_eq!(snapshot.diagnostics.scanned_records, 1);
            }
            _ => panic!("期望 Complete"),
        }
        assert_eq!(
            state.current_scan.read().unwrap().as_ref().map(|s| s.scan_id().to_string()),
            Some("new".to_string())
        );
    }

    #[test]
    fn stale_scan_on_id_mismatch() {
        let store = empty_store("a");
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(store);

        let result = expand_node_impl(&state, "b", "C:\\", 0, 10);
        assert!(matches!(result, Err(AppError::StaleScan)));
    }

    #[test]
    fn stale_scan_on_empty() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        let result = expand_node_impl(&state, "x", "C:\\", 0, 10);
        assert!(matches!(result, Err(AppError::StaleScan)));
    }

    fn sample_store() -> Arc<TreeStore> {
        let summary = RootFileSummary {
            direct_file_size_bytes: 0,
            direct_file_count: 0,
            system_metadata_size_bytes: None,
            total_known_size_bytes: 0,
            incomplete: false,
        };
        let mut nodes = HashMap::new();
        let a = TreeNode {
            path: r"C:\A".into(),
            display_name: "A".into(),
            size_bytes: 100,
            linked_target_size_bytes: None,
            file_count: 0,
            dir_count: 1,
            depth: 1,
            is_reparse: false,
            reparse_tag: None,
            is_junction: false,
            access_state: AccessState::Unknown,
            matched_preset: None,
            category: None,
            auto_migrate: false,
            scan_status: None,
            migration_id: None,
            child_count: 1,
            filtered_child_count: 0,
        };
        let b = TreeNode {
            path: r"C:\A\B".into(),
            display_name: "B".into(),
            size_bytes: 50,
            linked_target_size_bytes: None,
            file_count: 0,
            dir_count: 1,
            depth: 2,
            is_reparse: false,
            reparse_tag: None,
            is_junction: false,
            access_state: AccessState::Unknown,
            matched_preset: None,
            category: None,
            auto_migrate: false,
            scan_status: None,
            migration_id: None,
            child_count: 0,
            filtered_child_count: 0,
        };
        nodes.insert(r"c:\a".into(), a);
        nodes.insert(r"c:\a\b".into(), b);
        let mut children = HashMap::new();
        children.insert(r"c:\".into(), vec![r"c:\a".into()]);
        children.insert(r"c:\a".into(), vec![r"c:\a\b".into()]);
        let mut parent = HashMap::new();
        parent.insert(r"c:\a".into(), r"c:\".into());
        parent.insert(r"c:\a\b".into(), r"c:\a".into());
        Arc::new(TreeStore::from_parts(
            "sample".into(),
            ScanSource::Mft,
            summary,
            nodes,
            children,
            parent,
            vec![r"c:\a".into()],
            0,
            Vec::new(),
        ))
    }

    #[test]
    fn expand_node_returns_child_page() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(sample_store());

        let page = expand_node_impl(&state, "sample", r"C:\A", 0, 10).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.items[0].path, r"C:\A\B");
    }

    #[test]
    fn reveal_node_returns_chain() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(sample_store());

        let levels = reveal_node_impl(&state, "sample", r"C:\A\B", 10).unwrap();
        assert_eq!(levels.len(), 2);
        assert!(levels.iter().any(|l| l.parent_path == r"c:\"));
        assert!(levels.iter().any(|l| l.parent_path == r"C:\A"));
    }

    #[test]
    fn list_recommended_returns_nodes() {
        let summary = RootFileSummary {
            direct_file_size_bytes: 0,
            direct_file_count: 0,
            system_metadata_size_bytes: None,
            total_known_size_bytes: 0,
            incomplete: false,
        };
        let mut nodes = HashMap::new();
        let a = TreeNode {
            path: r"C:\A".into(),
            display_name: "A".into(),
            size_bytes: 100,
            linked_target_size_bytes: None,
            file_count: 0,
            dir_count: 1,
            depth: 1,
            is_reparse: false,
            reparse_tag: None,
            is_junction: false,
            access_state: AccessState::Unknown,
            matched_preset: Some("p1".into()),
            category: None,
            auto_migrate: false,
            scan_status: None,
            migration_id: None,
            child_count: 0,
            filtered_child_count: 0,
        };
        nodes.insert(r"c:\a".into(), a);
        let store_with_rec = Arc::new(TreeStore::from_parts(
            "rec".into(),
            ScanSource::Mft,
            summary,
            nodes,
            HashMap::new(),
            HashMap::new(),
            vec![r"c:\a".into()],
            0,
            vec![r"c:\a".into()],
        ));
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(store_with_rec);

        let nodes = list_recommended_impl(&state, "rec").unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].path, r"C:\A");
    }

    #[test]
    fn cancel_scan_returns_false_when_idle() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        assert!(!cancel_scan_impl(&state));
    }

    fn cancel_scan_impl(state: &AppState) -> bool {
        if let Some(token) = state.scan_cancel_token.lock().unwrap().as_ref() {
            token.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    #[test]
    fn read_commands_dont_hold_scan_lock() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(sample_store());

        // 读树命令使用短读锁，scan_slot（scan_cancel_token）应保持空闲
        let _ = expand_node_impl(&state, "sample", r"C:\A", 0, 10).unwrap();
        let _ = reveal_node_impl(&state, "sample", r"C:\A\B", 10).unwrap();
        let _ = list_recommended_impl(&state, "sample").unwrap();

        assert!(state.scan_cancel_token.try_lock().is_ok());
    }

    fn temp_app_state_with_intent(intent: Option<bool>) -> AppState {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.startup_scan_intent.lock().unwrap() = intent;
        state
    }

    #[test]
    fn take_startup_scan_intent_one_shot_true() {
        let state = temp_app_state_with_intent(Some(true));
        assert!(take_startup_scan_intent_impl(&state).unwrap());
        assert!(!take_startup_scan_intent_impl(&state).unwrap());
        assert!(!take_startup_scan_intent_impl(&state).unwrap());
    }

    #[test]
    fn take_startup_scan_intent_false_on_normal_start() {
        let state = temp_app_state_with_intent(Some(false));
        assert!(!take_startup_scan_intent_impl(&state).unwrap());
    }

    #[test]
    fn take_startup_scan_intent_false_when_none() {
        let state = temp_app_state_with_intent(None);
        assert!(!take_startup_scan_intent_impl(&state).unwrap());
    }

    #[test]
    fn restart_elevated_returns_true_on_launched() {
        let result = tauri::async_runtime::block_on(restart_elevated_impl(|_| {
            Ok(ElevationOutcome::Launched)
        }));
        assert!(result.unwrap());
    }

    #[test]
    fn restart_elevated_returns_err_on_cancelled() {
        let result = tauri::async_runtime::block_on(restart_elevated_impl(|_| {
            Ok(ElevationOutcome::Cancelled)
        }));
        let err = result.unwrap_err().to_string();
        assert!(err.contains("取消"), "错误信息应提示取消: {}", err);
    }

    #[test]
    fn restart_elevated_returns_err_on_failed() {
        let result = tauri::async_runtime::block_on(restart_elevated_impl(|_| {
            Ok(ElevationOutcome::Failed { code: 5 })
        }));
        let err = result.unwrap_err().to_string();
        assert!(err.contains("code=5"), "错误信息应包含 code: {}", err);
    }

    #[test]
    fn restart_elevated_never_exits_process() {
        let called = Arc::new(AtomicBool::new(false));
        let c = called.clone();
        let result = tauri::async_runtime::block_on(restart_elevated_impl(move |_| {
            c.store(true, Ordering::SeqCst);
            Ok(ElevationOutcome::Launched)
        }));
        assert!(called.load(Ordering::SeqCst), "mock 应被调用");
        assert!(result.unwrap(), "函数应正常返回而非退出进程");
    }

    #[test]
    fn csp_is_non_null_and_local_only() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tauri.conf.json"))
            .expect("读取 tauri.conf.json 失败");
        let conf: serde_json::Value = serde_json::from_str(&raw).expect("tauri.conf.json 解析失败");
        let csp = conf["app"]["security"]["csp"].as_str().expect("CSP 必须是非空字符串");
        assert!(csp.contains("default-src 'self'"), "CSP 必须限制 default-src 为 'self': {csp}");
        assert!(!csp.contains("http://"), "CSP 不得包含 http:// 远程源: {csp}");
        assert!(!csp.contains("https://"), "CSP 不得包含 https:// 远程源: {csp}");
    }

    #[test]
    fn no_shell_execute_permission() {
        let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/capabilities/default.json"))
            .expect("读取 capabilities/default.json 失败");
        let cap: serde_json::Value = serde_json::from_str(&raw).expect("capabilities/default.json 解析失败");
        let perms = cap["permissions"].as_array().expect("capabilities 必须有 permissions 数组");
        for p in perms {
            let s = p.as_str().unwrap_or("");
            assert!(!s.contains("shell:allow-execute"), "禁止 shell:allow-execute: {s}");
            assert!(!s.contains("shell:default"), "禁止 shell:default: {s}");
            assert!(!s.contains("process:"), "禁止 process 类 permission: {s}");
        }
    }

    // ===== T10: 文件系统操作后整树失效 =====

    /// 构造合成 TreeStore 用于注入 current_scan。
    fn synthetic_store(scan_id: &str, source: ScanSource) -> Arc<TreeStore> {
        let summary = RootFileSummary {
            direct_file_size_bytes: 0,
            direct_file_count: 0,
            system_metadata_size_bytes: None,
            total_known_size_bytes: 0,
            incomplete: false,
        };
        Arc::new(TreeStore::from_parts(
            scan_id.into(),
            source,
            summary,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            Vec::new(),
            0,
            Vec::new(),
        ))
    }

    /// 1. 发布 ScanSource::Mft 合成 store，调失效，断言 current_scan 变 None + auto_rescan=true。
    #[test]
    fn invalidate_clears_published_mft_store_and_auto_rescans() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(synthetic_store("old", ScanSource::Mft));

        let outcome = OperationOutcome::changed("migrated");
        let emitted = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let current_scan = state.current_scan.clone();
        let emitted_c = emitted.clone();
        invalidate_scan_tree_impl(
            &outcome,
            move || current_scan.write().unwrap().take(),
            move |reason, auto_rescan| {
                emitted_c.lock().unwrap().push((reason.to_string(), auto_rescan));
                Ok(())
            },
        );

        assert!(
            state.current_scan.read().unwrap().is_none(),
            "current_scan 应被 take 清空"
        );
        assert_eq!(
            emitted.lock().unwrap().clone(),
            vec![("migrated".to_string(), true)],
            "auto_rescan 必须为 true（MFT 模式）"
        );
    }

    /// 2. 发布 ScanSource::Filesystem store，调失效，断言 auto_rescan=false。
    #[test]
    fn invalidate_filesystem_store_no_auto_rescan() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(synthetic_store("old", ScanSource::Filesystem));

        let outcome = OperationOutcome::changed("migrated");
        let emitted = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let current_scan = state.current_scan.clone();
        let emitted_c = emitted.clone();
        invalidate_scan_tree_impl(
            &outcome,
            move || current_scan.write().unwrap().take(),
            move |reason, auto_rescan| {
                emitted_c.lock().unwrap().push((reason.to_string(), auto_rescan));
                Ok(())
            },
        );

        assert!(state.current_scan.read().unwrap().is_none());
        assert_eq!(
            emitted.lock().unwrap().clone(),
            vec![("migrated".to_string(), false)],
            "filesystem 模式 auto_rescan=false"
        );
    }

    /// 3. 无活跃扫描时调失效：auto_rescan=false 且不 panic。
    #[test]
    fn invalidate_when_no_store_no_auto_rescan() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        assert!(state.current_scan.read().unwrap().is_none());

        let outcome = OperationOutcome::changed("migrated");
        let emitted = Arc::new(Mutex::new(Vec::<(String, bool)>::new()));
        let current_scan = state.current_scan.clone();
        let emitted_c = emitted.clone();
        invalidate_scan_tree_impl(
            &outcome,
            move || current_scan.write().unwrap().take(),
            move |reason, auto_rescan| {
                emitted_c.lock().unwrap().push((reason.to_string(), auto_rescan));
                Ok(())
            },
        );

        assert_eq!(
            emitted.lock().unwrap().clone(),
            vec![("migrated".to_string(), false)],
            "无 store 时 auto_rescan=false"
        );
    }

    /// 4. emit 失败不影响 take 完成的 current_scan 清除（take 在锁内完成）。
    #[test]
    fn invalidate_clears_before_emit_failure() {
        let state = temp_app_state(Arc::new(MockScanEngine::needs_elevation()));
        *state.current_scan.write().unwrap() = Some(synthetic_store("old", ScanSource::Mft));

        let outcome = OperationOutcome::changed("migrated");
        let emitted = Arc::new(Mutex::new(false));
        let current_scan = state.current_scan.clone();
        let emitted_c = emitted.clone();
        invalidate_scan_tree_impl(
            &outcome,
            move || current_scan.write().unwrap().take(),
            move |_reason, _auto_rescan| {
                *emitted_c.lock().unwrap() = true;
                Err(())  // 模拟 emit 失败
            },
        );

        // 关键断言：take 在锁内已完成，即使 emit 失败 current_scan 也已清。
        assert!(state.current_scan.read().unwrap().is_none());
        assert!(*emitted.lock().unwrap(), "emit_fn 应被调用过");
    }

    /// 5. start_migrate 成功路径（source_changed=true）应触发失效。
    ///    通过 apply_outcome 纯函数验证命令接线语义。
    #[test]
    fn start_migrate_success_invalidates_tree() {
        let outcome = OperationOutcome::changed("migrated");
        let called = Arc::new(Mutex::new(false));
        let called_c = called.clone();
        apply_outcome(&outcome, move |_o| {
            *called_c.lock().unwrap() = true;
        });
        assert!(*called.lock().unwrap(), "成功路径 source_changed=true 必须触发失效");
        assert_eq!(outcome.reason, "migrated");
    }

    /// 6. start_migrate 失败但完全回滚（source_changed=false）不应触发失效。
    #[test]
    fn start_migrate_rolled_back_failure_does_not_invalidate() {
        let outcome = OperationOutcome::unchanged("migrate_rolled_back");
        let called = Arc::new(Mutex::new(false));
        let called_c = called.clone();
        apply_outcome(&outcome, move |_o| {
            *called_c.lock().unwrap() = true;
        });
        assert!(!*called.lock().unwrap(), "完全回滚的失败不应触发失效");
        assert_eq!(outcome.reason, "migrate_rolled_back");
    }

    /// 7. start_migrate 改名后失败（source_changed=true）必须触发失效。
    #[test]
    fn start_migrate_partial_failure_source_changed_invalidates() {
        let outcome = OperationOutcome::changed("migrate_partial");
        let called = Arc::new(Mutex::new(false));
        let called_c = called.clone();
        apply_outcome(&outcome, move |_o| {
            *called_c.lock().unwrap() = true;
        });
        assert!(*called.lock().unwrap(), "源路径已变的失败必须触发失效");
        assert_eq!(outcome.reason, "migrate_partial");
    }

    /// 8. start_restore 成功（source_changed=true）应触发失效。
    #[test]
    fn start_restore_success_invalidates() {
        let outcome = OperationOutcome::changed("restored");
        let called = Arc::new(Mutex::new(false));
        let called_c = called.clone();
        apply_outcome(&outcome, move |_o| {
            *called_c.lock().unwrap() = true;
        });
        assert!(*called.lock().unwrap(), "start_restore 成功必须触发失效");
    }

    /// 9. break_link 成功（source_changed=true）应触发失效。
    #[test]
    fn break_link_success_invalidates() {
        let outcome = OperationOutcome::changed("broken_link");
        let called = Arc::new(Mutex::new(false));
        let called_c = called.clone();
        apply_outcome(&outcome, move |_o| {
            *called_c.lock().unwrap() = true;
        });
        assert!(*called.lock().unwrap(), "break_link 成功必须触发失效");
    }

    /// 10. break_link 失败（remove_junction 失败 source_changed=false）不应失效。
    #[test]
    fn break_link_failure_does_not_invalidate() {
        let outcome = OperationOutcome::unchanged("break_link_rolled_back");
        let called = Arc::new(Mutex::new(false));
        let called_c = called.clone();
        apply_outcome(&outcome, move |_o| {
            *called_c.lock().unwrap() = true;
        });
        assert!(!*called.lock().unwrap(), "break_link 失败 source_changed=false 不应触发失效");
    }
}
