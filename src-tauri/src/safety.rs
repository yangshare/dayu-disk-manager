#[cfg(not(windows))]
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::{Config, Migration, MigrationStatus, PrecheckReport, Preset};
use crate::scanner::matches_preset;
use std::path::Path;

/// 系统探针抽象：盘空间、卷信息、占用进程、运行中进程。
/// 生产用 Win32Probe，测试用 mock。
pub trait SystemProbe {
    /// (卷序列号, 是否 NTFS)
    fn volume_info(&self, p: &Path) -> AppResult<(String, bool)>;
    fn disk_free(&self, p: &Path) -> AppResult<u64>;
    /// Restart Manager 检测锁定某路径的进程（对文件路径有效，目录路径生产返回 None）。
    fn locked_processes(&self, p: &Path) -> AppResult<Option<Vec<String>>>;
    /// 当前系统运行中的进程名列表（小写、去 .exe 后缀）。
    fn running_processes(&self) -> AppResult<Vec<String>>;
}

/// 生产实现，包装 win32 + process_probe。
pub struct Win32Probe;

#[cfg(windows)]
impl SystemProbe for Win32Probe {
    fn volume_info(&self, p: &Path) -> AppResult<(String, bool)> {
        crate::win32::volume_info(p)
    }
    fn disk_free(&self, p: &Path) -> AppResult<u64> {
        crate::win32::disk_free_bytes(p)
    }
    fn locked_processes(&self, p: &Path) -> AppResult<Option<Vec<String>>> {
        crate::win32::locked_processes(p)
    }
    fn running_processes(&self) -> AppResult<Vec<String>> {
        crate::process_probe::running_process_names()
    }
}

#[cfg(not(windows))]
impl SystemProbe for Win32Probe {
    fn volume_info(&self, _p: &Path) -> AppResult<(String, bool)> {
        Err(AppError::Win32("仅支持 Windows".into()))
    }
    fn disk_free(&self, _p: &Path) -> AppResult<u64> {
        Err(AppError::Win32("仅支持 Windows".into()))
    }
    fn locked_processes(&self, _p: &Path) -> AppResult<Option<Vec<String>>> {
        Ok(None)
    }
    fn running_processes(&self) -> AppResult<Vec<String>> {
        Ok(Vec::new())
    }
}

/// 系统关键路径黑名单（迁移拒绝）。
const SYSTEM_BLACKLIST: &[&str] = &[
    "C:/Windows",
    "C:/Program Files/WindowsApps",
    "C:/Program Files (x86)",
    "C:/ProgramData/Microsoft",
    "C:/Windows/System32",
    "C:/Recovery",
];

/// 安全余量：源大小的 10% + 100MB（吸收复制期间增长与回收站占用）。
fn safety_margin(src_size: u64) -> u64 {
    src_size / 10 + 100 * 1024 * 1024
}

fn migration_blocks_new_source(status: &MigrationStatus) -> bool {
    !matches!(status, MigrationStatus::TargetPendingDelete)
}

fn is_descendant(path: &str, parent: &str) -> bool {
    let path = norm(path);
    let parent = norm(parent);
    path.strip_prefix(&parent)
        .is_some_and(|rest| rest.starts_with('\\'))
}

/// 返回持久化迁移记录与待迁移路径之间的首个冲突。
pub fn migration_conflict(src: &Path, existing: &[Migration]) -> Option<String> {
    let src = src.to_string_lossy();
    for migration in existing
        .iter()
        .filter(|migration| migration_blocks_new_source(&migration.status))
    {
        if norm(&migration.source) == norm(&src) {
            return Some("源路径已有迁移记录，不能重复迁移".into());
        }
        if is_descendant(&migration.source, &src) {
            return Some(format!(
                "源目录包含已迁移的子目录：{}，请先在软链接管理中处理",
                migration.source
            ));
        }
        if is_descendant(&src, &migration.source) {
            return Some(format!(
                "源路径位于已迁移目录内部：{}，不能单独迁移",
                migration.source
            ));
        }
    }
    None
}

pub fn precheck(
    src: &Path,
    config: &Config,
    existing: &[Migration],
    src_size: u64,
    probe: &dyn SystemProbe,
) -> PrecheckReport {
    let repo = config.repository.trim_end_matches('/');
    let mut warnings = Vec::new();
    let mut blockers = Vec::new();
    let src_str = src.to_string_lossy().replace('/', "\\");

    // 1. 重复或重叠迁移
    if let Some(conflict) = migration_conflict(src, existing) {
        blockers.push(conflict);
    }

    // 2. 系统黑名单（双方统一 norm：去末尾分隔符、统一反斜杠、统一小写）
    if SYSTEM_BLACKLIST
        .iter()
        .any(|b| norm(src_str.as_str()).starts_with(&norm(b)))
    {
        blockers.push("源路径在系统关键目录黑名单内".into());
    }

    // 3. 仓库路径合法性
    if repo.to_lowercase().starts_with("c:") {
        blockers.push("仓库不能位于 C 盘（系统盘）".into());
    }
    if repo.starts_with("\\\\") {
        blockers.push("仓库不能是网络路径".into());
    }
    if norm(src_str.as_str()).starts_with(&norm(repo)) {
        blockers.push("仓库不能位于源目录内部".into());
    }

    // 4. 目标卷能力
    let (_target_serial, is_ntfs) = match probe.volume_info(Path::new(repo)) {
        Ok(v) => v,
        Err(e) => {
            blockers.push(format!("无法读取目标卷信息: {e}"));
            (String::new(), false)
        }
    };
    if !is_ntfs {
        blockers.push("目标卷不是 NTFS（junction 需 NTFS）".into());
    }

    // 5. 空间
    let free = match probe.disk_free(Path::new(repo)) {
        Ok(f) => f,
        Err(e) => {
            blockers.push(format!("无法读取目标盘剩余空间: {e}"));
            0
        }
    };
    let need = src_size.saturating_add(safety_margin(src_size));
    if free < need {
        blockers.push(format!(
            "目标盘空间不足：需 {} 字节（含安全余量），实有 {}",
            need, free
        ));
    }

    // 6. 占用检测
    // (a) Restart Manager（对文件路径有效；目录生产返回 None，不影响）
    match probe.locked_processes(src) {
        Ok(Some(procs)) if !procs.is_empty() => {
            warnings.push(format!(
                "源被进程占用（Restart Manager）：{}",
                procs.join(", ")
            ));
        }
        _ => {}
    }
    // (b) 主检测：预设进程名匹配（针对目录路径真正生效）
    let preset_match: Option<&Preset> = config.presets.iter().find(|p| matches_preset(&src_str, p));
    if let Some(preset) = preset_match {
        if !preset.match_processes.is_empty() {
            match probe.running_processes() {
                Ok(running) => {
                    let running_lower: Vec<String> =
                        running.iter().map(|s| s.to_lowercase()).collect();
                    let hits: Vec<String> = preset
                        .match_processes
                        .iter()
                        .filter(|probe_proc| {
                            let p = probe_proc.to_lowercase();
                            // 精确相等（忽略大小写）。运行中名已小写去.exe，
                            // 预设 match_processes 约定为不带扩展名的小写名，
                            // 用 contains 会误报（如 "wechat" 命中 "wechatdevtools"）。
                            running_lower.iter().any(|r| r.eq_ignore_ascii_case(&p))
                        })
                        .cloned()
                        .collect();
                    if !hits.is_empty() {
                        warnings.push(format!(
                            "源目录被进程占用，请先关闭 {}：{}",
                            preset.name,
                            hits.join(", ")
                        ));
                    }
                }
                Err(e) => {
                    warnings.push(format!("无法枚举运行中进程（跳过占用检测）: {e}"));
                }
            }
        }
    }

    let ok = blockers.is_empty();
    PrecheckReport {
        ok,
        warnings,
        blockers,
        source_size_bytes: src_size,
        target_free_bytes: free,
    }
}

fn norm(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;
    use crate::store::default_config;
    use std::path::Path;

    /// 可编程的 mock probe：返回预设的盘空间/卷信息/占用/运行进程。
    struct Mock {
        free: u64,
        ntfs: bool,
        serial: String,
        locked: Option<Vec<String>>,
        running: Vec<String>,
    }
    impl SystemProbe for Mock {
        fn volume_info(&self, _p: &Path) -> AppResult<(String, bool)> {
            Ok((self.serial.clone(), self.ntfs))
        }
        fn disk_free(&self, _p: &Path) -> AppResult<u64> {
            Ok(self.free)
        }
        fn locked_processes(&self, _p: &Path) -> AppResult<Option<Vec<String>>> {
            Ok(self.locked.clone())
        }
        fn running_processes(&self) -> AppResult<Vec<String>> {
            Ok(self.running.clone())
        }
    }

    fn cfg_repo(repo: &str) -> Config {
        let mut c = default_config();
        c.repository = repo.into();
        c
    }

    fn migration(source: &str, status: MigrationStatus) -> Migration {
        Migration {
            id: "u1".into(),
            schema_version: 1,
            source: source.into(),
            target: "D:/Migrated/c/u1/data".into(),
            old_path: String::new(),
            preset: None,
            created_at: "2026-07-18T00:00:00Z".into(),
            status,
            source_volume_serial: "C".into(),
            target_volume_serial: "D".into(),
            recycle_bin_ref: String::new(),
            pending_cleanup: None,
        }
    }

    #[test]
    fn passes_when_space_and_ntfs_ok() {
        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "DDDD".into(),
            locked: None,
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Users/x/Data"),
            &cfg_repo("D:/Migrated"),
            &[],
            1_000_000_000,
            &probe,
        );
        assert!(report.ok, "blockers: {:?}", report.blockers);
    }

    #[test]
    fn blocks_when_space_insufficient() {
        let probe = Mock {
            free: 100_000,
            ntfs: true,
            serial: "DDDD".into(),
            locked: None,
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Users/x/Data"),
            &cfg_repo("D:/Migrated"),
            &[],
            1_000_000_000,
            &probe,
        );
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("空间")));
    }

    #[test]
    fn blocks_when_target_not_ntfs() {
        let probe = Mock {
            free: 10_000_000_000,
            ntfs: false,
            serial: "DDDD".into(),
            locked: None,
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Users/x/Data"),
            &cfg_repo("D:/Migrated"),
            &[],
            1_000_000_000,
            &probe,
        );
        assert!(!report.ok);
        assert!(report.blockers.iter().any(|b| b.contains("NTFS")));
    }

    #[test]
    fn blocks_when_repo_on_c_drive() {
        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "CCCC".into(),
            locked: None,
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Users/x/Data"),
            &cfg_repo("C:/Migrated"),
            &[],
            1_000_000_000,
            &probe,
        );
        assert!(!report.ok);
        assert!(report
            .blockers
            .iter()
            .any(|b| b.contains("C 盘") || b.contains("系统盘")));
    }

    #[test]
    fn blocks_system_critical_path() {
        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "CCCC".into(),
            locked: None,
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Windows/System32"),
            &cfg_repo("D:/Migrated"),
            &[],
            1_000,
            &probe,
        );
        assert!(!report.ok);
        assert!(report
            .blockers
            .iter()
            .any(|b| b.contains("系统") || b.contains("黑名单")));
    }

    #[test]
    fn warns_when_source_locked() {
        // 文件级占用（Restart Manager 路径）：locked_processes 返回进程列表
        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "CCCC".into(),
            locked: Some(vec!["wechat.exe".into()]),
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Users/x/Data"),
            &cfg_repo("D:/Migrated"),
            &[],
            1_000_000,
            &probe,
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("wechat")),
            "warnings: {:?}",
            report.warnings
        );
    }

    /// 增强（第 6 步）：源目录命中预设 + 预设的 match_processes 进程在运行中 → warning。
    #[test]
    fn warns_when_preset_process_running() {
        // 取 wechat 预设的真实路径模板，构造一个匹配源
        let cfg = default_config();
        let wechat = cfg.presets.iter().find(|p| p.id == "wechat").unwrap();
        let userprofile = std::env::var("USERPROFILE").unwrap();
        let wechat_path = format!("{userprofile}\\Documents\\WeChat Files");
        assert!(
            matches_preset(&wechat_path, wechat),
            "测试前置：路径应匹配 wechat 预设"
        );

        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "CCCC".into(),
            locked: None,
            running: vec!["wechat".into()], // 按契约：小写、去 .exe 后缀
        };
        let mut cfg = cfg;
        cfg.repository = "D:/Migrated".into();
        let report = precheck(Path::new(&wechat_path), &cfg, &[], 1_000_000, &probe);
        assert!(report.ok, "不应有 blocker: {:?}", report.blockers);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("微信文件") && w.contains("wechat")),
            "warnings: {:?}",
            report.warnings
        );
    }

    /// 增强（第 6 步）反例：进程未运行 → 不触发占用 warning。
    #[test]
    fn does_not_warn_when_preset_process_not_running() {
        let cfg = default_config();
        let userprofile = std::env::var("USERPROFILE").unwrap();
        let wechat_path = format!("{userprofile}\\Documents\\WeChat Files");

        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "CCCC".into(),
            locked: None,
            running: vec!["explorer".into(), "chrome".into()],
        };
        let mut cfg = cfg;
        cfg.repository = "D:/Migrated".into();
        let report = precheck(Path::new(&wechat_path), &cfg, &[], 1_000_000, &probe);
        assert!(report.warnings.iter().all(|w| !w.contains("微信文件")));
    }

    #[test]
    fn blocks_duplicate_active_migration() {
        let existing = vec![migration("C:/Users/x/Data", MigrationStatus::Active)];
        let probe = Mock {
            free: 10_000_000_000,
            ntfs: true,
            serial: "C".into(),
            locked: None,
            running: vec![],
        };
        let report = precheck(
            Path::new("C:/Users/x/Data"),
            &cfg_repo("D:/Migrated"),
            &existing,
            1_000,
            &probe,
        );
        assert!(!report.ok);
        assert!(report
            .blockers
            .iter()
            .any(|b| b.contains("已迁移") || b.contains("重复")));
    }

    #[test]
    fn blocks_parent_that_contains_a_migrated_directory() {
        let existing = vec![migration(
            "C:/Users/x/AppData/Local/Cache",
            MigrationStatus::Active,
        )];

        let conflict = migration_conflict(Path::new("C:\\Users\\x\\AppData"), &existing);

        assert!(conflict
            .as_deref()
            .is_some_and(|message| message.contains("包含已迁移的子目录")));
    }

    #[test]
    fn blocks_pending_migration_but_not_restored_cleanup_record() {
        let pending = vec![migration(
            "C:/Users/x/Data",
            MigrationStatus::PendingManualConfirm,
        )];
        let restored = vec![migration(
            "C:/Users/x/Data",
            MigrationStatus::TargetPendingDelete,
        )];

        assert!(migration_conflict(Path::new("C:/Users/x/Data"), &pending).is_some());
        assert!(migration_conflict(Path::new("C:/Users/x/Data"), &restored).is_none());
    }

    /// 真机验证：running_process_names 真能枚举出非空进程列表。
    #[test]
    fn running_process_names_returns_nonempty() {
        let names = crate::process_probe::running_process_names().expect("应能枚举进程");
        assert!(!names.is_empty(), "运行中进程列表不应为空");
        // 当前进程（cargo test）至少应出现，或常见系统进程
        assert!(
            names.iter().any(|n| n == "explorer"
                || n == "svchost"
                || n == "cargo"
                || n == "conhost"
                || n == "rustc"
                || n == "system"),
            "未找到常见进程，列表前若干：{:?}",
            names.iter().take(10).collect::<Vec<_>>()
        );
    }
}
