use crate::error::{AppError, AppResult};
use crate::file_ops::{resolve_concurrency, CopyPhase, CopyProgress, FileOps};
use crate::history::History;
use crate::journal::Journal;
use crate::models::{
    HistoryEntry, Migration, MigrationStatus, OperationOutcome, ProgressEvent, TransferProgress,
};
use crate::store::Store;
use crate::vss::{shadow_path, SnapshotGuard};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
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

/// 源读取路径解析器：决定 copy_tree/manifest 等**读源**操作用哪个根路径。
///
/// - 非 VSS：[`read`](Self::read) 返回构造时的原始路径（`plan.src` / restore 的 target）。
/// - VSS：`read` 返回快照设备下该路径的映射，读源即走卷影副本，免疫文件锁。
///
/// **写源操作**（`rename`、`create_junction`、`to_recycle_bin`）不经 resolver，
/// 恒用原始路径——快照只读，无法承载写操作。
///
/// 构造参数取 `Option<&str>`（设备路径）而非 `&SnapshotGuard`，便于脱离真实快照单测。
pub struct SrcResolver {
    read_root: PathBuf,
    vss: bool,
}

impl SrcResolver {
    pub fn new(original: &Path, snapshot_device: Option<&str>) -> Self {
        match snapshot_device {
            Some(device) => Self {
                read_root: shadow_path(device, original),
                vss: true,
            },
            None => Self {
                read_root: original.to_path_buf(),
                vss: false,
            },
        }
    }

    /// 读源操作应使用的路径（快照映射或原始路径）。
    pub fn read(&self) -> &Path {
        &self.read_root
    }

    /// 是否处于 VSS 模式（决定是否跳过增量同步）。
    pub fn vss_enabled(&self) -> bool {
        self.vss
    }
}

/// 构造失败返回：携带 (AppError, OperationOutcome)，OperationOutcome 反映
/// 当前 source_changed 状态，使调用方能精确决定是否失效扫描树。
fn fail(
    op: AppError,
    source_changed: bool,
    reason: impl Into<String>,
) -> (AppError, OperationOutcome) {
    (
        op,
        OperationOutcome {
            source_changed,
            reason: reason.into(),
        },
    )
}

/// 把 AppResult 包装为带 source_changed 的 Result，避免每个 `?` 手动匹配。
/// 用于 migrate/restore/break_link 内部已确定 source_changed 状态的阶段。
fn bail<T>(
    r: AppResult<T>,
    source_changed: bool,
    reason: &str,
) -> Result<T, (AppError, OperationOutcome)> {
    r.map_err(|e| fail(e, source_changed, reason))
}

fn transfer_event(
    task_id: &str,
    stage: &str,
    range: (u8, u8),
    progress: &CopyProgress,
    preparing_message: &str,
    copying_message: &str,
) -> ProgressEvent {
    let message = match progress.phase {
        CopyPhase::Preparing => preparing_message,
        CopyPhase::Copying => copying_message,
    };
    let percent = match progress.phase {
        CopyPhase::Preparing => range.0,
        CopyPhase::Copying => {
            let span = range.1.saturating_sub(range.0) as u16;
            range
                .0
                .saturating_add(((progress.percent() as u16 * span) / 100) as u8)
        }
    };
    let mut event = ProgressEvent::new(task_id, stage, percent, message);
    event.transfer = Some(TransferProgress {
        phase: match progress.phase {
            CopyPhase::Preparing => "preparing",
            CopyPhase::Copying => "copying",
        }
        .into(),
        completed_bytes: progress.completed_bytes,
        total_bytes: progress.total_bytes,
        completed_files: progress.completed_files,
        total_files: progress.total_files,
        current_path: progress
            .current_path
            .as_ref()
            .map(|path| path.to_string_lossy().replace('/', "\\")),
    });
    event
}

pub struct MigratePlan {
    pub task_id: String,
    pub migration_id: String,
    pub src: PathBuf,
    pub target: PathBuf,   // 最终 data 路径
    pub tmp: PathBuf,      // data.tmp
    pub old_path: PathBuf, // src.dayu-old-{taskId}
    pub preset_id: Option<String>,
    pub source_volume_serial: String,
    pub target_volume_serial: String,
    /// 是否启用 VSS 卷影快照绕过被占用文件。SnapshotGuard 不进 plan，
    /// 由调用方在 spawn_blocking 内构造后作为 migrate()/restore() 的独立参数传入。
    pub enable_vss: bool,
    /// 复制并发度覆盖。`None` 走默认（min(逻辑核数, 8)）。当前不暴露前端配置。
    pub copy_concurrency: Option<NonZeroUsize>,
}

pub fn migrate(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    plan: &MigratePlan,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
) -> Result<(Migration, OperationOutcome), (AppError, OperationOutcome)> {
    migrate_with_snapshot(ops, store, journal, history, plan, on_progress, cancel, None)
}

/// 带 VSS 快照的迁移。`snapshot` 非空时读源走卷影副本，且跳过增量同步。
///
/// 快照必须在**调用线程**内构造与释放（COM 线程亲和性），由调用方（commands.rs）
/// 在同一 `spawn_blocking` 闭包内创建后传入。guard 在本函数返回时 drop，
/// 末尾的 store/history/journal 命根子调用已完成后才释放。
pub fn migrate_with_snapshot(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    plan: &MigratePlan,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
    snapshot: Option<SnapshotGuard>,
) -> Result<(Migration, OperationOutcome), (AppError, OperationOutcome)> {
    // 读源解析器：VSS 模式下读源走快照设备路径；非 VSS 走原始路径。
    let device = snapshot.as_ref().map(|g| g.device_path());
    let resolver = SrcResolver::new(&plan.src, device);
    // snapshot 作为参数自然持有到函数返回：所有 store/history/journal 命根子调用
    // 完成后才随返回 drop，释放快照（BackupComplete + DeleteSnapshots）。
    let now = || chrono::Utc::now().to_rfc3339();
    let emit = |stage: &str, pct: u8, msg: &str| {
        on_progress(ProgressEvent::new(&plan.task_id, stage, pct, msg));
    };

    // 跟踪 source_changed：改名源前/未改名为 false；改名源后为 true；
    // 任何失败返回时使用当前 source_changed（由各阶段独立设值）。
    let mut source_changed = false;

    // 在写 journal 和复制前拒绝已是链接的源路径。
    if ops.is_reparse_point(&plan.src) {
        return Err(fail(
            AppError::Migrate("源已是 reparse point，不能重复迁移".into()),
            source_changed,
            "migrate_rejected_reparse",
        ));
    }

    journal
        .begin(
            &plan.task_id,
            &plan.migration_id,
            "migrate",
            &plan.src.to_string_lossy(),
            &plan.target.to_string_lossy(),
            &plan.tmp.to_string_lossy(),
            &plan.old_path.to_string_lossy(),
        )
        .map_err(|e| fail(e, source_changed, "migrate_rolled_back"))?;

    // 阶段 a：复制
    emit(stage::COPYING, 0, "准备复制到临时目录");
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = ops.remove_tree(&plan.tmp);
        bail(
            journal.cancel(&plan.task_id),
            source_changed,
            "migrate_rolled_back",
        )?;
        return Err(fail(
            AppError::Cancelled,
            source_changed,
            "migrate_canceled",
        ));
    }
    let outcome = match ops.copy_tree(
        resolver.read(),
        &plan.tmp,
        &|progress| {
            on_progress(transfer_event(
                &plan.task_id,
                stage::COPYING,
                (0, 60),
                progress,
                "正在统计待复制内容",
                "正在复制到迁移仓库",
            ))
        },
        &|| cancel.load(std::sync::atomic::Ordering::Relaxed),
        resolve_concurrency(plan.copy_concurrency),
    ) {
        Ok(o) => o,
        Err(e) => {
            let _ = ops.remove_tree(&plan.tmp);
            if matches!(e, AppError::Cancelled) {
                bail(
                    journal.cancel(&plan.task_id),
                    source_changed,
                    "migrate_rolled_back",
                )?;
            } else {
                bail(
                    journal.fail(&plan.task_id, "复制失败"),
                    source_changed,
                    "migrate_rolled_back",
                )?;
            }
            return Err(fail(e, source_changed, "migrate_rolled_back"));
        }
    };
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        let _ = ops.remove_tree(&plan.tmp);
        bail(
            journal.cancel(&plan.task_id),
            source_changed,
            "migrate_rolled_back",
        )?;
        return Err(fail(
            AppError::Cancelled,
            source_changed,
            "migrate_canceled",
        ));
    }

    // 阶段 b'：复核——零额外目录遍历，直接 diff 复制的两份 outcome manifest。
    emit(stage::VERIFYING, 60, "校验 manifest");
    if !ops
        .diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest)
        .is_empty()
    {
        // 保留 tmp 供排查
        bail(
            journal.fail(&plan.task_id, "manifest 不一致"),
            source_changed,
            "migrate_rolled_back",
        )?;
        return Err(fail(
            AppError::Migrate("manifest 不一致，已保留 tmp 待人工确认".into()),
            source_changed,
            "migrate_rolled_back",
        ));
    }
    bail(
        journal.mark_stage(&plan.task_id, "copied"),
        source_changed,
        "migrate_rolled_back",
    )?;
    bail(
        journal.mark_stage(&plan.task_id, "manifest_ok"),
        source_changed,
        "migrate_rolled_back",
    )?;

    // 阶段 c：改名源 + 增量同步 + 建链
    emit(stage::RENAMING_SOURCE, 70, "改名源目录");
    if let Err(e) = ops.rename(&plan.src, &plan.old_path) {
        bail(
            journal.fail(&plan.task_id, "源改名失败（可能被占用）"),
            source_changed,
            "migrate_rolled_back",
        )?;
        return Err(fail(e, source_changed, "migrate_rolled_back"));
    }
    source_changed = true; // 改名后源路径形态已变
    bail(
        journal.mark_stage(&plan.task_id, "source_renamed"),
        source_changed,
        "migrate_partial",
    )?;

    // 增量同步：old_path -> tmp（捕捉复制期间变化）
    //
    // VSS 模式：快照本身即一致视图，复制期间无“源变化”需追平，整段跳过。
    // 写一条 journal 注解保留 stage 流转兼容（recover_pending_decisions 仍可见
    // incremental_synced 阶段标记）。
    if resolver.vss_enabled() {
        emit(stage::SYNCING, 80, "快照一致视图，跳过增量同步");
        bail(
            journal.mark_stage(&plan.task_id, "incremental_synced"),
            source_changed,
            "migrate_partial",
        )?;
    } else {
        emit(stage::SYNCING, 80, "检查复制期间的变化");
        let old_manifest = bail(
            ops.manifest(&plan.old_path),
            source_changed,
            "migrate_partial",
        )?;
        if ops
            .diff_manifests(&outcome.copied_manifest, &old_manifest)
            .is_empty()
        {
            // 源没变 -> 跳过补传（常态，省掉旧的全量重读）。
        } else {
            // 源变了（罕见）-> 先清空旧 tmp，再在空目录中全量补传。
            // 不能原地覆盖：old_path 中已删除的条目必须从 tmp 消失。
            if let Err(e) = ops.remove_tree(&plan.tmp) {
                let _ = ops.rename(&plan.old_path, &plan.src);
                bail(
                    journal.fail(&plan.task_id, "清空临时目录失败"),
                    source_changed,
                    "migrate_partial",
                )?;
                return Err(fail(e, source_changed, "migrate_partial"));
            }
            let patch = match ops.copy_tree(
                &plan.old_path,
                &plan.tmp,
                &|progress| {
                    on_progress(transfer_event(
                        &plan.task_id,
                        stage::SYNCING,
                        (80, 90),
                        progress,
                        "正在检查复制期间的变化",
                        "正在同步复制期间的变化",
                    ))
                },
                &|| cancel.load(std::sync::atomic::Ordering::Relaxed),
                resolve_concurrency(plan.copy_concurrency),
            ) {
                Ok(p) => p,
                Err(e) => {
                    // 回滚：改回原名
                    let _ = ops.rename(&plan.old_path, &plan.src);
                    if matches!(e, AppError::Cancelled) {
                        let _ = ops.remove_tree(&plan.tmp);
                        bail(
                            journal.cancel(&plan.task_id),
                            source_changed,
                            "migrate_partial",
                        )?;
                    } else {
                        bail(
                            journal.fail(&plan.task_id, "增量同步失败"),
                            source_changed,
                            "migrate_partial",
                        )?;
                    }
                    return Err(fail(e, source_changed, "migrate_partial"));
                }
            };
            if !ops
                .diff_manifests(&patch.copied_manifest, &patch.dst_manifest)
                .is_empty()
            {
                let _ = ops.rename(&plan.old_path, &plan.src);
                bail(
                    journal.fail(&plan.task_id, "二次校验不一致"),
                    source_changed,
                    "migrate_partial",
                )?;
                return Err(fail(
                    AppError::Migrate("增量后 manifest 不一致".into()),
                    source_changed,
                    "migrate_partial",
                ));
            }
            // old_path 在改名后理论上不再被常规路径写入；仍复核一次，
            // 保持现有"补传期间发生变化则回滚"的安全语义。
            let final_old_manifest = bail(
                ops.manifest(&plan.old_path),
                source_changed,
                "migrate_partial",
            )?;
            if !ops
                .diff_manifests(&patch.copied_manifest, &final_old_manifest)
                .is_empty()
            {
                let _ = ops.rename(&plan.old_path, &plan.src);
                bail(
                    journal.fail(&plan.task_id, "补传期间源发生变化"),
                    source_changed,
                    "migrate_partial",
                )?;
                return Err(fail(
                    AppError::Migrate("补传期间源发生变化".into()),
                    source_changed,
                    "migrate_partial",
                ));
            }
        }
        bail(
            journal.mark_stage(&plan.task_id, "incremental_synced"),
            source_changed,
            "migrate_partial",
        )?;
    }

    // tmp -> target 原子改名
    emit(stage::CREATING_JUNCTION, 90, "建立 junction");
    if let Some(parent) = plan.target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = ops.rename(&plan.tmp, &plan.target) {
        bail(
            journal.fail(&plan.task_id, "tmp 改名 target 失败"),
            source_changed,
            "migrate_partial",
        )?;
        return Err(fail(e, source_changed, "migrate_partial"));
    }
    if let Err(e) = ops.create_junction(&plan.src, &plan.target) {
        // 回滚：删可能半成品 junction，target 回 tmp，old 改回原名
        let _ = ops.remove_junction(&plan.src);
        let _ = ops.rename(&plan.target, &plan.tmp);
        let _ = ops.rename(&plan.old_path, &plan.src);
        bail(
            journal.fail(&plan.task_id, "建链失败"),
            source_changed,
            "migrate_partial",
        )?;
        return Err(fail(e, source_changed, "migrate_partial"));
    }
    if !ops.junction_resolves(&plan.src) {
        let _ = ops.remove_junction(&plan.src);
        let _ = ops.rename(&plan.target, &plan.tmp);
        let _ = ops.rename(&plan.old_path, &plan.src);
        bail(
            journal.fail(&plan.task_id, "junction 解析失败"),
            source_changed,
            "migrate_partial",
        )?;
        return Err(fail(
            AppError::Junction("junction 解析失败".into()),
            source_changed,
            "migrate_partial",
        ));
    }
    bail(
        journal.mark_stage(&plan.task_id, "junction_created"),
        source_changed,
        "migrate_partial",
    )?;

    // 阶段 d：先写迁移映射（命根子），再删原
    emit(stage::RECORDING, 95, "记录迁移映射");
    let mut migration = Migration {
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
        bail(
            journal.fail(&plan.task_id, "记录写入失败，oldPath 保留"),
            source_changed,
            "migrate_partial",
        )?;
        return Err(fail(e, source_changed, "migrate_partial"));
    }
    bail(
        journal.mark_stage(&plan.task_id, "record_written"),
        source_changed,
        "migrate_partial",
    )?;

    // 删原（走回收站，失败降级）
    emit(stage::CLEANING, 99, "清理原目录");
    match ops.to_recycle_bin(&plan.old_path) {
        Ok(()) => {
            bail(
                journal.mark_stage(&plan.task_id, "old_recycled"),
                source_changed,
                "migrate_partial",
            )?;
        }
        Err(error) => {
            // junction 已建好、映射已落盘，仅 oldPath 未清理
            migration.status = MigrationStatus::OldPendingDelete;
            migration.pending_cleanup = Some(error.to_string());
            let _ = store.upsert_migration(migration.clone());
        }
    }

    // 历史与终态
    if let Err(e) = history.append(&HistoryEntry {
        op: "migrate".into(),
        id: plan.migration_id.clone(),
        src: plan.src.to_string_lossy().into(),
        dst: plan.target.to_string_lossy().into(),
        result: "ok".into(),
        time: now(),
        duration_sec: 0,
    }) {
        return Err(fail(e, true, "migrate_partial"));
    }
    if let Err(e) = journal.complete(&plan.task_id) {
        return Err(fail(e, true, "migrate_partial"));
    }
    emit(stage::CLEANING, 100, "迁移完成");
    Ok((migration, OperationOutcome::changed("migrated")))
}

pub fn restore(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    mig: &Migration,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
) -> Result<OperationOutcome, (AppError, OperationOutcome)> {
    restore_with_snapshot(ops, store, journal, history, mig, on_progress, cancel, None)
}

/// 带 VSS 快照的还原。`snapshot` 非空时读 target 走卷影副本。
pub fn restore_with_snapshot(
    ops: &dyn FileOps,
    store: &Store,
    journal: &Journal,
    history: &History,
    mig: &Migration,
    on_progress: &dyn Fn(ProgressEvent),
    cancel: &AtomicBool,
    snapshot: Option<SnapshotGuard>,
) -> Result<OperationOutcome, (AppError, OperationOutcome)> {
    use crate::error::AppError;
    let now = || chrono::Utc::now().to_rfc3339();
    let task_id = format!("restore-{}", mig.id);
    let emit = |s: &str, p: u8, m: &str| {
        on_progress(ProgressEvent::new(&task_id, s, p, m));
    };
    let src: std::path::PathBuf = mig.source.clone().into();
    let target: std::path::PathBuf = mig.target.clone().into();
    let restore_tmp = src.with_extension(format!("dayu-restore-{}", mig.id));
    // 读 target 解析器：VSS 走快照设备路径，非 VSS 走原路径。
    let device = snapshot.as_ref().map(|g| g.device_path());
    let resolver = SrcResolver::new(&target, device);

    // source_changed：删 junction 之前为 false；删 junction 之后为 true（src 不再是 junction）。
    // 删 junction 后改名失败时重建 junction：仍然报告 true（删-重建过程 src 曾不是 junction）。
    let mut source_changed = false;

    journal
        .begin(
            &format!("restore-{}", mig.id),
            &mig.id,
            "restore",
            &mig.source,
            &mig.target,
            &restore_tmp.to_string_lossy(),
            &mig.old_path,
        )
        .map_err(|e| fail(e, source_changed, "restore_rolled_back"))?;

    // 校验 junction 仍指向有效 target
    if !ops.junction_resolves(&src) {
        bail(
            journal.fail(&format!("restore-{}", mig.id), "junction 失效"),
            source_changed,
            "restore_rolled_back",
        )?;
        return Err(fail(
            AppError::Junction("junction 已失效，无法还原".into()),
            source_changed,
            "restore_rolled_back",
        ));
    }

    emit(stage::COPYING, 0, "复制回源盘临时目录");
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        bail(
            journal.cancel(&format!("restore-{}", mig.id)),
            source_changed,
            "restore_rolled_back",
        )?;
        return Err(fail(
            AppError::Cancelled,
            source_changed,
            "restore_canceled",
        ));
    }
    if let Err(e) = ops.copy_tree(
        resolver.read(),
        &restore_tmp,
        &|progress| {
            on_progress(transfer_event(
                &task_id,
                stage::COPYING,
                (0, 50),
                progress,
                "正在统计待还原内容",
                "正在复制回原磁盘",
            ))
        },
        &|| cancel.load(std::sync::atomic::Ordering::Relaxed),
        resolve_concurrency(None),
    ) {
        let _ = ops.remove_tree(&restore_tmp);
        if matches!(e, AppError::Cancelled) {
            bail(
                journal.cancel(&task_id),
                source_changed,
                "restore_rolled_back",
            )?;
        } else {
            bail(
                journal.fail(&task_id, "还原复制失败"),
                source_changed,
                "restore_rolled_back",
            )?;
        }
        return Err(fail(e, source_changed, "restore_rolled_back"));
    }

    emit(stage::VERIFYING, 50, "校验 manifest");
    let m1 = bail(ops.manifest(resolver.read()), source_changed, "restore_rolled_back")?;
    let m2 = bail(
        ops.manifest(&restore_tmp),
        source_changed,
        "restore_rolled_back",
    )?;
    if !ops.diff_manifests(&m1, &m2).is_empty() {
        let _ = ops.remove_tree(&restore_tmp);
        bail(
            journal.fail(&format!("restore-{}", mig.id), "manifest 不一致"),
            source_changed,
            "restore_rolled_back",
        )?;
        return Err(fail(
            AppError::Migrate("还原校验不一致".into()),
            source_changed,
            "restore_rolled_back",
        ));
    }
    bail(
        journal.mark_stage(&format!("restore-{}", mig.id), "restore_copied"),
        source_changed,
        "restore_rolled_back",
    )?;
    bail(
        journal.mark_stage(&format!("restore-{}", mig.id), "restore_manifest_ok"),
        source_changed,
        "restore_rolled_back",
    )?;

    // 删 junction -> restore_tmp 原子改名回 src
    emit(stage::REMOVING_JUNCTION, 70, "删除 junction");
    if let Err(e) = ops.remove_junction(&src) {
        let _ = ops.remove_tree(&restore_tmp);
        bail(
            journal.fail(&format!("restore-{}", mig.id), "删 junction 失败"),
            source_changed,
            "restore_rolled_back",
        )?;
        return Err(fail(e, source_changed, "restore_rolled_back"));
    }
    source_changed = true; // junction 已删，src 不再是 junction
    bail(
        journal.mark_stage(&format!("restore-{}", mig.id), "junction_removed"),
        source_changed,
        "restore_partial",
    )?;

    emit(stage::SWITCHING, 85, "切换为普通目录");
    if let Err(e) = ops.rename(&restore_tmp, &src) {
        // 切换失败：优先重建 junction 指回 target，保入口
        let _ = ops.create_junction(&src, &target);
        bail(
            journal.fail(&format!("restore-{}", mig.id), "切换失败，已重建 junction"),
            source_changed,
            "restore_partial",
        )?;
        // 重建后形态恢复（src 又是 junction），但中间过程 src 不再是 junction，
        // 扫描树基于删 junction 前的 src 视图已不一致 → 报告 source_changed=true。
        return Err(fail(e, source_changed, "restore_partial"));
    }
    bail(
        journal.mark_stage(&format!("restore-{}", mig.id), "restore_switched"),
        source_changed,
        "restore_partial",
    )?;

    // 清理 target（走回收站，失败降级）
    emit(stage::CLEANING, 95, "清理目标数据");
    match ops.to_recycle_bin(&target) {
        Ok(()) => {
            bail(
                journal.mark_stage(&format!("restore-{}", mig.id), "restore_target_recycled"),
                source_changed,
                "restore_partial",
            )?;
        }
        Err(_) => {
            let mut m = mig.clone();
            m.status = MigrationStatus::TargetPendingDelete;
            let _ = store.upsert_migration(m);
        }
    }

    if let Err(e) = store.remove_migration(&mig.id) {
        return Err(fail(e, true, "restore_partial"));
    }
    if let Err(e) = history.append(&HistoryEntry {
        op: "restore".into(),
        id: mig.id.clone(),
        src: mig.source.clone(),
        dst: mig.target.clone(),
        result: "ok".into(),
        time: now(),
        duration_sec: 0,
    }) {
        return Err(fail(e, true, "restore_partial"));
    }
    if let Err(e) = journal.complete(&format!("restore-{}", mig.id)) {
        return Err(fail(e, true, "restore_partial"));
    }
    emit(stage::CLEANING, 100, "还原完成");
    Ok(OperationOutcome::changed("restored"))
}

/// 断开链接：删 junction 但保留 target 数据（原路径将不可用，调用方需二次确认）。
///
/// source_changed：成功删除 junction 后为 true（src 不再是 junction）；
/// remove_junction 失败时为 false（src 仍是 junction，扫描树仍有效）。
pub fn break_link(
    ops: &dyn FileOps,
    store: &Store,
    history: &History,
    mig: &Migration,
) -> Result<OperationOutcome, (AppError, OperationOutcome)> {
    let src: std::path::PathBuf = mig.source.clone().into();
    if let Err(e) = ops.remove_junction(&src) {
        return Err(fail(e, false, "break_link_rolled_back"));
    }
    if let Err(e) = store.remove_migration(&mig.id) {
        return Err(fail(e, true, "break_link_partial"));
    }
    if let Err(e) = history.append(&HistoryEntry {
        op: "break_link".into(),
        id: mig.id.clone(),
        src: mig.source.clone(),
        dst: mig.target.clone(),
        result: "ok".into(),
        time: chrono::Utc::now().to_rfc3339(),
        duration_sec: 0,
    }) {
        return Err(fail(e, true, "break_link_partial"));
    }
    Ok(OperationOutcome::changed("broken_link"))
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
            enable_vss: false,
            copy_concurrency: None,
        }
    }

    #[test]
    fn migrate_success_creates_junction_records_and_logs() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "1");
        let src = plan.src.clone();
        let cancel = AtomicBool::new(false);
        let events = RefCell::new(Vec::new());
        let (m, outcome) = migrate(
            &RealFileOps,
            &store,
            &journal,
            &history,
            &plan,
            &|e| events.borrow_mut().push(e),
            &cancel,
        )
        .unwrap();
        assert_eq!(m.status, MigrationStatus::Active);
        assert!(
            outcome.source_changed,
            "成功路径 source_changed 必须为 true"
        );
        assert_eq!(outcome.reason, "migrated");
        assert!(crate::junction::exists(&src), "源路径应已变为 junction");
        assert!(plan.target.join("a.txt").exists(), "数据应落到 target");
        assert!(store
            .load_migrations()
            .unwrap()
            .iter()
            .any(|x| x.id == "m-1"));
        let migrated = history.list(Some("migrate"), None).unwrap();
        assert!(migrated.iter().any(|h| h.id == "m-1" && h.result == "ok"));
        assert!(
            journal.recover_pending().unwrap().is_empty(),
            "任务应已完成"
        );
        let events = events.into_inner();
        let copied = events
            .iter()
            .find(|event| {
                event.stage == stage::COPYING
                    && event
                        .transfer
                        .as_ref()
                        .is_some_and(|progress| progress.phase == "copying")
            })
            .expect("复制阶段应包含结构化传输进度");
        let transfer = copied.transfer.as_ref().unwrap();
        assert_eq!(transfer.total_bytes, Some(5));
        assert_eq!(transfer.total_files, Some(1));
        assert!(copied.percent <= 60, "复制阶段只占迁移总进度的前 60%");
        assert_eq!(events.last().unwrap().percent, 100);
    }

    /// 可编程 mock：复制/校验成功，但建链可注入失败。
    struct MockOps {
        copy_ok: bool,
        manifest_ok: bool,
        junction_fails: bool,
        rename_ok: bool,
        /// 改名源阶段 (src -> old_path) 是否成功
        rename_source_ok: bool,
        /// 增量同步阶段 (old_path -> tmp) copy_tree 是否成功
        incremental_copy_ok: bool,
        /// 回收站 (to_recycle_bin) 是否成功
        recycle_ok: bool,
        /// 复制期间源是否变化：true 时 c' 判定需增量补传。
        source_changed_during_copy: bool,
        /// 补传期间源是否继续变化：true 时 final 复核不一致 -> 回滚改名。
        source_keeps_changing: bool,
        /// 记录每次 create_junction 的 (link, target) 实参，便于断言重建被调用。
        create_junction_calls: std::cell::RefCell<Vec<(std::path::PathBuf, std::path::PathBuf)>>,
        /// 记录每次 copy_tree 的 src 实参（用于断言读源走快照、写源走原路径）。
        copy_src_calls: std::cell::RefCell<Vec<std::path::PathBuf>>,
        /// 记录每次 manifest 的实参（同上）。
        manifest_calls: std::cell::RefCell<Vec<std::path::PathBuf>>,
        /// 记录每次 rename 的 (from, to) 实参。
        rename_calls: std::cell::RefCell<Vec<(std::path::PathBuf, std::path::PathBuf)>>,
        /// manifest(old_path) 调用计数：区分 c' 增量前(第1次)与 final(第2次)。
        manifest_old_count: std::cell::RefCell<usize>,
    }

    /// 与 MockOps::copy_tree 返回的 copied_manifest 内容一致的哨兵条目，
    /// 用于让 b'(copied vs dst) 与 c' 源未变(copied vs old) 的真实 diff 为空。
    fn mock_sentinel_manifest() -> Manifest {
        Manifest {
            root: String::new(),
            entries: vec![crate::file_ops::ManifestEntry {
                rel_path: "mocked.txt".into(),
                is_dir: false,
                size: 1,
                mtime: 0,
                attrs: 0,
            }],
        }
    }

    impl MockOps {
        fn success_path() -> Self {
            Self {
                copy_ok: true,
                manifest_ok: true,
                junction_fails: false,
                rename_ok: true,
                rename_source_ok: true,
                incremental_copy_ok: true,
                recycle_ok: true,
                source_changed_during_copy: false,
                source_keeps_changing: false,
                create_junction_calls: RefCell::new(vec![]),
                copy_src_calls: RefCell::new(vec![]),
                manifest_calls: RefCell::new(vec![]),
                rename_calls: RefCell::new(vec![]),
                manifest_old_count: RefCell::new(0),
            }
        }
    }

    impl FileOps for MockOps {
        fn copy_tree(
            &self,
            s: &Path,
            _d: &Path,
            _p: &dyn Fn(&crate::file_ops::CopyProgress),
            _cancel: &(dyn Fn() -> bool + Sync),
            _concurrency: usize,
        ) -> AppResult<crate::file_ops::CopyOutcome> {
            self.copy_src_calls.borrow_mut().push(s.to_path_buf());
            let is_incremental_stage = s
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains("dayu-old-"))
                .unwrap_or(false);
            let ok = if is_incremental_stage {
                self.incremental_copy_ok
            } else {
                self.copy_ok
            };
            if ok {
                Ok(crate::file_ops::CopyOutcome {
                    copied_manifest: mock_sentinel_manifest(),
                    dst_manifest: mock_sentinel_manifest(),
                    total_bytes: 0,
                    total_files: 0,
                })
            } else {
                Err(crate::error::AppError::Migrate(
                    if is_incremental_stage { "incremental copy fail" } else { "copy fail" }.into(),
                ))
            }
        }
        fn manifest(&self, s: &Path) -> AppResult<Manifest> {
            self.manifest_calls.borrow_mut().push(s.to_path_buf());
            let is_old = s
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.contains("dayu-old-"))
                .unwrap_or(false);
            if is_old {
                let mut count = self.manifest_old_count.borrow_mut();
                *count += 1;
                // 第 1 次 = c' 增量前；第 2 次 = 补传后 final 复核。
                let changed = if *count == 1 {
                    self.source_changed_during_copy
                } else {
                    self.source_keeps_changing
                };
                if changed {
                    Ok(Manifest { root: String::new(), entries: vec![] })
                } else {
                    Ok(mock_sentinel_manifest())
                }
            } else {
                Ok(mock_sentinel_manifest())
            }
        }
        fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String> {
            if !self.manifest_ok {
                return vec!["f.txt".into()];
            }
            // 委托真实比较（与 RealFileOps::diff_manifests 等价：仅比 rel_path/is_dir/size）。
            use std::collections::{HashMap, HashSet};
            let map_a: HashMap<&str, &crate::file_ops::ManifestEntry> =
                a.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
            let map_b: HashMap<&str, &crate::file_ops::ManifestEntry> =
                b.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
            let mut keys: HashSet<&str> = map_a.keys().copied().collect();
            keys.extend(map_b.keys().copied());
            let mut diffs = Vec::new();
            for k in keys {
                match (map_a.get(k), map_b.get(k)) {
                    (Some(x), Some(y)) => {
                        if x.is_dir != y.is_dir || x.size != y.size {
                            diffs.push(k.to_string());
                        }
                    }
                    _ => diffs.push(k.to_string()),
                }
            }
            diffs
        }
        fn rename(&self, f: &Path, t: &Path) -> AppResult<()> {
            self.rename_calls
                .borrow_mut()
                .push((f.to_path_buf(), t.to_path_buf()));
            // 区分阶段：旧名（old_path / restore_tmp）改名与源改名
            let fname = f.file_name().and_then(|s| s.to_str()).unwrap_or("");
            let is_old_path_stage = fname.contains("dayu-old-") || fname.contains("dayu-restore-");
            if is_old_path_stage {
                if self.rename_ok {
                    Ok(())
                } else {
                    Err(crate::error::AppError::Migrate("rename fail".into()))
                }
            } else {
                if self.rename_source_ok {
                    Ok(())
                } else {
                    Err(crate::error::AppError::Migrate("rename source fail".into()))
                }
            }
        }
        fn to_recycle_bin(&self, _p: &Path) -> AppResult<()> {
            if self.recycle_ok {
                Ok(())
            } else {
                Err(crate::error::AppError::Win32("recycle fail".into()))
            }
        }
        fn remove_tree(&self, _p: &Path) -> AppResult<()> {
            Ok(())
        }
        fn is_reparse_point(&self, _p: &Path) -> bool {
            false
        }
        fn dir_exists(&self, _p: &Path) -> bool {
            true
        }
        fn create_junction(&self, l: &Path, t: &Path) -> AppResult<()> {
            self.create_junction_calls
                .borrow_mut()
                .push((l.to_path_buf(), t.to_path_buf()));
            if self.junction_fails {
                Err(crate::error::AppError::Junction(
                    "mock junction fail".into(),
                ))
            } else {
                Ok(())
            }
        }
        fn remove_junction(&self, _l: &Path) -> AppResult<()> {
            Ok(())
        }
        fn junction_resolves(&self, _l: &Path) -> bool {
            !self.junction_fails
        }
    }

    #[test]
    fn migrate_stops_when_source_rename_stage_cannot_be_journaled() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "journal-stage");
        let ops = MockOps::success_path();
        let journal_path = journal.path.clone();
        let cancel = AtomicBool::new(false);

        let result = migrate(
            &ops,
            &store,
            &journal,
            &history,
            &plan,
            &|event| {
                if event.stage == stage::RENAMING_SOURCE {
                    fs::remove_file(&journal_path).unwrap();
                }
            },
            &cancel,
        );

        let (_error, outcome) = result.expect_err("journal 阶段写入失败必须中止迁移");
        assert!(outcome.source_changed);
        assert_eq!(outcome.reason, "migrate_partial");
        assert!(
            ops.create_junction_calls.borrow().is_empty(),
            "journal 未记录源改名时不得继续创建 junction"
        );
    }

    #[test]
    fn migrate_rolls_back_when_copy_fails_keeps_source() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "c");
        let ops = {
            let mut o = MockOps::success_path();
            o.copy_ok = false;
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源目录未被改名（仍存在原文件）
        assert!(plan.src.join("a.txt").exists());
        assert!(
            store.load_migrations().unwrap().is_empty(),
            "不应落盘迁移记录"
        );
        let (_e, outcome) = res.err().unwrap();
        assert!(
            !outcome.source_changed,
            "复制失败回滚应保持 source_changed=false"
        );
        assert_eq!(outcome.reason, "migrate_rolled_back");
    }

    #[test]
    fn migrate_aborts_when_manifest_mismatch_keeps_tmp() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "m");
        let ops = {
            let mut o = MockOps::success_path();
            o.manifest_ok = false;
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源未改名
        assert!(plan.src.join("a.txt").exists());
        let (_e, outcome) = res.err().unwrap();
        assert!(
            !outcome.source_changed,
            "首次校验失败回滚应 source_changed=false"
        );
    }

    #[test]
    fn migrate_cancellation_cleans_tmp_and_logs_canceled() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "x");
        let ops = MockOps::success_path();
        let cancel = AtomicBool::new(true); // 复制前已取消
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        assert!(store.load_migrations().unwrap().is_empty());
        let (_e, outcome) = res.err().unwrap();
        assert!(!outcome.source_changed, "复制前取消应 source_changed=false");
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
            id: format!("m-{id}"),
            schema_version: 1,
            source: src.to_string_lossy().into(),
            target: target.to_string_lossy().into(),
            old_path: String::new(),
            preset: None,
            created_at: "2026-07-18T00:00:00Z".into(),
            status: MigrationStatus::Active,
            source_volume_serial: "C".into(),
            target_volume_serial: "D".into(),
            recycle_bin_ref: String::new(),
            pending_cleanup: None,
        };
        store.upsert_migration(mig.clone()).unwrap();
        (dir, store, journal, history, mig)
    }

    #[test]
    fn restore_success_recovers_dir_and_removes_link() {
        let (_dir, store, journal, history, mig) = restore_fixture("1");
        let src: std::path::PathBuf = mig.source.clone().into();
        let cancel = AtomicBool::new(false);
        let outcome = restore(
            &RealFileOps,
            &store,
            &journal,
            &history,
            &mig,
            &|_| {},
            &cancel,
        )
        .unwrap();
        assert!(outcome.source_changed, "还原成功应 source_changed=true");
        assert_eq!(outcome.reason, "restored");
        assert!(!crate::junction::exists(&src), "junction 应已删除");
        assert!(src.join("a.txt").exists(), "源应恢复为普通目录");
        assert!(
            store
                .load_migrations()
                .unwrap()
                .iter()
                .all(|x| x.id != "m-1"),
            "记录应移除"
        );
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
        let res = restore(
            &RealFileOps,
            &store,
            &journal,
            &history,
            &mig,
            &|_| {},
            &cancel,
        );
        assert!(res.is_err(), "junction 失效时应中止");
        let (_e, outcome) = res.err().unwrap();
        assert!(
            !outcome.source_changed,
            "junction 失效回滚应 source_changed=false"
        );
        assert_eq!(outcome.reason, "restore_rolled_back");
    }

    #[test]
    fn restore_switch_fail_rebuilds_junction() {
        let (_dir, store, journal, history, mig) = restore_fixture("3");
        let src: std::path::PathBuf = mig.source.clone().into();
        let target: std::path::PathBuf = mig.target.clone().into();
        // mock：切换阶段（remove_junction 之后 rename）失败，期望重建 junction
        let ops = {
            let mut o = MockOps::success_path();
            o.rename_ok = false;
            o
        };
        let cancel = AtomicBool::new(false);
        let res = restore(&ops, &store, &journal, &history, &mig, &|_| {}, &cancel);
        assert!(res.is_err());
        // 真实验证：create_junction 被调用一次（重建 junction 保入口）
        let calls = ops.create_junction_calls.borrow();
        assert_eq!(
            calls.len(),
            1,
            "切换失败时应调用 create_junction 重建 junction"
        );
        assert_eq!(calls[0].0, src, "重建 junction 的 link 应为 src");
        assert_eq!(
            calls[0].1, target,
            "重建 junction 的 target 应为 mig.target"
        );
        // 重建 junction 失败路径：删 junction 已发生，src 不再是 junction，
        // 即使重建了也报告 source_changed=true（中间过程 src 不是 junction）。
        let (_e, outcome) = res.err().unwrap();
        assert!(
            outcome.source_changed,
            "删 junction 后改名失败应 source_changed=true"
        );
        assert_eq!(outcome.reason, "restore_partial");
    }

    /// T10 测试 11：migrate 各失败路径 source_changed 跟踪。
    #[test]
    fn migrate_source_changed_tracking() {
        let cancel = AtomicBool::new(false);

        // 成功：source_changed=true, reason="migrated"
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "ok");
        let ops = MockOps::success_path();
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_mig, outcome) = res.expect("成功路径应返 Ok");
        assert!(outcome.source_changed, "成功路径 source_changed=true");
        assert_eq!(outcome.reason, "migrated");

        // 复制失败：source_changed=false
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cpf");
        let ops = {
            let mut o = MockOps::success_path();
            o.copy_ok = false;
            o
        };
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("复制失败应返 Err");
        assert!(!outcome.source_changed, "复制失败 source_changed=false");
        assert_eq!(outcome.reason, "migrate_rolled_back");

        // 改名源失败：source_changed=false（未改名）
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "rsf");
        let ops = {
            let mut o = MockOps::success_path();
            o.rename_source_ok = false;
            o
        };
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("改名源失败应返 Err");
        assert!(!outcome.source_changed, "改名源失败 source_changed=false");
        assert_eq!(outcome.reason, "migrate_rolled_back");

        // 增量同步失败：source_changed=true（已改名）
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "isf");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.incremental_copy_ok = false;
            o
        };
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("增量同步失败应返 Err");
        assert!(
            outcome.source_changed,
            "增量同步失败 source_changed=true（已改名）"
        );
        assert_eq!(outcome.reason, "migrate_partial");

        // 建 junction 失败：source_changed=true
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cjf");
        let ops = {
            let mut o = MockOps::success_path();
            o.junction_fails = true;
            o
        };
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("建链失败应返 Err");
        assert!(outcome.source_changed, "建链失败 source_changed=true");
        assert_eq!(outcome.reason, "migrate_partial");

        // 回收站失败降级：成功路径 source_changed=true
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "recfail");
        let ops = {
            let mut o = MockOps::success_path();
            o.recycle_ok = false;
            o
        };
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (migration, outcome) = res.expect("回收站失败降级仍应成功");
        assert!(outcome.source_changed, "回收站失败降级 source_changed=true");
        assert_eq!(outcome.reason, "migrated");
        assert_eq!(migration.status, MigrationStatus::OldPendingDelete);
        assert_eq!(
            store.load_migrations().unwrap()[0].status,
            MigrationStatus::OldPendingDelete
        );
    }

    /// T10 测试 12：复制阶段失败回滚无残留（src 未改名、无 junction、无 tmp）。
    #[test]
    fn migrate_rolled_back_failure_no_junction_left() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "clean");
        let ops = {
            let mut o = MockOps::success_path();
            o.copy_ok = false;
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err());
        // 源目录未改名（仍为普通目录）
        assert!(plan.src.is_dir(), "源仍应为普通目录");
        assert!(plan.src.join("a.txt").exists(), "源文件应保留");
        // 无 junction
        assert!(!crate::junction::exists(&plan.src));
        // 无 target/tmp 残留
        assert!(!plan.target.exists(), "target 不应存在");
        assert!(!plan.tmp.exists(), "tmp 不应存在");
        // 无迁移记录落盘
        assert!(store.load_migrations().unwrap().is_empty());
    }

    // ===== T10 c' 降级路径测试 =====
    //
    // 用改造后的 MockOps 注入源变化，验证 c' 的四种行为：
    // 源未变跳过补传 / 源变+增量失败回滚 / 源持续变化回滚 / 源变后稳定成功。

    #[test]
    fn migrate_c_prime_skips_sync_when_source_unchanged() {
        // 源没变：c' 应跳过补传，不调用增量 copy_tree(old_path)。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-skip");
        let ops = MockOps::success_path(); // source_changed_during_copy = false
        let cancel = AtomicBool::new(false);
        migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel).unwrap();
        let copies = ops.copy_src_calls.borrow();
        assert!(
            !copies.iter().any(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains("dayu-old-"))
            }),
            "源未变时 c' 不应调用增量 copy_tree(old_path)，实参: {copies:?}"
        );
    }

    #[test]
    fn migrate_c_prime_incremental_fail_rolls_back_rename() {
        // 源变了 + 增量 copy_tree 失败 -> 回滚改名，source_changed=true，migrate_partial。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-incfail");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.incremental_copy_ok = false;
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("源变+增量失败应回滚");
        assert!(outcome.source_changed, "已改名，source_changed=true");
        assert_eq!(outcome.reason, "migrate_partial");
    }

    #[test]
    fn migrate_c_prime_source_keeps_changing_rolls_back_rename() {
        // 源变了、增量成功、但补传期间源再变 -> final 复核不一致 -> 回滚改名。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-keepchanging");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.source_keeps_changing = true; // 补传后 final old_manifest 为空 -> diff 非空
            o
        };
        let cancel = AtomicBool::new(false);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        let (_e, outcome) = res.expect_err("补传期间源再变应回滚");
        assert!(outcome.source_changed, "已改名，source_changed=true");
        assert_eq!(outcome.reason, "migrate_partial");
    }

    #[test]
    fn migrate_c_prime_source_changed_then_stable_succeeds() {
        // 源变了、增量补传成功、补传后源稳定 -> 最终建链成功。
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "cprime-stable");
        let ops = {
            let mut o = MockOps::success_path();
            o.source_changed_during_copy = true;
            o.source_keeps_changing = false; // final old_manifest = 哨兵 -> diff 空
            o
        };
        let cancel = AtomicBool::new(false);
        let (m, outcome) = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel)
            .expect("源变后稳定补传应成功");
        assert!(outcome.source_changed);
        assert_eq!(outcome.reason, "migrated");
        assert_eq!(m.status, MigrationStatus::Active);
    }

    // ===== T10 修复轮：成功路径命根子失败注入测试 =====
    //
    // migrate/restore 的成功路径对 history.append / journal.complete /
    // store.remove_migration 改为 if let Err 传播后，必须在任一调用失败时
    // 返回 Err，且 source_changed=true、reason 为 "..._partial"。
    //
    // 注入策略：History/Journal/Store 是具体结构（非 trait），生产签名不允许
    // 改造（§6.4"不要为测试改生产代码签名"）。改用文件系统层注入：
    // 在迁移/还原最后一个进度回调（CLEANING 99 / CLEANING 95）触发后、
    // 命根子调用之前，把对应路径替换为目录或清空文件，使后续 OpenOptions 或
    // read_all/save_migrations 等真实调用按预期失败。

    /// 辅助：把 path 指向的文件删除，替换为同名目录（让 OpenOptions::append 失败）。
    fn replace_file_with_dir(path: &Path) {
        if path.exists() {
            fs::remove_file(path).expect("test setup: remove file");
        }
        fs::create_dir(path).expect("test setup: create dir at file path");
    }

    /// 辅助：把 path 指向的文件删除（让 read_all 返回空 / find 失败）。
    fn remove_file(path: &Path) {
        if path.exists() {
            fs::remove_file(path).expect("test setup: remove file");
        }
    }

    #[test]
    fn migrate_history_append_failure_reports_partial() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "haf");
        let ops = MockOps::success_path();
        let cancel = AtomicBool::new(false);
        // CLEANING 99 emit 是 history.append 之前最后一个 emit：在回调里把
        // history 文件路径替换为目录，append 将无法以 append 模式打开。
        replace_file_with_dir(&history.path);
        let res = migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel);
        assert!(res.is_err(), "history.append 失败应传播 Err");
        let (_e, outcome) = res.err().unwrap();
        assert!(
            outcome.source_changed,
            "history.append 失败时 source_changed=true（已建链）"
        );
        assert_eq!(outcome.reason, "migrate_partial");
    }

    #[test]
    fn migrate_journal_complete_failure_reports_partial() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "jcf");
        let ops = MockOps::success_path();
        let cancel = AtomicBool::new(false);
        // 在 CLEANING 99 emit 时清空 journal 文件：mark_stage "old_recycled"
        // 失败走 let _ = 静默；history.append 走 history 文件不受影响而成功；
        // journal.complete 时 read_all 返回空 → find 返回 None → 失败。
        let on_progress = |e: crate::models::ProgressEvent| {
            if e.stage == stage::CLEANING && e.percent == 99 {
                remove_file(&journal.path);
            }
        };
        let res = migrate(
            &ops,
            &store,
            &journal,
            &history,
            &plan,
            &on_progress,
            &cancel,
        );
        assert!(res.is_err(), "journal.complete 失败应传播 Err");
        let (_e, outcome) = res.err().unwrap();
        assert!(
            outcome.source_changed,
            "journal.complete 失败时 source_changed=true"
        );
        assert_eq!(outcome.reason, "migrate_partial");
    }

    #[test]
    fn restore_remove_migration_failure_reports_partial() {
        let (_dir, store, journal, history, mig) = restore_fixture("rmf");
        let cancel = AtomicBool::new(false);
        // restore 的成功路径中 store.remove_migration 是第一个命根子调用。
        // 在 CLEANING 95 emit 时把 store.mig_path 替换为目录，load_migrations
        // 内的 fs::read 会以 IsADirectory / AccessDenied 失败，从而失败传播。
        // 即便 recycle 失败，upsert_migration 也是 let _ = 静默，不影响测试目标。
        let on_progress = |e: crate::models::ProgressEvent| {
            if e.stage == stage::CLEANING && e.percent == 95 {
                replace_file_with_dir(&store.mig_path());
            }
        };
        let res = restore(
            &RealFileOps,
            &store,
            &journal,
            &history,
            &mig,
            &on_progress,
            &cancel,
        );
        assert!(res.is_err(), "store.remove_migration 失败应传播 Err");
        let (_e, outcome) = res.err().unwrap();
        assert!(
            outcome.source_changed,
            "store.remove_migration 失败时 source_changed=true"
        );
        assert_eq!(outcome.reason, "restore_partial");
    }

    #[test]
    fn restore_history_append_failure_reports_partial() {
        let (_dir, store, journal, history, mig) = restore_fixture("rhaf");
        let cancel = AtomicBool::new(false);
        // CLEANING 95 emit 时把 history 文件路径替换为目录，让 history.append
        // 失败。store.remove_migration 先于 history.append 且不受影响，仍 Ok。
        let on_progress = |e: crate::models::ProgressEvent| {
            if e.stage == stage::CLEANING && e.percent == 95 {
                replace_file_with_dir(&history.path);
            }
        };
        let res = restore(
            &RealFileOps,
            &store,
            &journal,
            &history,
            &mig,
            &on_progress,
            &cancel,
        );
        assert!(res.is_err(), "history.append 失败应传播 Err");
        let (_e, outcome) = res.err().unwrap();
        assert!(
            outcome.source_changed,
            "history.append 失败时 source_changed=true"
        );
        assert_eq!(outcome.reason, "restore_partial");
    }

    // ===== SrcResolver 单元测试（纯逻辑，无需文件系统）=====

    #[test]
    fn src_resolver_non_vss_returns_original() {
        let r = SrcResolver::new(Path::new(r"C:\Users\x\cache"), None);
        assert!(!r.vss_enabled());
        assert_eq!(r.read(), Path::new(r"C:\Users\x\cache"));
    }

    #[test]
    fn src_resolver_vss_returns_shadow_device_path() {
        let dev = r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7";
        let r = SrcResolver::new(Path::new(r"C:\Users\x\cache"), Some(dev));
        assert!(r.vss_enabled());
        assert_eq!(
            r.read(),
            Path::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy7\Users\x\cache")
        );
    }

    // ===== VSS 模式下读源走快照、写源走原路径的集成断言 =====
    //
    // 用 MockOps 记录实参：migrate_with_snapshot 传入一个“伪快照”——但 SnapshotGuard
    // 无法在无 COM 环境下构造。因此改用 SrcResolver 的纯逻辑 + migrate(None) 的既有断言
    // 覆盖非 VSS 路径；VSS 真实路径分流由 vss.rs 手动门控测试验证。
    // 此处补充：非 VSS 模式下 copy_tree 首参为 plan.src、rename 源改名首参为 plan.src。

    #[test]
    fn non_vss_migrate_reads_original_and_writes_original() {
        let (dir, store, journal, history) = fixtures();
        let plan = plan_for(dir.path(), "rv");
        let src = plan.src.clone();
        let old_path = plan.old_path.clone();
        let ops = MockOps::success_path();
        let cancel = AtomicBool::new(false);
        migrate(&ops, &store, &journal, &history, &plan, &|_| {}, &cancel).unwrap();

        // 首次 copy_tree 首参 = 源原路径（非快照）。
        let copy_calls = ops.copy_src_calls.borrow();
        assert!(
            copy_calls.iter().any(|p| *p == src),
            "非 VSS 首次复制应读原路径 src，实参: {copy_calls:?}"
        );
        // 源改名 rename 首参 = src。
        let renames = ops.rename_calls.borrow();
        assert!(
            renames.iter().any(|(f, _t)| *f == src),
            "改名源应作用于原路径 src，实参: {renames:?}"
        );
        // c' 源未变：不调用增量 copy_tree(old_path)，而是扫描 old_path 记 manifest。
        assert!(
            !copy_calls.iter().any(|p| *p == old_path),
            "源未变时 c' 不应调用 copy_tree(old_path)，实参: {copy_calls:?}"
        );
        let manifest_calls = ops.manifest_calls.borrow();
        assert!(
            manifest_calls.iter().any(|p| *p == old_path),
            "c' 应扫描 old_path 记 manifest，实参: {manifest_calls:?}"
        );
    }
}
