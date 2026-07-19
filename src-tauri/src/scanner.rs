use crate::models::{Config, Preset, ScanItem};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// 递归计算目录体积（字节）。不跟随 reparse point 的内容。
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let meta = match std::fs::symlink_metadata(&cur) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if metadata_is_reparse_point(&meta) && cur != path {
            continue; // 跳过 reparse point 内部
        }
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&cur) {
                for e in entries.flatten() {
                    stack.push(e.path());
                }
            } // AccessDenied 静默跳过
        } else {
            total += meta.len();
        }
    }
    total
}

pub fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|meta| metadata_is_reparse_point(&meta))
        .unwrap_or(false)
}

fn metadata_is_reparse_point(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const RP: u32 = 0x400;
        meta.file_attributes() & RP != 0
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
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

struct ScanContext {
    exclude: Vec<String>,
    preset_by_path: HashMap<String, usize>,
    min_bytes: u64,
}

impl ScanContext {
    fn new(cfg: &Config) -> Self {
        let exclude = cfg
            .scan
            .exclude_paths
            .iter()
            .filter_map(|p| {
                let normalized = normalize(&expand_env(p));
                (!normalized.is_empty()).then_some(normalized)
            })
            .collect();

        // Preset paths are static for the duration of a scan. Expand and normalize
        // them once instead of doing environment lookups for every visited directory.
        let mut preset_by_path = HashMap::new();
        for (index, preset) in cfg.presets.iter().enumerate() {
            for path in &preset.match_paths {
                preset_by_path
                    .entry(normalize(&expand_env(path)))
                    .or_insert(index);
            }
        }

        Self {
            exclude,
            preset_by_path,
            min_bytes: cfg.scan.min_size_mb.saturating_mul(1024 * 1024),
        }
    }

    fn excluded(&self, normalized_path: &str) -> bool {
        self.exclude.iter().any(|excluded| {
            normalized_path == excluded
                || normalized_path
                    .strip_prefix(excluded)
                    .is_some_and(|rest| rest.starts_with('\\'))
        })
    }

    fn preset_index(&self, normalized_path: &str) -> Option<usize> {
        self.preset_by_path.get(normalized_path).copied()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ScanStats {
    pub scanned_dirs: u64,
    pub scanned_files: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanCancelled;

pub struct ScanOutput {
    pub items: Vec<ScanItem>,
    pub stats: ScanStats,
}

/// Scan several roots with one precomputed matcher. The callback is invoked during
/// traversal; callers can throttle expensive IPC/event work at the boundary.
pub fn scan_roots_with_control(
    roots: &[PathBuf],
    cfg: &Config,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(&ScanStats, &Path),
) -> Result<ScanOutput, ScanCancelled> {
    let context = ScanContext::new(cfg);
    let mut output = ScanOutput {
        items: Vec::new(),
        stats: ScanStats::default(),
    };
    for root in roots {
        scan_root(root, cfg, &context, cancel, &mut output, on_progress)?;
    }
    on_progress(
        &output.stats,
        roots.last().map(PathBuf::as_path).unwrap_or(Path::new("")),
    );
    Ok(output)
}

fn scan_root(
    root: &Path,
    cfg: &Config,
    context: &ScanContext,
    cancel: &AtomicBool,
    output: &mut ScanOutput,
    on_progress: &mut dyn FnMut(&ScanStats, &Path),
) -> Result<(), ScanCancelled> {
    let mut stack = vec![(root.to_path_buf(), None::<PathBuf>, false, None::<usize>)];
    let mut sizes = HashMap::<PathBuf, u64>::new();

    while let Some((cur, parent, visited, preset_index)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(ScanCancelled);
        }

        if visited {
            let size = sizes.remove(&cur).unwrap_or(0);
            push_if_big_or_preset(
                &mut output.items,
                &cur,
                size,
                preset_index,
                cfg,
                context.min_bytes,
            );
            if let Some(parent) = parent {
                let parent_size = sizes.entry(parent).or_default();
                *parent_size = parent_size.saturating_add(size);
            }
            continue;
        }

        let normalized = normalize(&cur.to_string_lossy());
        if context.excluded(&normalized) {
            continue;
        }

        let meta = match std::fs::symlink_metadata(&cur) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                output.stats.scanned_dirs += 1;
                output.items.push(inaccessible_item(&cur));
                on_progress(&output.stats, &cur);
                continue;
            }
        };
        if metadata_is_reparse_point(&meta) {
            output.stats.scanned_dirs += 1;
            output.items.push(junction_item(&cur));
            on_progress(&output.stats, &cur);
            continue;
        }

        let entries = match std::fs::read_dir(&cur) {
            Ok(entries) => entries,
            Err(_) => {
                output.stats.scanned_dirs += 1;
                output.items.push(inaccessible_item(&cur));
                on_progress(&output.stats, &cur);
                continue;
            }
        };

        let mut direct_size = 0u64;
        let mut subdirs = Vec::new();
        for entry in entries.flatten() {
            if cancel.load(Ordering::Relaxed) {
                return Err(ScanCancelled);
            }
            let entry_path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(_) => continue,
            };
            if file_type.is_dir() || file_type.is_symlink() {
                subdirs.push(entry_path);
            } else if let Ok(meta) = entry.metadata() {
                output.stats.scanned_files += 1;
                direct_size = direct_size.saturating_add(meta.len());
                if output.stats.scanned_files.is_multiple_of(256) {
                    on_progress(&output.stats, &cur);
                }
            }
        }

        output.stats.scanned_dirs += 1;
        on_progress(&output.stats, &cur);
        sizes.insert(cur.clone(), direct_size);
        stack.push((cur.clone(), parent, true, context.preset_index(&normalized)));
        for subdir in subdirs.into_iter().rev() {
            stack.push((subdir, Some(cur.clone()), false, None));
        }
    }
    Ok(())
}

/// 单次后序遍历 root，返回大于阈值或命中预设的项。
pub fn scan(root: &Path, cfg: &Config) -> Vec<ScanItem> {
    let cancel = AtomicBool::new(false);
    scan_roots_with_control(&[root.to_path_buf()], cfg, &cancel, &mut |_, _| {})
        .map(|output| output.items)
        .unwrap_or_default()
}

fn push_if_big_or_preset(
    items: &mut Vec<ScanItem>,
    path: &Path,
    size: u64,
    preset_index: Option<usize>,
    cfg: &Config,
    min_bytes: u64,
) {
    let path_str = path.to_string_lossy();
    let preset_match = preset_index.and_then(|index| cfg.presets.get(index));
    let big = size >= min_bytes;
    if !(big || preset_match.is_some()) {
        return;
    }
    items.push(ScanItem {
        path: path_str.into(),
        display_name: preset_match
            .map(|p| p.name.clone())
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

fn junction_item(path: &Path) -> ScanItem {
    ScanItem {
        path: path.to_string_lossy().into(),
        display_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_default(),
        size_bytes: 0,
        matched_preset: None,
        category: None,
        auto_migrate: false,
        is_junction: true,
        inaccessible: false,
    }
}

fn inaccessible_item(path: &Path) -> ScanItem {
    ScanItem {
        path: path.to_string_lossy().into(),
        display_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into())
            .unwrap_or_default(),
        size_bytes: 0,
        matched_preset: None,
        category: None,
        auto_migrate: false,
        is_junction: false,
        inaccessible: true,
    }
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
        junction::create(&target, d.join("link")).unwrap();
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
        let parent_item = items
            .iter()
            .find(|i| i.path == parent.to_string_lossy())
            .unwrap();
        let child_item = items
            .iter()
            .find(|i| i.path == child.to_string_lossy())
            .unwrap();

        assert_eq!(parent_item.size_bytes, 1000);
        assert_eq!(child_item.size_bytes, 300);
    }

    #[test]
    fn scan_exclude_requires_a_path_boundary() {
        let root = TempDir::new().unwrap();
        let excluded = root.path().join("cache");
        let sibling = root.path().join("cache-backup");
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(excluded.join("hidden.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(sibling.join("visible.bin"), vec![0u8; 20]).unwrap();

        let mut cfg = default_config();
        cfg.scan.min_size_mb = 0;
        cfg.scan.exclude_paths = vec![excluded.to_string_lossy().into()];
        let items = scan(root.path(), &cfg);

        assert!(!items
            .iter()
            .any(|item| item.path == excluded.to_string_lossy()));
        assert!(items
            .iter()
            .any(|item| item.path == sibling.to_string_lossy()));
    }

    #[test]
    fn controlled_scan_reports_counts() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("nested")).unwrap();
        std::fs::write(root.path().join("a.bin"), vec![0u8; 10]).unwrap();
        std::fs::write(root.path().join("nested/b.bin"), vec![0u8; 20]).unwrap();
        let cancel = AtomicBool::new(false);
        let mut last_stats = ScanStats::default();

        let output = scan_roots_with_control(
            &[root.path().to_path_buf()],
            &default_config(),
            &cancel,
            &mut |stats, _| last_stats = *stats,
        )
        .unwrap();

        assert_eq!(output.stats.scanned_dirs, 2);
        assert_eq!(output.stats.scanned_files, 2);
        assert_eq!(last_stats.scanned_dirs, 2);
        assert_eq!(last_stats.scanned_files, 2);
    }

    #[test]
    fn controlled_scan_honors_pre_cancel() {
        let root = TempDir::new().unwrap();
        let cancel = AtomicBool::new(true);
        let result = scan_roots_with_control(
            &[root.path().to_path_buf()],
            &default_config(),
            &cancel,
            &mut |_, _| {},
        );

        assert!(matches!(result, Err(ScanCancelled)));
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
