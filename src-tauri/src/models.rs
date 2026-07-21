use serde::{Deserialize, Serialize};

// ===== Config =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub schema_version: u32,
    pub repository: String,
    pub scan: ScanConfig,
    #[serde(default)]
    pub presets: Vec<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanConfig {
    pub min_size_mb: u64,
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PresetCategory {
    Communication,
    GameLibrary,
    DevCache,
    Ide,
    Container,
    AppInstall,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub category: PresetCategory,
    /// 路径匹配模板，可含环境变量占位（%USERPROFILE% / %LOCALAPPDATA% / %APPDATA%）。
    /// scanner 展开后与扫描到的目录路径匹配。
    pub match_paths: Vec<String>,
    /// 用于占用检测提示的进程名（不带扩展名的小写名）。
    pub match_processes: Vec<String>,
    /// true=预检通过即可一键迁移；false=需用户确认风险。
    pub auto_migrate: bool,
    /// 仓库下的子目录名（如 "wechat"），最终目标 = repository/{targetSubdir}/{uuid}/data
    pub target_subdir: String,
}

// ===== Migration =====
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MigrationStatus {
    Active,
    OldPendingDelete,
    TargetPendingDelete,
    PendingManualConfirm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Migration {
    pub id: String,
    pub schema_version: u32,
    pub source: String,
    pub target: String,
    pub old_path: String,
    pub preset: Option<String>,
    pub created_at: String,
    pub status: MigrationStatus,
    pub source_volume_serial: String,
    pub target_volume_serial: String,
    #[serde(default)]
    pub recycle_bin_ref: String,
    #[serde(default)]
    pub pending_cleanup: Option<String>,
}

// ===== Scan =====

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanSource {
    Mft,
    Filesystem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanMode {
    Auto,
    Mft,
    Filesystem,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanDriveResult {
    NeedsElevation,
    FastScanUnavailable { reason: FastScanFailure },
    Complete { snapshot: ScanSnapshot },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FastScanFailure {
    UnsupportedFilesystem { actual: String },
    UnsupportedNtfsVersion { major: u16, minor: u16 },
    InvalidVolumeData,
    RootRecordMissing,
    ExcessiveRecordErrors { skipped: u64, scanned: u64 },
    Io { code: Option<i32> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSnapshot {
    pub scan_id: String,
    pub source: ScanSource,
    pub roots: Vec<TreeNode>,
    pub filtered_root_count: u32,
    pub root_file_summary: RootFileSummary,
    pub diagnostics: ScanDiagnostics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanDiagnostics {
    pub scanned_records: u64,
    pub scanned_dirs: u64,
    pub scanned_files: u64,
    pub skipped_records: u64,
    pub orphan_entries: u64,
    pub hard_link_entries: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanItemStatus {
    Migrated,
    MigrationPending,
    LinkBroken,
    ExistingLink,
    ContainsMigrated,
    ContainsLink,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AccessState {
    Unknown,
    Accessible,
    Inaccessible,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RootFileSummary {
    pub direct_file_size_bytes: u64,
    pub direct_file_count: u64,
    pub system_metadata_size_bytes: Option<u64>,
    pub total_known_size_bytes: u64,
    pub incomplete: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TreeNode {
    pub path: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub linked_target_size_bytes: Option<u64>,
    pub file_count: u64,
    pub dir_count: u64,
    pub depth: u32,
    pub is_reparse: bool,
    pub reparse_tag: Option<u32>,
    pub is_junction: bool,
    pub access_state: AccessState,
    pub matched_preset: Option<String>,
    pub category: Option<PresetCategory>,
    pub auto_migrate: bool,
    pub scan_status: Option<ScanItemStatus>,
    pub migration_id: Option<String>,
    pub child_count: u32,
    pub filtered_child_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildPage {
    pub items: Vec<TreeNode>,
    pub total: u32,
    pub next_offset: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RevealLevel {
    pub parent_path: String,
    pub page: ChildPage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanItem {
    pub path: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub matched_preset: Option<String>,
    pub category: Option<PresetCategory>,
    pub auto_migrate: bool,
    pub is_junction: bool,
    pub inaccessible: bool,
    pub scan_status: Option<ScanItemStatus>,
    pub migration_id: Option<String>,
}

// ===== Journal =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub task_id: String,
    /// "migrate" | "restore"
    pub op: String,
    pub migration_id: String,
    /// 见 journal.rs 的 Stage 常量
    pub stage: String,
    pub src: String,
    pub dst: String,
    pub tmp: String,
    pub old_path: String,
    pub time: String,
    /// None=进行中；Some("completed"|"failed"|"canceled")=任务终态
    #[serde(default)]
    pub final_mark: Option<String>,
}

// ===== History =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// "migrate" | "restore" | "delete_link" | "break_link"
    pub op: String,
    pub id: String,
    pub src: String,
    pub dst: String,
    /// "ok" | "failed" | "canceled"
    pub result: String,
    pub time: String,
    pub duration_sec: u64,
}

// ===== Progress event (后端 emit -> 前端) =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferProgress {
    /// "preparing" | "copying"
    pub phase: String,
    pub completed_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes: Option<u64>,
    pub completed_files: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub task_id: String,
    pub stage: String,
    pub percent: u8,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transfer: Option<TransferProgress>,
}

impl ProgressEvent {
    pub fn new(task_id: impl Into<String>, stage: &str, percent: u8, message: &str) -> Self {
        Self {
            task_id: task_id.into(),
            stage: stage.into(),
            percent,
            message: message.into(),
            transfer: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CurrentPhase {
    ReadingMft,
    Aggregating,
    Annotating,
    WalkingFs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgressEvent {
    pub scanned_records: u64,
    pub scanned_dirs: u64,
    pub scanned_files: u64,
    pub estimated_record_slots: u64,
    pub current_phase: CurrentPhase,
}

// ===== Precheck =====
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrecheckReport {
    pub ok: bool,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
    pub source_size_bytes: u64,
    pub target_free_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_snapshot() -> ScanSnapshot {
        ScanSnapshot {
            scan_id: "123".into(),
            source: ScanSource::Mft,
            roots: Vec::new(),
            filtered_root_count: 0,
            root_file_summary: RootFileSummary {
                direct_file_size_bytes: 0,
                direct_file_count: 0,
                system_metadata_size_bytes: None,
                total_known_size_bytes: 0,
                incomplete: false,
            },
            diagnostics: ScanDiagnostics {
                scanned_records: 0,
                scanned_dirs: 0,
                scanned_files: 0,
                skipped_records: 0,
                orphan_entries: 0,
                hard_link_entries: 0,
            },
        }
    }

    #[test]
    fn scan_drive_result_json_needs_elevation() {
        let v = serde_json::to_value(ScanDriveResult::NeedsElevation).unwrap();
        assert_eq!(v, serde_json::json!({"kind": "needs_elevation"}));
    }

    #[test]
    fn scan_drive_result_json_fast_scan_unavailable() {
        let v = serde_json::to_value(ScanDriveResult::FastScanUnavailable {
            reason: FastScanFailure::UnsupportedFilesystem { actual: "fat32".into() },
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "kind": "fast_scan_unavailable",
                "reason": { "kind": "unsupported_filesystem", "actual": "fat32" }
            })
        );
    }

    #[test]
    fn scan_drive_result_json_complete() {
        let v = serde_json::to_value(ScanDriveResult::Complete {
            snapshot: minimal_snapshot(),
        })
        .unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.get("kind"), Some(&serde_json::json!("complete")));
        assert!(obj.contains_key("snapshot"));
        let snap = obj.get("snapshot").unwrap();
        assert!(snap.get("scanId").is_some());
        assert!(snap.get("source").is_some());
        assert!(snap.get("roots").is_some());
        assert!(snap.get("filteredRootCount").is_some());
        assert!(snap.get("rootFileSummary").is_some());
        assert!(snap.get("diagnostics").is_some());
    }

    #[test]
    fn fast_scan_failure_each_variant() {
        let cases = vec![
            (
                FastScanFailure::UnsupportedFilesystem { actual: "exfat".into() },
                "unsupported_filesystem",
            ),
            (
                FastScanFailure::UnsupportedNtfsVersion { major: 1, minor: 2 },
                "unsupported_ntfs_version",
            ),
            (FastScanFailure::InvalidVolumeData, "invalid_volume_data"),
            (FastScanFailure::RootRecordMissing, "root_record_missing"),
            (
                FastScanFailure::ExcessiveRecordErrors { skipped: 1, scanned: 2 },
                "excessive_record_errors",
            ),
            (FastScanFailure::Io { code: Some(5) }, "io"),
            (FastScanFailure::Io { code: None }, "io"),
        ];
        for (failure, expected_kind) in cases {
            let v = serde_json::to_value(&failure).unwrap();
            assert_eq!(
                v.get("kind").unwrap().as_str().unwrap(),
                expected_kind,
                "{:?}",
                failure
            );
        }
    }
}
