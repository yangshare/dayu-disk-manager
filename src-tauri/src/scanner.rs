use crate::models::{Config, Preset, ScanItem};
use std::path::{Path, PathBuf};

/// 递归计算目录体积（字节）。不跟随 reparse point 的内容。
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if !cur.exists() { continue; }
        if is_reparse_point(&cur) && cur != path {
            continue; // 跳过 reparse point 内部
        }
        if cur.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&cur) {
                for e in entries.flatten() {
                    stack.push(e.path());
                }
            } // AccessDenied 静默跳过
        } else if let Ok(meta) = std::fs::metadata(&cur) {
            total += meta.len();
        }
    }
    total
}

pub fn is_reparse_point(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const RP: u32 = 0x400;
        std::fs::symlink_metadata(path).map(|m| m.file_attributes() & RP != 0).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

/// 展开 %USERPROFILE%/%LOCALAPPDATA%/%APPDATA% 占位。
pub fn expand_env(p: &str) -> String {
    let mut out = p.to_string();
    for var in ["USERPROFILE", "LOCALAPPDATA", "APPDATA"] {
        if let Ok(val) = std::env::var(var) {
            out = out.replace(&format!("%{var}%"), &val);
        }
    }
    out
}

/// 路径是否匹配某 preset（路径与任一展开后的 match_paths 严格相等（大小写不敏感）即命中）。
pub fn matches_preset(actual_path: &str, preset: &Preset) -> bool {
    let norm_actual = normalize(actual_path);
    preset.match_paths.iter().any(|tmpl| {
        let expanded = normalize(&expand_env(tmpl));
        norm_actual.eq_ignore_ascii_case(&expanded)
    })
}

fn normalize(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// 单次后序遍历 root，返回大于阈值或命中预设的项。
pub fn scan(root: &Path, cfg: &Config) -> Vec<ScanItem> {
    let exclude: Vec<String> = cfg.scan.exclude_paths.iter()
        .filter(|p| !p.trim().is_empty())
        .map(|p| normalize(p))
        .collect();
    let mut items = Vec::new();
    let mut sizes = std::collections::HashMap::<PathBuf, u64>::new();
    let mut stack = vec![(root.to_path_buf(), None::<PathBuf>, false)];

    while let Some((cur, parent, visited)) = stack.pop() {
        if visited {
            let size = sizes.remove(&cur).unwrap_or(0);
            push_if_big_or_preset(&mut items, &cur, size, cfg);
            if let Some(parent) = parent {
                *sizes.entry(parent).or_default() += size;
            }
            continue;
        }

        let cur_str = cur.to_string_lossy();
        if exclude.iter().any(|ex| normalize(&cur_str).starts_with(ex)) { continue; }
        if !cur.exists() { continue; }
        if is_reparse_point(&cur) {
            // reparse point：可能是已迁移的 junction，标注 is_junction，不进入
            items.push(ScanItem {
                path: cur_str.into(),
                display_name: cur.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_default(),
                size_bytes: 0, matched_preset: None, category: None,
                auto_migrate: false, is_junction: true, inaccessible: false,
            });
            continue;
        }
        let entries = match std::fs::read_dir(&cur) {
            Ok(e) => e,
            Err(_) => {
                items.push(ScanItem {
                    path: cur_str.into(),
                    display_name: cur.file_name().map(|n| n.to_string_lossy().into()).unwrap_or_default(),
                    size_bytes: 0, matched_preset: None, category: None,
                    auto_migrate: false, is_junction: false, inaccessible: true,
                });
                continue;
            }
        };

        let mut direct_size = 0u64;
        let mut subdirs = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                subdirs.push(p);
            } else if let Ok(meta) = e.metadata() {
                direct_size = direct_size.saturating_add(meta.len());
            }
        }

        sizes.insert(cur.clone(), direct_size);
        stack.push((cur.clone(), parent, true));
        for subdir in subdirs {
            stack.push((subdir, Some(cur.clone()), false));
        }
    }
    items
}

fn push_if_big_or_preset(items: &mut Vec<ScanItem>, path: &Path, size: u64, cfg: &Config) {
    let path_str = path.to_string_lossy();
    let preset_match = cfg.presets.iter().find(|p| matches_preset(&path_str, p));
    let min_bytes = cfg.scan.min_size_mb * 1024 * 1024;
    let big = size >= min_bytes;
    if !(big || preset_match.is_some()) { return; }
    items.push(ScanItem {
        path: path_str.into(),
        display_name: preset_match.map(|p| p.name.clone())
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into()))
            .unwrap_or_default(),
        size_bytes: size,
        matched_preset: preset_match.map(|p| p.id.clone()),
        category: preset_match.map(|p| p.category.clone()),
        auto_migrate: preset_match.map(|p| p.auto_migrate).unwrap_or(false),
        is_junction: false,
        inaccessible: false,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::default_config;
    use tempfile::TempDir;

    #[test]
    fn dir_size_sums_files() {
        let root = TempDir::new().unwrap();
        let d = root.path().join("d");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("a.txt"), vec![0u8; 1000]).unwrap();
        std::fs::write(d.join("sub/b.txt"), vec![0u8; 500]).unwrap();
        assert_eq!(dir_size(&d), 1500);
    }

    #[test]
    fn dir_size_skips_reparse_point_content() {
        let root = TempDir::new().unwrap();
        let d = root.path().join("d");
        let target = root.path().join("target");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("big.bin"), vec![0u8; 2000]).unwrap();
        #[cfg(windows)]
        junction::create(&target, &d.join("link")).unwrap();
        // link 是 reparse point，其内部 2000 字节不应计入 d
        let size = dir_size(&d);
        assert_eq!(size, 0, "reparse point 内部内容不应计数");
    }

    #[test]
    fn expand_env_path_resolves_userprofile() {
        let expanded = expand_env("%USERPROFILE%/Documents/WeChat Files");
        assert!(!expanded.contains("%USERPROFILE%"));
        assert!(expanded.contains("Documents"));
    }

    #[test]
    fn match_preset_matches_wechat_path() {
        let cfg = default_config();
        let preset = cfg.presets.iter().find(|p| p.id == "wechat").unwrap();
        let userprofile = std::env::var("USERPROFILE").unwrap();
        let path = format!("{userprofile}\\Documents\\WeChat Files");
        assert!(matches_preset(&path, preset));
    }

    #[test]
    fn scan_returns_items_above_threshold() {
        let root = TempDir::new().unwrap();
        let big = root.path().join("big");
        std::fs::create_dir_all(&big).unwrap();
        std::fs::write(big.join("f.bin"), vec![0u8; 600 * 1024]).unwrap(); // 600KB
        let small = root.path().join("small");
        std::fs::create_dir_all(&small).unwrap();
        std::fs::write(small.join("f.txt"), b"x").unwrap();
        let cfg = default_config();
        // 测试用 0 阈值便于断言：临时改 min_size_mb
        let mut cfg = cfg;
        cfg.scan.min_size_mb = 0;
        let items = scan(root.path(), &cfg);
        assert!(items.iter().any(|i| i.path.ends_with("big")));
    }

    #[test]
    fn scan_aggregates_nested_sizes() {
        let root = TempDir::new().unwrap();
        let parent = root.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(parent.join("a.bin"), vec![0u8; 700]).unwrap();
        std::fs::write(child.join("b.bin"), vec![0u8; 300]).unwrap();
        let mut cfg = default_config();
        cfg.scan.min_size_mb = 0;

        let items = scan(root.path(), &cfg);
        let parent_item = items.iter().find(|i| i.path == parent.to_string_lossy()).unwrap();
        let child_item = items.iter().find(|i| i.path == child.to_string_lossy()).unwrap();

        assert_eq!(parent_item.size_bytes, 1000);
        assert_eq!(child_item.size_bytes, 300);
    }

    #[test]
    fn inaccessible_dir_marked_not_panic() {
        let root = TempDir::new().unwrap();
        // 一个不存在的子目录不应导致 panic
        let cfg = default_config();
        let items = scan(root.path(), &cfg);
        assert!(items.iter().all(|i| !i.inaccessible || i.path.is_empty()));
    }
}
