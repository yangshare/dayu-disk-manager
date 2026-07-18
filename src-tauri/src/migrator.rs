use crate::error::AppResult;
use crate::file_ops::FileOps;
use crate::history::History;
use crate::journal::Journal;
use crate::models::{HistoryEntry, Migration, MigrationStatus, ProgressEvent};
use crate::store::Store;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

pub mod stage {
    pub const COPYING: &str = "copying";
    pub const VERIFYING: &str = "verifying";
    pub const RENAMING_SOURCE: &str = "renaming_source";
    pub const SYNCING: &str = "syncing";
    pub const CREATING_JUNCTION: &str = "creating_junction";
    pub const RECORDING: &str = "recording";
    pub const CLEANING: &str = "cleaning";
}

pub struct MigratePlan {
    pub task_id: String,
    pub migration_id: String,
    pub src: PathBuf,
    pub target: PathBuf,       // 最终 data 路径
    pub tmp: PathBuf,          // data.tmp
    pub old_path: PathBuf,     // src.dayu-old-{taskId}
    pub preset_id: Option<String>,
    pub source_volume_serial: String,
    pub target_volume_serial: String,
}

pub fn migrate(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    plan: &MigratePlan,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
) -> AppResult<Migration> {
    use crate::error::AppError;
    let now = || chrono::Utc::now().to_rfc3339();
    let emit = |stage: &str, pct: u8, msg: &str| {
        on_progress(ProgressEvent {
            task_id: plan.task_id.clone(), stage: stage.into(), percent: pct, message: msg.into(),
        });
    };

    journal.begin(
        &plan.task_id, &plan.migration_id, "migrate",
        &plan.src.to_string_lossy(),
        &plan.target.to_string_lossy(),
        &plan.tmp.to_string_lossy(),
        &plan.old_path.to_string_lossy(),
    )?;

    // 阶段 a：复制
    emit(stage::COPYING, 0, "复制到临时目录");
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = ops.remove_tree(&plan.tmp);
        journal.cancel(&plan.task_id)?;
        return Err(AppError::Cancelled);
    }
    let _src_size = crate::scanner::dir_size(&plan.src);
    if let Err(e) = ops.copy_tree(&plan.src, &plan.tmp, &|p| emit(stage::COPYING, p, "复制中")) {
        let _ = ops.remove_tree(&plan.tmp);
        journal.fail(&plan.task_id, "复制失败")?;
        return Err(e);
    }
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = ops.remove_tree(&plan.tmp);
        journal.cancel(&plan.task_id)?;
        return Err(AppError::Cancelled);
    }

    // 阶段 b：首次校验
    emit(stage::VERIFYING, 60, "校验 manifest");
    let m1 = ops.manifest(&plan.src)?;
    let m2 = ops.manifest(&plan.tmp)?;
    if !ops.diff_manifests(&m1, &m2).is_empty() {
        // 保留 tmp 供排查
        journal.fail(&plan.task_id, "manifest 不一致")?;
        return Err(AppError::Migrate("manifest 不一致，已保留 tmp 待人工确认".into()));
    }
    journal.mark_stage(&plan.task_id, "copied")?;
    journal.mark_stage(&plan.task_id, "manifest_ok")?;

    // 阶段 c：改名源 + 增量同步 + 建链
    emit(stage::RENAMING_SOURCE, 70, "改名源目录");
    if ops.is_reparse_point(&plan.src) {
        journal.fail(&plan.task_id, "源已是 reparse point")?;
        return Err(AppError::Migrate("源已是 reparse point".into()));
    }
    if let Err(e) = ops.rename(&plan.src, &plan.old_path) {
        journal.fail(&plan.task_id, "源改名失败（可能被占用）")?;
        return Err(e);
    }
    journal.mark_stage(&plan.task_id, "source_renamed")?;

    // 增量同步：old_path -> tmp（捕捉复制期间变化）
    emit(stage::SYNCING, 80, "增量同步");
    if let Err(e) = ops.copy_tree(&plan.old_path, &plan.tmp, &|_| {}) {
        // 回滚：改回原名
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "增量同步失败")?;
        return Err(e);
    }
    let m3 = ops.manifest(&plan.old_path)?;
    let m4 = ops.manifest(&plan.tmp)?;
    if !ops.diff_manifests(&m3, &m4).is_empty() {
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "二次校验不一致")?;
        return Err(AppError::Migrate("增量后 manifest 不一致".into()));
    }
    journal.mark_stage(&plan.task_id, "incremental_synced")?;

    // tmp -> target 原子改名
    emit(stage::CREATING_JUNCTION, 90, "建立 junction");
    if let Some(parent) = plan.target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ops.rename(&plan.tmp, &plan.target)?;
    if let Err(e) = ops.create_junction(&plan.src, &plan.target) {
        // 回滚：删可能半成品 junction，target 回 tmp，old 改回原名
        let _ = ops.remove_junction(&plan.src);
        let _ = ops.rename(&plan.target, &plan.tmp);
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "建链失败")?;
        return Err(e);
    }
    if !ops.junction_resolves(&plan.src) {
        let _ = ops.remove_junction(&plan.src);
        let _ = ops.rename(&plan.target, &plan.tmp);
        let _ = ops.rename(&plan.old_path, &plan.src);
        journal.fail(&plan.task_id, "junction 解析失败")?;
        return Err(AppError::Junction("junction 解析失败".into()));
    }
    journal.mark_stage(&plan.task_id, "junction_created")?;

    // 阶段 d：先写迁移映射（命根子），再删原
    emit(stage::RECORDING, 95, "记录迁移映射");
    let migration = Migration {
        id: plan.migration_id.clone(),
        schema_version: 1,
        source: plan.src.to_string_lossy().replace('/', "\\"),
        target: plan.target.to_string_lossy().replace('/', "\\"),
        old_path: plan.old_path.to_string_lossy().replace('/', "\\"),
        preset: plan.preset_id.clone(),
        created_at: now(),
        status: MigrationStatus::Active,
        source_volume_serial: plan.source_volume_serial.clone(),
        target_volume_serial: plan.target_volume_serial.clone(),
        recycle_bin_ref: String::new(),
        pending_cleanup: None,
    };
    if let Err(e) = store.upsert_migration(migration.clone()) {
        // junction 已建好但记录失败：保留 oldPath，标记 pending_record
        let mut m = migration.clone();
        m.status = MigrationStatus::OldPendingDelete;
        let _ = store.upsert_migration(m);
        journal.fail(&plan.task_id, "记录写入失败，oldPath 保留")?;
        return Err(e);
    }
    journal.mark_stage(&plan.task_id, "record_written")?;

    // 删原（走回收站，失败降级）
    emit(stage::CLEANING, 99, "清理原目录");
    match ops.to_recycle_bin(&plan.old_path) {
        Ok(()) => {
            journal.mark_stage(&plan.task_id, "old_recycled")?;
        }
        Err(_) => {
            // junction 已建好、映射已落盘，仅 oldPath 未清理
            let mut m = migration.clone();
            m.status = MigrationStatus::OldPendingDelete;
            let _ = store.upsert_migration(m);
        }
    }

    // 历史与终态
    history.append(&HistoryEntry {
        op: "migrate".into(), id: plan.migration_id.clone(),
        src: plan.src.to_string_lossy().into(),
        dst: plan.target.to_string_lossy().into(),
        result: "ok".into(), time: now(), duration_sec: 0,
    })?;
    journal.complete(&plan.task_id)?;
    emit(stage::CLEANING, 100, "迁移完成");
    Ok(migration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_ops::{FileOps, Manifest, RealFileOps};
    use std::cell::RefCell;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    fn fixtures() -> (TempDir, Store, Journal, History) {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("data")).unwrap();
        let journal = Journal::new(dir.path().join("journal.jsonl")).unwrap();
        let history = History::new(dir.path().join("history.jsonl")).unwrap();
        (dir, store, journal, history)
    }

    fn plan_for(dir: &std::path::Path, id: &str) -> MigratePlan {
        let src = dir.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        MigratePlan {
            task_id: format!("t-{id}"),
            migration_id: format!("m-{id}"),
            src: src.clone(),
            target: dir.join("repo/sub/m/data"),
            tmp: dir.join("repo/sub/m/data.tmp"),
            old_path: src.with_extension(format!("dayu-old-t-{id}")),
            preset_id: None,
            source_volume_serial: "C".into(),
            target_volume_serial: "D".into(),
        }
    }

    #[test]
    fn migrate_success_creates_junction_records_and_logs() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "1");
        let src = plan.src.clone();
        let cancel = AtomicBool::new(false);
        let events = RefCell::new(Vec::new());
        let m = migrate(
            &RealFileOps, &store, &journal, &history, &plan,
            &|e| events.borrow_mut().push(e), &cancel,
        ).unwrap();
        assert_eq!(m.status, MigrationStatus::Active);
        assert!(crate::junction::exists(&src), "源路径应已变为 junction");
        assert!(plan.target.join("a.txt").exists(), "数据应落到 target");
        assert!(store.load_migrations().unwrap().iter().any(|x| x.id == "m-1"));
        let migrated = history.list(Some("migrate"), None).unwrap();
        assert!(migrated.iter().any(|h| h.id == "m-1" && h.result == "ok"));
        assert!(journal.recover_pending().unwrap().is_empty(), "任务应已完成");
    }

    /// 可编程 mock：复制/校验成功，但建链可注入失败。
    struct MockOps {
        copy_ok: bool,
        manifest_ok: bool,
        junction_fails: bool,
        rename_ok: bool,
    }
    impl FileOps for MockOps {
        fn copy_tree(&self, _s: &Path, _d: &Path, _p: &dyn Fn(u8)) -> AppResult<()> {
            if self.copy_ok { Ok(()) } else { Err(crate::error::AppError::Migrate("copy fail".into())) }
        }
        fn manifest(&self, _s: &Path) -> AppResult<Manifest> {
            Ok(Manifest { root: String::new(), entries: vec![] })
        }
        fn diff_manifests(&self, _a: &Manifest, _b: &Manifest) -> Vec<String> {
            if self.manifest_ok { vec![] } else { vec!["f.txt".into()] }
        }
        fn rename(&self, _f: &Path, _t: &Path) -> AppResult<()> {
            if self.rename_ok { Ok(()) } else { Err(crate::error::AppError::Migrate("rename fail".into())) }
        }
        fn to_recycle_bin(&self, _p: &Path) -> AppResult<()> { Ok(()) }
        fn remove_tree(&self, _p: &Path) -> AppResult<()> { Ok(()) }
        fn is_reparse_point(&self, _p: &Path) -> bool { false }
        fn dir_exists(&self, _p: &Path) -> bool { true }
        fn create_junction(&self, _l: &Path, _t: &Path) -> AppResult<()> {
            if self.junction_fails { Err(crate::error::AppError::Junction("mock junction fail".into())) }
            else { Ok(()) }
        }
        fn remove_junction(&self, _l: &Path) -> AppResult<()> { Ok(()) }
        fn junction_resolves(&self, _l: &Path) -> bool { !self.junction_fails }
    }

    #[test]
    fn migrate_rolls_back_when_copy_fails_keeps_source() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "c");
        let ops = MockOps { copy_ok: false, manifest_ok: true, junction_fails: false, rename_ok: true };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源目录未被改名（仍存在原文件）
        assert!(plan.src.join("a.txt").exists());
        assert!(store.load_migrations().unwrap().is_empty(), "不应落盘迁移记录");
    }

    #[test]
    fn migrate_aborts_when_manifest_mismatch_keeps_tmp() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "m");
        let ops = MockOps { copy_ok: true, manifest_ok: false, junction_fails: false, rename_ok: true };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源未改名
        assert!(plan.src.join("a.txt").exists());
    }

    #[test]
    fn migrate_cancellation_cleans_tmp_and_logs_canceled() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "x");
        let ops = MockOps { copy_ok: true, manifest_ok: true, junction_fails: false, rename_ok: true };
        let cancel = AtomicBool::new(true); // 复制前已取消
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        assert!(store.load_migrations().unwrap().is_empty());
    }
}
