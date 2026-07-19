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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgressEvent {
    pub scanned_dirs: u64,
    pub scanned_files: u64,
    pub current_path: String,
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
