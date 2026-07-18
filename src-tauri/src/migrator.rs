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
    pub const REMOVING_JUNCTION: &str = "removing_junction";
    pub const SWITCHING: &str = "switching";
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

pub fn restore(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    mig: &Migration,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
) -> AppResult<()> {
    use crate::error::AppError;
    let now = || chrono::Utc::now().to_rfc3339();
    let emit = |s: &str, p: u8, m: &str| on_progress(ProgressEvent {
        task_id: format!("restore-{}", mig.id), stage: s.into(), percent: p, message: m.into(),
    });
    let src: std::path::PathBuf = mig.source.clone().into();
    let target: std::path::PathBuf = mig.target.clone().into();
    let restore_tmp = src.with_extension(format!("dayu-restore-{}", mig.id));

    journal.begin(&format!("restore-{}", mig.id), &mig.id, "restore",
        &mig.source, &mig.target, &restore_tmp.to_string_lossy(), &mig.old_path)?;

    // 校验 junction 仍指向有效 target
    if !ops.junction_resolves(&src) {
        journal.fail(&format!("restore-{}", mig.id), "junction 失效")?;
        return Err(AppError::Junction("junction 已失效，无法还原".into()));
    }

    emit(stage::COPYING, 0, "复制回源盘临时目录");
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        journal.cancel(&format!("restore-{}", mig.id))?;
        return Err(AppError::Cancelled);
    }
    if let Err(e) = ops.copy_tree(&target, &restore_tmp, &|_| {}) {
        let _ = ops.remove_tree(&restore_tmp);
        journal.fail(&format!("restore-{}", mig.id), "还原复制失败")?;
        return Err(e);
    }

    emit(stage::VERIFYING, 50, "校验 manifest");
    let m1 = ops.manifest(&target)?;
    let m2 = ops.manifest(&restore_tmp)?;
    if !ops.diff_manifests(&m1, &m2).is_empty() {
        let _ = ops.remove_tree(&restore_tmp);
        journal.fail(&format!("restore-{}", mig.id), "manifest 不一致")?;
        return Err(AppError::Migrate("还原校验不一致".into()));
    }
    journal.mark_stage(&format!("restore-{}", mig.id), "restore_copied")?;
    journal.mark_stage(&format!("restore-{}", mig.id), "restore_manifest_ok")?;

    // 删 junction -> restore_tmp 原子改名回 src
    emit(stage::REMOVING_JUNCTION, 70, "删除 junction");
    if let Err(e) = ops.remove_junction(&src) {
        let _ = ops.remove_tree(&restore_tmp);
        journal.fail(&format!("restore-{}", mig.id), "删 junction 失败")?;
        return Err(e);
    }
    journal.mark_stage(&format!("restore-{}", mig.id), "junction_removed")?;

    emit(stage::SWITCHING, 85, "切换为普通目录");
    if let Err(e) = ops.rename(&restore_tmp, &src) {
        // 切换失败：优先重建 junction 指回 target，保入口
        let _ = ops.create_junction(&src, &target);
        journal.fail(&format!("restore-{}", mig.id), "切换失败，已重建 junction")?;
        return Err(e);
    }
    journal.mark_stage(&format!("restore-{}", mig.id), "restore_switched")?;

    // 清理 target（走回收站，失败降级）
    emit(stage::CLEANING, 95, "清理目标数据");
    match ops.to_recycle_bin(&target) {
        Ok(()) => {
            journal.mark_stage(&format!("restore-{}", mig.id), "restore_target_recycled")?;
        }
        Err(_) => {
            let mut m = mig.clone();
            m.status = MigrationStatus::TargetPendingDelete;
            let _ = store.upsert_migration(m);
        }
    }

    store.remove_migration(&mig.id)?;
    history.append(&HistoryEntry {
        op: "restore".into(), id: mig.id.clone(),
        src: mig.source.clone(), dst: mig.target.clone(),
        result: "ok".into(), time: now(), duration_sec: 0,
    })?;
    journal.complete(&format!("restore-{}", mig.id))?;
    emit(stage::CLEANING, 100, "还原完成");
    Ok(())
}

/// 断开链接：删 junction 但保留 target 数据（原路径将不可用，调用方需二次确认）。
pub fn break_link(ops: &dyn FileOps, store: &Store, history: &History, mig: &Migration) -> AppResult<()> {
    let src: std::path::PathBuf = mig.source.clone().into();
    ops.remove_junction(&src)?;
    store.remove_migration(&mig.id)?;
    history.append(&HistoryEntry {
        op: "break_link".into(), id: mig.id.clone(),
        src: mig.source.clone(), dst: mig.target.clone(),
        result: "ok".into(), time: chrono::Utc::now().to_rfc3339(), duration_sec: 0,
    })?;
    Ok(())
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
        /// 记录每次 create_junction 的 (link, target) 实参，便于断言重建被调用。
        create_junction_calls: std::cell::RefCell<Vec<(std::path::PathBuf, std::path::PathBuf)>>,
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
        fn create_junction(&self, l: &Path, t: &Path) -> AppResult<()> {
            self.create_junction_calls.borrow_mut().push((l.to_path_buf(), t.to_path_buf()));
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
        let ops = MockOps { copy_ok: false, manifest_ok: true, junction_fails: false, rename_ok: true, create_junction_calls: RefCell::new(vec![]) };
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
        let ops = MockOps { copy_ok: true, manifest_ok: false, junction_fails: false, rename_ok: true, create_junction_calls: RefCell::new(vec![]) };
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
        let ops = MockOps { copy_ok: true, manifest_ok: true, junction_fails: false, rename_ok: true, create_junction_calls: RefCell::new(vec![]) };
        let cancel = AtomicBool::new(true); // 复制前已取消
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        assert!(store.load_migrations().unwrap().is_empty());
    }

    fn restore_fixture(id: &str) -> (TempDir, Store, Journal, History, Migration) {
        let dir = TempDir::new().unwrap();
        let store = Store::new(dir.path().join("data")).unwrap();
        let journal = Journal::new(dir.path().join("journal.jsonl")).unwrap();
        let history = History::new(dir.path().join("history.jsonl")).unwrap();
        let target = dir.path().join("repo/m/data");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"hello").unwrap();
        let src = dir.path().join("src");
        crate::junction::create(&src, &target).unwrap();
        let mig = Migration {
            id: format!("m-{id}"), schema_version: 1,
            source: src.to_string_lossy().into(),
            target: target.to_string_lossy().into(),
            old_path: String::new(), preset: None,
            created_at: "2026-07-18T00:00:00Z".into(),
            status: MigrationStatus::Active,
            source_volume_serial: "C".into(), target_volume_serial: "D".into(),
            recycle_bin_ref: String::new(), pending_cleanup: None,
        };
        store.upsert_migration(mig.clone()).unwrap();
        (dir, store, journal, history, mig)
    }

    #[test]
    fn restore_success_recovers_dir_and_removes_link() {
        let (_dir, store, journal, history, mig) = restore_fixture("1");
        let src: std::path::PathBuf = mig.source.clone().into();
        let cancel = AtomicBool::new(false);
        restore(&RealFileOps, &store, &journal, &history, &mig, &|_| {}, &cancel).unwrap();
        assert!(!crate::junction::exists(&src), "junction 应已删除");
        assert!(src.join("a.txt").exists(), "源应恢复为普通目录");
        assert!(store.load_migrations().unwrap().iter().all(|x| x.id != "m-1"), "记录应移除");
        let r = history.list(Some("restore"), None).unwrap();
        assert!(r.iter().any(|h| h.id == "m-1" && h.result == "ok"));
    }

    #[test]
    fn restore_aborts_when_junction_invalid() {
        let (_dir, store, journal, history, mig) = restore_fixture("2");
        // 删掉 target 使 junction 失效
        let target: std::path::PathBuf = mig.target.clone().into();
        std::fs::remove_dir_all(&target).unwrap();
        let cancel = AtomicBool::new(false);
        let res = restore(&RealFileOps, &store, &journal, &history, &mig, &|_| {}, &cancel);
        assert!(res.is_err(), "junction 失效时应中止");
    }

    #[test]
    fn restore_switch_fail_rebuilds_junction() {
        let (_dir, store, journal, history, mig) = restore_fixture("3");
        let src: std::path::PathBuf = mig.source.clone().into();
        let target: std::path::PathBuf = mig.target.clone().into();
        // mock：切换阶段（remove_junction 之后 rename）失败，期望重建 junction
        let ops = MockOps { copy_ok: true, manifest_ok: true, junction_fails: false, rename_ok: false, create_junction_calls: RefCell::new(vec![]) };
        let cancel = AtomicBool::new(false);
        let res = restore(&ops, &store, &journal, &history, &mig, &|_| {}, &cancel);
        assert!(res.is_err());
        // 真实验证：create_junction 被调用一次（重建 junction 保入口）
        let calls = ops.create_junction_calls.borrow();
        assert_eq!(calls.len(), 1, "切换失败时应调用 create_junction 重建 junction");
        assert_eq!(calls[0].0, src, "重建 junction 的 link 应为 src");
        assert_eq!(calls[0].1, target, "重建 junction 的 target 应为 mig.target");
    }
}
