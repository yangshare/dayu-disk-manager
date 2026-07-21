use crate::journal::Journal;
use crate::models::JournalEntry;
use crate::scanner::{ScanEngine, TreeStore};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::AtomicBool;

/// 链接列表项（list_links 返回）。定义于此，commands.rs 复用。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkItem {
    pub id: String,
    pub source: String,
    pub target: String,
    pub preset: Option<String>,
    pub created_at: String,
    pub status: String,
    pub valid: bool,        // junction 是否解析正常
    pub broken: bool,       // target 不存在
}

pub struct AppState {
    pub store: crate::store::Store,
    pub journal: Journal,
    pub history: crate::history::History,
    /// 当前迁移/还原任务的取消令牌；无任务时为 None
    pub cancel_token: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// 当前扫描任务的取消令牌；无任务时为 None
    pub scan_cancel_token: Arc<Mutex<Option<Arc<AtomicBool>>>>,
    /// 当前扫描快照（None = 无活跃扫描结果）
    pub current_scan: Arc<RwLock<Option<Arc<TreeStore>>>>,
    /// 扫描引擎（生产 RealScanEngine，测试可注入 MockScanEngine）
    pub scan_engine: Arc<dyn ScanEngine>,
}

/// 启动时根据 journal 恢复决策。
/// 返回每个未完成任务的 (migration_id, stage, decision) 供前端展示与人工处理。
pub fn recover_pending_decisions(entries: &[JournalEntry]) -> Vec<(String, String, String)> {
    entries.iter().map(|e| {
        let decision = match e.stage.as_str() {
            "created" | "copied" | "manifest_ok" => "清 tmp 可重试".into(),
            "source_renamed" | "incremental_synced" => "oldPath 改回原名可重试".into(),
            "junction_created" | "record_written" => "已建链，补写或确认".into(),
            s if s.starts_with("restore_") => "还原中断，按阶段恢复".into(),
            _ => "待人工确认".into(),
        };
        (e.migration_id.clone(), e.stage.clone(), decision)
    }).collect()
}

#[allow(dead_code)]
fn _unused(_p: PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::JournalEntry;

    fn entry(stage: &str, mid: &str) -> JournalEntry {
        JournalEntry {
            task_id: "t1".into(), op: "migrate".into(), migration_id: mid.into(),
            stage: stage.into(), src: "C:/s".into(), dst: "D:/d".into(),
            tmp: "D:/d.tmp".into(), old_path: "C:/s.old".into(),
            time: "2026-07-18T00:00:00Z".into(), final_mark: None,
        }
    }

    #[test]
    fn copied_stage_decision_is_clean_tmp() {
        let d = recover_pending_decisions(&[entry("copied", "m1")]);
        assert_eq!(d.len(), 1);
        assert!(d[0].2.contains("清 tmp"));
    }

    #[test]
    fn junction_created_decision_keeps_link() {
        let d = recover_pending_decisions(&[entry("junction_created", "m2")]);
        assert!(d[0].2.contains("已建链"));
    }

    #[test]
    fn restore_stage_decision_recognized() {
        let d = recover_pending_decisions(&[entry("restore_copied", "m3")]);
        assert!(d[0].2.contains("还原"));
    }

    #[test]
    fn status_serializes_snake_case() {
        use crate::models::MigrationStatus;
        let s = serde_json::to_string(&MigrationStatus::OldPendingDelete).unwrap();
        assert_eq!(s, "\"old_pending_delete\"");
        // trim_matches 后即前端 LinkItem.status 期望的字面量
        assert_eq!(s.trim_matches('"'), "old_pending_delete");
        // 顺带验证其它变体也走 snake_case
        assert_eq!(serde_json::to_string(&MigrationStatus::Active).unwrap(), "\"active\"");
        assert_eq!(serde_json::to_string(&MigrationStatus::TargetPendingDelete).unwrap(), "\"target_pending_delete\"");
        assert_eq!(serde_json::to_string(&MigrationStatus::PendingManualConfirm).unwrap(), "\"pending_manual_confirm\"");
    }
}
