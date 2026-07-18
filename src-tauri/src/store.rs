use crate::error::{AppError, AppResult};
use crate::models::{Config, Migration, Preset, PresetCategory, ScanConfig};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct Store {
    pub data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: impl Into<PathBuf>) -> AppResult<Self> {
        let data_dir = data_dir.into();
        fs::create_dir_all(&data_dir)?;
        Ok(Store { data_dir })
    }

    pub fn config_path(&self) -> PathBuf { self.data_dir.join("config.json") }
    pub fn config_tmp(&self) -> PathBuf { self.data_dir.join("config.json.tmp") }
    pub fn config_bak(&self) -> PathBuf { self.data_dir.join("config.json.bak") }
    pub fn mig_path(&self) -> PathBuf { self.data_dir.join("migrations.json") }
    pub fn mig_tmp(&self) -> PathBuf { self.data_dir.join("migrations.json.tmp") }
    pub fn mig_bak(&self) -> PathBuf { self.data_dir.join("migrations.json.bak") }

    pub fn load_config(&self) -> AppResult<Config> {
        match fs::read(self.config_path()) {
            Ok(bytes) => match serde_json::from_slice::<Config>(&bytes) {
                Ok(cfg) => Ok(ensure_presets(cfg)),
                Err(_) => self.load_config_bak_or_default(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default_config()),
            Err(e) => Err(AppError::Io(e)),
        }
    }

    fn load_config_bak_or_default(&self) -> AppResult<Config> {
        let bak = self.config_bak();
        if bak.exists() {
            if let Ok(bak_bytes) = fs::read(&bak) {
                if let Ok(cfg) = serde_json::from_slice::<Config>(&bak_bytes) {
                    return Ok(ensure_presets(cfg));
                }
            }
        }
        Ok(default_config())
    }

    pub fn save_config(&self, cfg: &Config) -> AppResult<()> {
        atomic_write_json(&self.config_path(), &self.config_tmp(), &self.config_bak(), cfg)
    }

    pub fn load_migrations(&self) -> AppResult<Vec<Migration>> {
        match fs::read(self.mig_path()) {
            Ok(bytes) => serde_json::from_slice::<Vec<Migration>>(&bytes).map_err(AppError::Json),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(AppError::Io(e)),
        }
    }

    pub fn save_migrations(&self, ms: &[Migration]) -> AppResult<()> {
        atomic_write_json(&self.mig_path(), &self.mig_tmp(), &self.mig_bak(), ms)
    }

    pub fn upsert_migration(&self, m: Migration) -> AppResult<()> {
        let mut ms = self.load_migrations()?;
        if let Some(slot) = ms.iter_mut().find(|x| x.id == m.id) {
            *slot = m;
        } else {
            ms.push(m);
        }
        self.save_migrations(&ms)
    }

    pub fn remove_migration(&self, id: &str) -> AppResult<()> {
        let mut ms = self.load_migrations()?;
        ms.retain(|x| x.id != id);
        self.save_migrations(&ms)
    }
}

/// 临时文件 -> flush/sync -> 备份旧文件为 .bak -> 原子 rename 覆盖。
fn atomic_write_json<T: ?Sized + serde::Serialize>(
    path: &Path,
    tmp: &Path,
    bak: &Path,
    value: &T,
) -> AppResult<()> {
    let json = serde_json::to_vec_pretty(value)?;
    {
        let mut f = fs::File::create(tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    if path.exists() {
        let _ = fs::remove_file(bak);
        let _ = fs::rename(path, bak); // 失败不致命：仍尝试覆盖
    }
    fs::rename(tmp, path)?; // std 在 Windows 用 MoveFileEx(REPLACE_EXISTING)，原子替换
    Ok(())
}

pub fn default_config() -> Config {
    Config {
        schema_version: 1,
        repository: "D:/Migrated".into(),
        scan: ScanConfig {
            min_size_mb: 500,
            exclude_paths: vec!["C:/Windows".into(), "C:/Program Files/WindowsApps".into()],
        },
        presets: default_presets(),
    }
}

/// 旧配置（无 presets 或为空）补齐内置 presets。
fn ensure_presets(mut cfg: Config) -> Config {
    if cfg.presets.is_empty() {
        cfg.presets = default_presets();
    }
    cfg
}

/// 内置预设场景。match_paths 可含 %USERPROFILE%/%LOCALAPPDATA%/%APPDATA% 占位（scanner 展开）。
/// 一键迁移（auto_migrate=true）：当前用户可写数据/缓存目录；
/// 需确认风险（auto_migrate=false）：游戏库、容器等可能涉及服务/ACL。
pub fn default_presets() -> Vec<Preset> {
    macro_rules! p {
        ($id:expr, $name:expr, $cat:expr, $auto:expr, $sub:expr, $paths:expr, $procs:expr) => {
            Preset {
                id: $id.into(), name: $name.into(), category: $cat,
                auto_migrate: $auto, target_subdir: $sub.into(),
                match_paths: $paths, match_processes: $procs,
            }
        };
    }
    vec![
        p!("wechat", "微信文件", PresetCategory::Communication, true, "wechat",
           vec!["%USERPROFILE%/Documents/WeChat Files".into(), "%APPDATA%/Tencent/WeChat".into()],
           vec!["wechat".into()]),
        p!("qq", "QQ 文件", PresetCategory::Communication, true, "qq",
           vec!["%USERPROFILE%/Documents/Tencent Files".into()],
           vec!["qq".into()]),
        p!("dingtalk", "钉钉", PresetCategory::Communication, true, "dingtalk",
           vec!["%APPDATA%/DingTalk".into()],
           vec!["dingtalk".into()]),
        p!("wxwork", "企业微信", PresetCategory::Communication, true, "wxwork",
           vec!["%USERPROFILE%/Documents/WXWork".into()],
           vec!["wxwork".into()]),
        p!("npm-cache", "npm 缓存", PresetCategory::DevCache, true, "npm-cache",
           vec!["%LOCALAPPDATA%/npm-cache".into(), "%APPDATA%/npm-cache".into()],
           vec![]),
        p!("maven", "Maven 仓库", PresetCategory::DevCache, true, "maven",
           vec!["%USERPROFILE%/.m2/repository".into()],
           vec![]),
        p!("gradle", "Gradle 缓存", PresetCategory::DevCache, true, "gradle",
           vec!["%USERPROFILE%/.gradle".into()],
           vec![]),
        p!("pip-cache", "pip 缓存", PresetCategory::DevCache, true, "pip-cache",
           vec!["%LOCALAPPDATA%/pip/Cache".into()],
           vec![]),
        p!("jetbrains", "JetBrains 配置", PresetCategory::Ide, true, "jetbrains",
           vec!["%APPDATA%/JetBrains".into()],
           vec![]),
        p!("vscode", "VS Code 用户数据", PresetCategory::Ide, true, "vscode",
           vec!["%APPDATA%/Code".into(), "%USERPROFILE%/.vscode".into()],
           vec!["code".into()]),
        // 需确认风险场景
        p!("steam", "Steam 游戏库", PresetCategory::GameLibrary, false, "steam",
           vec!["steamapps".into()],
           vec!["steam".into()]),
        p!("docker", "Docker 数据", PresetCategory::Container, false, "docker",
           vec!["%LOCALAPPDATA%/Docker".into()],
           vec!["dockerd".into()]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::MigrationStatus;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, Store) {
        let dir = TempDir::new().unwrap();
        let s = Store::new(dir.path()).unwrap();
        (dir, s)
    }

    #[test]
    fn load_config_returns_default_when_missing() {
        let (_t, s) = fresh_store();
        let cfg = s.load_config().unwrap();
        assert_eq!(cfg.schema_version, 1);
        assert_eq!(cfg.scan.min_size_mb, 500);
        assert!(!cfg.presets.is_empty(), "默认 presets 必须被注入");
        assert!(cfg.presets.iter().any(|p| p.id == "wechat"));
    }

    #[test]
    fn save_then_load_config_roundtrip() {
        let (_t, s) = fresh_store();
        let mut cfg = s.load_config().unwrap();
        cfg.repository = "E:/Migrated2".into();
        s.save_config(&cfg).unwrap();
        let again = s.load_config().unwrap();
        assert_eq!(again.repository, "E:/Migrated2");
    }

    #[test]
    fn corrupt_config_falls_back_to_default() {
        let (_t, s) = fresh_store();
        fs::write(s.config_path(), b"{ not valid json").unwrap();
        let cfg = s.load_config().unwrap();
        assert_eq!(cfg.repository, "D:/Migrated");
    }

    #[test]
    fn save_migrations_creates_bak_on_second_write() {
        let (_t, s) = fresh_store();
        let sample = Migration {
            id: "u1".into(),
            schema_version: 1,
            source: "C:/src".into(),
            target: "D:/dst".into(),
            old_path: "C:/src.dayu-old-t1".into(),
            preset: None,
            created_at: "2026-07-18T10:00:00Z".into(),
            status: MigrationStatus::Active,
            source_volume_serial: "AAA".into(),
            target_volume_serial: "BBB".into(),
            recycle_bin_ref: String::new(),
            pending_cleanup: None,
        };
        s.upsert_migration(sample.clone()).unwrap();
        assert!(s.mig_path().exists());
        assert!(!s.mig_bak().exists(), "首次写入不应有 bak");
        s.upsert_migration(Migration { id: "u2".into(), ..sample }).unwrap();
        assert!(s.mig_bak().exists(), "第二次写入应生成 bak");
        let loaded = s.load_migrations().unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn load_migrations_empty_when_missing() {
        let (_t, s) = fresh_store();
        assert!(s.load_migrations().unwrap().is_empty());
    }
}
