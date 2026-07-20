use crate::models::{Config, Migration, MigrationStatus, Preset, ScanItem, ScanItemStatus};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::mft::{select_effective_names, FileRef, MftIndex};

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

fn is_descendant(path: &str, parent: &str) -> bool {
    let path = normalize(path);
    let parent = normalize(parent);
    path.strip_prefix(&parent)
        .is_some_and(|rest| rest.starts_with('\\'))
}

fn represents_migrated_link(status: &MigrationStatus) -> bool {
    !matches!(status, MigrationStatus::TargetPendingDelete)
}

/// 合并迁移记录，使所有扫描调用方得到一致的状态和迁移资格判断。
pub fn annotate_migrations(
    items: &mut [ScanItem],
    migrations: &[Migration],
    link_valid: &dyn Fn(&Path) -> bool,
    target_size: &dyn Fn(&Path) -> u64,
) {
    let linked_migrations: Vec<&Migration> = migrations
        .iter()
        .filter(|migration| represents_migrated_link(&migration.status))
        .collect();
    let junction_paths: Vec<String> = items
        .iter()
        .filter(|item| item.is_junction)
        .map(|item| item.path.clone())
        .collect();

    for item in items {
        if let Some(migration) = linked_migrations
            .iter()
            .find(|migration| normalize(&migration.source) == normalize(&item.path))
        {
            item.migration_id = Some(migration.id.clone());
            let target = Path::new(&migration.target);
            if target.exists() {
                item.size_bytes = target_size(target);
            }
            item.scan_status = Some(if !link_valid(Path::new(&migration.source)) {
                ScanItemStatus::LinkBroken
            } else if migration.status == MigrationStatus::Active {
                ScanItemStatus::Migrated
            } else {
                ScanItemStatus::MigrationPending
            });
            continue;
        }

        if linked_migrations
            .iter()
            .any(|migration| is_descendant(&migration.source, &item.path))
        {
            item.scan_status = Some(ScanItemStatus::ContainsMigrated);
        } else if junction_paths
            .iter()
            .any(|junction_path| is_descendant(junction_path, &item.path))
        {
            item.scan_status = Some(ScanItemStatus::ContainsLink);
        }
    }
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
        is_path_excluded(normalized_path, &self.exclude)
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
        scan_status: None,
        migration_id: None,
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
        scan_status: Some(ScanItemStatus::ExistingLink),
        migration_id: None,
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
        scan_status: None,
        migration_id: None,
    }
}

// ===== T4: DirectoryGraph 线性构建（MFT 树形扫描的聚合层） =====
//
// 任务 4 的职责：把 T2 输出的扁平 `MftIndex` 聚合成内部 `DirectoryGraph`：
//   - 两遍建图：先建节点、再连父子（不依赖 HashMap 随机顺序）
//   - 序列号验证：父引用的 sequence 必须与 `records` 中的实际 sequence 匹配
//   - 显式栈路径构建 + 三色循环检测（无递归，防深链栈溢出）
//   - 排除子树标记（在路径构建之后、后序聚合之前）
//   - 后序 O(V+E) 聚合（saturating arithmetic）
//   - Diagnostics：缺父、陈旧 sequence、重复入口、循环、不可达、orphan
//
// T4 不涉及：预设匹配、迁移状态、阈值可见性、TreeStore 物化、分页、filesystem 降级。

/// NTFS 根目录记录号（`$Root`）。
const ROOT_RECORD_NO: u64 = 5;

/// 归一化后的排除路径列表，判定某归一化路径是否命中排除规则。
///
/// 复用 [`ScanContext::excluded`] 的语义：路径与任一排除项完全相等，或
/// `strip_prefix + starts_with('\\')`（必须越过路径边界，避免 `cache` 误匹配
/// `cache-backup`）。
pub(crate) fn is_path_excluded(normalized_path: &str, exclude: &[String]) -> bool {
    exclude.iter().any(|excluded| {
        normalized_path == excluded
            || normalized_path
                .strip_prefix(excluded)
                .is_some_and(|rest| rest.starts_with('\\'))
    })
}

/// 单条目录节点（含直接/聚合统计）。
///
/// `path` / `display_name` / `depth` 在路径构建阶段填充；`subtree_*` 在后序
/// 聚合阶段填充；`excluded` / `reachable_from_root` 是树形拓扑标志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryNode {
    /// 完整 `FileRef`（record_no + sequence）。
    pub file_ref: FileRef,
    /// 完整绝对路径。根 = `r"C:\"`，未达节点为空串。
    pub path: String,
    /// 末级显示名。根 = 盘符字符串。
    pub display_name: String,
    /// 距根的层数。根 = 0。
    pub depth: u32,
    /// 是否携带 `$REPARSE_POINT` 属性。
    pub is_reparse: bool,
    /// `$REPARSE_POINT` tag（仅当 `is_reparse == true`）。
    pub reparse_tag: Option<u32>,
    /// 是否为本工具创建的 junction。T4 留 `false`，由 T5/T8 借 `junction::verify` 填充。
    pub is_junction: bool,
    /// 本目录直接子文件的 logical_size 之和（不含子目录内文件）。
    pub direct_file_size_bytes: u64,
    /// 本目录直接子文件数（按文件 record 计数，硬链接近似下每个有效名入口计一次）。
    pub direct_file_count: u64,
    /// 本目录直接子目录数（不在聚合前计入 `subtree_dir_count`）。
    pub direct_dir_count: u32,
    /// 后序聚合：本节点直接 + 所有未排除后代的 total。`saturating_add`。
    pub subtree_size_bytes: u64,
    /// 后序聚合：本节点直接 + 所有未排除后代文件总数。
    pub subtree_file_count: u64,
    /// 后序聚合：本节点自身 + 所有未排除后代目录总数（含自身）。
    pub subtree_dir_count: u64,
    /// 命中排除规则。
    pub excluded: bool,
    /// 从根 5 可达（经过循环检测后）。
    pub reachable_from_root: bool,
}

/// 构建阶段诊断计数。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphDiagnostics {
    /// 目录父引用 sequence 与 records 中的实际 sequence 不匹配。
    pub stale_sequence_dirs: u64,
    /// 文件父引用 sequence 不匹配。
    pub stale_sequence_files: u64,
    /// 父 record_no 不在 `records` 中。
    pub missing_parent: u64,
    /// 目录的有效名指向第二个不同的父（重复目录入口）。
    pub duplicate_dir_entry: u64,
    /// 三色 DFS 检测到的循环节点数。
    pub cycle_nodes: u64,
    /// 建图后未从根可达的目录节点数。
    pub unreachable_nodes: u64,
    /// 等价于 unreachable_nodes（规格 4.5 orphan 语义：不伪挂到根下）。
    pub orphan_entries: u64,
    /// 根记录 5 不在 `records` 中，或不是目录。
    pub root_missing: bool,
}

/// 线性构建的目录图。
///
/// `nodes` 含全部目录节点（含不可达，便于诊断），`children` 仅含从根可达、
/// 未循环、未排除的正常边。`system_metadata_size_bytes` 是记录 0..15 除根 5
/// 之外的 logical_size 汇总。
#[derive(Debug, Clone)]
pub struct DirectoryGraph {
    pub nodes: HashMap<FileRef, DirectoryNode>,
    pub root: FileRef,
    pub children: HashMap<FileRef, Vec<FileRef>>,
    pub system_metadata_size_bytes: u64,
    pub diagnostics: GraphDiagnostics,
    /// 被排除节点的 `subtree_size_bytes` 之和（诊断用）。
    pub excluded_subtree_size_bytes: u64,
}

/// 从 `MftIndex` 线性构建 `DirectoryGraph`。
///
/// - `excluded_paths`：归一化前的原始排除路径列表（与 `Config.scan.exclude_paths`
///   同语义，会在内部用 [`expand_env`] + [`normalize`] 归一化）。
/// - `root_drive`：根 5 的盘符字母（如 `'C'`）。根路径格式化为 `r"{drive}:\"`。
///
/// 调用方负责传 `&[String]`（T4 不读 `Config`）。
pub fn build_graph(index: &MftIndex, excluded_paths: &[String], root_drive: char) -> DirectoryGraph {
    // 预归一化排除路径（与 ScanContext 同样的处理）。
    let normalized_excluded: Vec<String> = excluded_paths
        .iter()
        .filter_map(|p| {
            let normalized = normalize(&expand_env(p));
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect();

    let mut diagnostics = GraphDiagnostics::default();
    let mut nodes: HashMap<FileRef, DirectoryNode> = HashMap::new();
    let mut children: HashMap<FileRef, Vec<(FileRef, String)>> = HashMap::new();
    let mut assigned_parent: HashMap<FileRef, FileRef> = HashMap::new();
    let mut system_metadata_size_bytes: u64 = 0;

    // ===== Phase 0：系统元文件 logical_size 汇总 =====
    for record in index.records.values() {
        let rn = record.id.record_no;
        if rn < 16 && rn != ROOT_RECORD_NO {
            system_metadata_size_bytes =
                system_metadata_size_bytes.saturating_add(record.logical_size);
        }
    }

    // ===== Phase 1a：建所有目录节点（不连父子） =====
    for record in index.records.values() {
        let rn = record.id.record_no;
        if rn < 16 && rn != ROOT_RECORD_NO {
            // 系统元记录不建目录节点、不挂到根。
            continue;
        }
        if record.is_dir && record.in_use {
            nodes.insert(
                record.id,
                DirectoryNode {
                    file_ref: record.id,
                    path: String::new(),
                    display_name: String::new(),
                    depth: 0,
                    is_reparse: record.reparse_tag.is_some(),
                    reparse_tag: record.reparse_tag,
                    is_junction: false, // T5/T8 才用 junction::verify 填
                    direct_file_size_bytes: 0,
                    direct_file_count: 0,
                    direct_dir_count: 0,
                    subtree_size_bytes: 0,
                    subtree_file_count: 0,
                    subtree_dir_count: 0,
                    excluded: false,
                    reachable_from_root: false,
                },
            );
        }
    }

    // ===== Phase 1b：把文件的 logical_size 聚合到父目录的 direct_file_* =====
    for record in index.records.values() {
        let rn = record.id.record_no;
        if rn < 16 && rn != ROOT_RECORD_NO {
            continue;
        }
        if record.is_dir || !record.in_use {
            continue;
        }
        let effective_names = select_effective_names(&record.names);
        for name in &effective_names {
            let parent_ref = name.parent;
            if nodes.contains_key(&parent_ref) {
                if let Some(parent_node) = nodes.get_mut(&parent_ref) {
                    parent_node.direct_file_size_bytes = parent_node
                        .direct_file_size_bytes
                        .saturating_add(record.logical_size);
                    parent_node.direct_file_count =
                        parent_node.direct_file_count.saturating_add(1);
                }
            } else {
                // 父不在 nodes：区分"记录不存在"与"sequence 不匹配"。
                match index.records.get(&parent_ref.record_no) {
                    None => diagnostics.missing_parent += 1,
                    Some(r) if r.id.sequence != parent_ref.sequence => {
                        diagnostics.stale_sequence_files += 1;
                    }
                    Some(_) => {
                        // 记录存在且 sequence 匹配，但不在 nodes（系统记录或非目录）。
                        // 不计入诊断——视为超出当前树的范围。
                    }
                }
            }
        }
    }

    // ===== Phase 2：连父子边（第一个 parent 建主边，后续重复入口计数） =====
    for record in index.records.values() {
        let rn = record.id.record_no;
        if rn < 16 && rn != ROOT_RECORD_NO {
            continue;
        }
        if !record.is_dir || !record.in_use {
            continue;
        }
        if rn == ROOT_RECORD_NO {
            // 根 5 自引用不形成循环，不在此阶段处理。
            continue;
        }
        let effective_names = select_effective_names(&record.names);
        for name in &effective_names {
            let parent_ref = name.parent;

            // 防御：非根目录的"自引用"（不应出现在合法 MFT 中）→ 当作循环。
            if parent_ref == record.id {
                diagnostics.cycle_nodes += 1;
                continue;
            }

            // 验证父引用：父在 nodes 中意味着 sequence 匹配且父是合法目录。
            if !nodes.contains_key(&parent_ref) {
                match index.records.get(&parent_ref.record_no) {
                    None => diagnostics.missing_parent += 1,
                    Some(r) if r.id.sequence != parent_ref.sequence => {
                        diagnostics.stale_sequence_dirs += 1;
                    }
                    Some(_) => {
                        // 父存在但不在 nodes（系统记录或非目录）：静默跳过。
                    }
                }
                continue;
            }

            // 第一个 parent 建主边；后续同名/异名入口但 parent 不同的 → duplicate。
            if let Some(existing) = assigned_parent.get(&record.id) {
                if *existing != parent_ref {
                    diagnostics.duplicate_dir_entry += 1;
                }
                continue;
            }

            assigned_parent.insert(record.id, parent_ref);
            children
                .entry(parent_ref)
                .or_default()
                .push((record.id, name.name.clone()));
        }
    }

    // ===== 根 5 检测 =====
    let root_file_ref = match index.records.get(&ROOT_RECORD_NO) {
        Some(rec) if rec.is_dir => rec.id,
        _ => {
            diagnostics.root_missing = true;
            FileRef {
                record_no: ROOT_RECORD_NO,
                sequence: 0,
            }
        }
    };

    // ===== Phase 3：显式栈路径构建 + 三色循环检测 =====
    if !diagnostics.root_missing {
        if let Some(root_node) = nodes.get_mut(&root_file_ref) {
            root_node.path = format!("{}:\\", root_drive);
            root_node.display_name = root_drive.to_string();
            root_node.depth = 0;
            root_node.reachable_from_root = true;
        }

        // color: 0=白, 1=灰, 2=黑
        let mut color: HashMap<FileRef, u8> = HashMap::new();
        let mut stack: Vec<(FileRef, bool)> = Vec::new();
        color.insert(root_file_ref, 1);
        stack.push((root_file_ref, false));

        while let Some((node, processing)) = stack.pop() {
            if processing {
                color.insert(node, 2);
                continue;
            }
            // 第一次访问：标灰、压栈（处理完再变黑）
            stack.push((node, true));

            if let Some(node_children) = children.get(&node) {
                // 反向遍历以保持子节点顺序。
                for (child_ref, child_name) in node_children.iter().rev() {
                    match color.get(child_ref).copied() {
                        Some(1) => {
                            // 灰 = 循环
                            diagnostics.cycle_nodes += 1;
                            continue;
                        }
                        Some(2) => {
                            // 黑 = 已完成（first-parent-wins 下不应发生）
                            continue;
                        }
                        _ => {} // 白
                    }
                    color.insert(*child_ref, 1);
                    let parent_path = nodes
                        .get(&node)
                        .map(|n| n.path.clone())
                        .unwrap_or_default();
                    let parent_depth = nodes.get(&node).map(|n| n.depth).unwrap_or(0);
                    // 父路径若以 '\\' 结尾（如根 "C:\\"），不要再补分隔符。
                    let mut new_path =
                        String::with_capacity(parent_path.len() + child_name.len() + 1);
                    new_path.push_str(&parent_path);
                    if !parent_path.ends_with('\\') {
                        new_path.push('\\');
                    }
                    new_path.push_str(child_name);
                    if let Some(child_node) = nodes.get_mut(child_ref) {
                        child_node.path = new_path;
                        child_node.display_name = child_name.clone();
                        child_node.depth = parent_depth + 1;
                        child_node.reachable_from_root = true;
                    }
                    stack.push((*child_ref, false));
                }
            }
        }
    }

    // 物化 children 索引：去掉名字，保留 FileRef。
    let mut children_index: HashMap<FileRef, Vec<FileRef>> = HashMap::new();
    for (parent, kids) in &children {
        children_index.insert(*parent, kids.iter().map(|(r, _)| *r).collect());
    }

    // direct_dir_count = 直接子目录数（按 children 索引）。
    for (parent, kids) in &children_index {
        if let Some(parent_node) = nodes.get_mut(parent) {
            parent_node.direct_dir_count = kids.len() as u32;
        }
    }

    // 不可达节点统计：建图后所有 nodes 中 reachable_from_root == false 的目录节点。
    let reachable_count = nodes.values().filter(|n| n.reachable_from_root).count() as u64;
    diagnostics.unreachable_nodes = (nodes.len() as u64).saturating_sub(reachable_count);
    diagnostics.orphan_entries = diagnostics.unreachable_nodes;

    // ===== Phase 4：排除子树标记（在路径构建之后、后序聚合之前） =====
    for node in nodes.values_mut() {
        if node.path.is_empty() {
            continue;
        }
        let normalized = normalize(&node.path);
        if is_path_excluded(&normalized, &normalized_excluded) {
            node.excluded = true;
        }
    }

    // ===== Phase 5：后序 O(V+E) 聚合 =====
    // 用两阶段显式栈做后序：先压 (n, false)，访问时改压 (n, true)；弹 true 时为后序。
    let mut post_order: Vec<FileRef> = Vec::new();
    if !diagnostics.root_missing && nodes.contains_key(&root_file_ref) {
        let mut stack: Vec<(FileRef, bool)> = vec![(root_file_ref, false)];
        let mut visited: HashSet<FileRef> = HashSet::new();
        visited.insert(root_file_ref);
        while let Some((node, processing)) = stack.pop() {
            if processing {
                post_order.push(node);
                continue;
            }
            stack.push((node, true));
            if let Some(kids) = children_index.get(&node) {
                for kid in kids.iter().rev() {
                    if visited.insert(*kid) {
                        stack.push((*kid, false));
                    }
                }
            }
        }
    }

    // post_order：叶子在前，根在最后。
    // 聚合规则：非排除节点 = 直接 + Σ 非排除子节点.subtree；排除节点 = 直接（不下传）。
    // subtree_dir_count 含自身（=1+Σ）。
    let mut aggregate: HashMap<FileRef, (u64, u64, u64)> = HashMap::new();
    let mut excluded_subtree_size_bytes: u64 = 0;

    for node_ref in &post_order {
        let node = nodes
            .get(node_ref)
            .expect("post_order 仅含已入 nodes 的节点");
        let mut size = node.direct_file_size_bytes;
        let mut file_count = node.direct_file_count;
        let mut dir_count: u64 = 1; // 包含自身

        if !node.excluded {
            if let Some(kids) = children_index.get(node_ref) {
                for kid in kids {
                    if let Some(kid_node) = nodes.get(kid) {
                        if kid_node.excluded {
                            // 排除子节点不计入父的聚合。
                            continue;
                        }
                        let (ks, kfc, kdc) =
                            aggregate.get(kid).copied().unwrap_or((0, 0, 0));
                        size = size.saturating_add(ks);
                        file_count = file_count.saturating_add(kfc);
                        dir_count = dir_count.saturating_add(kdc);
                    }
                }
            }
        }

        aggregate.insert(*node_ref, (size, file_count, dir_count));
        if node.excluded {
            excluded_subtree_size_bytes =
                excluded_subtree_size_bytes.saturating_add(size);
        }
    }

    for (node_ref, (size, file_count, dir_count)) in &aggregate {
        if let Some(node) = nodes.get_mut(node_ref) {
            node.subtree_size_bytes = *size;
            node.subtree_file_count = *file_count;
            node.subtree_dir_count = *dir_count;
        }
    }

    DirectoryGraph {
        nodes,
        root: root_file_ref,
        children: children_index,
        system_metadata_size_bytes,
        diagnostics,
        excluded_subtree_size_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::default_config;
    use tempfile::TempDir;

    fn scan_item(path: &str, is_junction: bool) -> ScanItem {
        ScanItem {
            path: path.into(), display_name: path.into(), size_bytes: 0,
            matched_preset: None, category: None, auto_migrate: false,
            is_junction, inaccessible: false,
            scan_status: is_junction.then_some(ScanItemStatus::ExistingLink),
            migration_id: None,
        }
    }

    fn migration(source: &str, target: &Path, status: MigrationStatus) -> Migration {
        Migration {
            id: "migration-1".into(), schema_version: 1, source: source.into(),
            target: target.to_string_lossy().into(), old_path: String::new(), preset: None,
            created_at: "2026-07-19T00:00:00Z".into(), status,
            source_volume_serial: "C".into(), target_volume_serial: "D".into(),
            recycle_bin_ref: String::new(), pending_cleanup: None,
        }
    }

    #[test]
    fn migration_annotations_mark_exact_link_and_parent() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let mut items = vec![
            scan_item("C:\\Users\\x", false),
            scan_item("C:\\Users\\x\\Cache", true),
        ];
        let migrations = vec![migration("c:/users/X/cache/", &target, MigrationStatus::Active)];

        annotate_migrations(&mut items, &migrations, &|_| true, &|_| 4096);

        assert_eq!(items[0].scan_status, Some(ScanItemStatus::ContainsMigrated));
        assert_eq!(items[1].scan_status, Some(ScanItemStatus::Migrated));
        assert_eq!(items[1].migration_id.as_deref(), Some("migration-1"));
        assert_eq!(items[1].size_bytes, 4096);
    }

    #[test]
    fn migration_annotations_surface_broken_links() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let mut items = vec![scan_item("C:\\Users\\x\\Cache", false)];
        let migrations = vec![migration("C:\\Users\\x\\Cache", &target, MigrationStatus::Active)];

        annotate_migrations(&mut items, &migrations, &|_| false, &|_| 0);

        assert_eq!(items[0].scan_status, Some(ScanItemStatus::LinkBroken));
        assert_eq!(items[0].migration_id.as_deref(), Some("migration-1"));
    }

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

    // ===== T4：DirectoryGraph 构建测试 =====
    mod graph {
        use super::*;
        use crate::mft::{MftName, MftRecord};
        use std::collections::HashMap;

        const NAMESPACE_WIN32: u8 = 1;

        fn fileref(no: u64, seq: u16) -> FileRef {
            FileRef {
                record_no: no,
                sequence: seq,
            }
        }

        fn mk_name(parent_rec: u64, parent_seq: u16, name: &str) -> MftName {
            MftName {
                parent: fileref(parent_rec, parent_seq),
                name: name.to_string(),
                namespace: NAMESPACE_WIN32,
            }
        }

        fn mk_dir(record_no: u64, sequence: u16, parent: FileRef, name: &str) -> MftRecord {
            MftRecord {
                id: fileref(record_no, sequence),
                base_record: None,
                names: vec![mk_name(parent.record_no, parent.sequence, name)],
                logical_size: 0,
                is_dir: true,
                in_use: true,
                reparse_tag: None,
                has_nonresident_attr_list: false,
            }
        }

        fn mk_file(
            record_no: u64,
            sequence: u16,
            parent: FileRef,
            name: &str,
            logical_size: u64,
        ) -> MftRecord {
            MftRecord {
                id: fileref(record_no, sequence),
                base_record: None,
                names: vec![mk_name(parent.record_no, parent.sequence, name)],
                logical_size,
                is_dir: false,
                in_use: true,
                reparse_tag: None,
                has_nonresident_attr_list: false,
            }
        }

        fn mk_index(records: Vec<MftRecord>) -> MftIndex {
            let mut map = HashMap::new();
            for r in records {
                map.insert(r.id.record_no, r);
            }
            MftIndex {
                records: map,
                scanned_records: 0,
                skipped_records: 0,
                scanned_files: 0,
                hard_link_entries: 0,
            }
        }

        fn root_record() -> MftRecord {
            // 根 5 自引用。
            MftRecord {
                id: fileref(5, 5),
                base_record: None,
                names: vec![mk_name(5, 5, ".")],
                logical_size: 0,
                is_dir: true,
                in_use: true,
                reparse_tag: None,
                has_nonresident_attr_list: false,
            }
        }

        // ===== 场景 1：输入顺序无关 =====

        #[test]
        fn graph_construction_is_order_independent() {
            let mk = |order: &[u64]| -> MftIndex {
                let mut records = Vec::new();
                for &rn in order {
                    let r = match rn {
                        5 => root_record(),
                        // 用户记录从 16+ 开始（0..15 是 NTFS 系统元记录）
                        20 => mk_dir(20, 1, fileref(5, 5), "Users"),
                        30 => mk_dir(30, 1, fileref(20, 1), "alice"),
                        31 => mk_dir(31, 1, fileref(30, 1), "docs"),
                        40 => mk_file(40, 1, fileref(30, 1), "a.txt", 500),
                        41 => mk_file(41, 1, fileref(31, 1), "b.txt", 300),
                        50 => mk_file(50, 1, fileref(5, 5), "root.txt", 100),
                        _ => unreachable!(),
                    };
                    records.push(r);
                }
                mk_index(records)
            };

            let parent_first = mk(&[5, 20, 30, 31, 40, 41, 50]);
            let child_first = mk(&[50, 41, 40, 31, 30, 20, 5]);
            let random = mk(&[30, 5, 41, 20, 50, 31, 40]);

            let g1 = build_graph(&parent_first, &[], 'C');
            let g2 = build_graph(&child_first, &[], 'C');
            let g3 = build_graph(&random, &[], 'C');

            // 节点集合大小一致
            assert_eq!(g1.nodes.len(), g2.nodes.len());
            assert_eq!(g1.nodes.len(), g3.nodes.len());
            assert_eq!(g1.nodes.len(), 4); // 5/20/30/31

            // 关键节点的 path/depth/统计全部一致
            for key in [
                fileref(20, 1),
                fileref(30, 1),
                fileref(31, 1),
                fileref(5, 5),
            ] {
                assert_eq!(
                    g1.nodes[&key].path,
                    g2.nodes[&key].path,
                    "path mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].path,
                    g3.nodes[&key].path,
                    "path mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].depth,
                    g2.nodes[&key].depth,
                    "depth mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].subtree_size_bytes,
                    g2.nodes[&key].subtree_size_bytes,
                    "subtree_size mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].subtree_file_count,
                    g2.nodes[&key].subtree_file_count,
                    "subtree_file_count mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].subtree_dir_count,
                    g2.nodes[&key].subtree_dir_count,
                    "subtree_dir_count mismatch for {:?}",
                    key
                );
            }

            // 根直接文件 100；dir 30 含直接 a.txt=500；dir 31 含 b.txt=300
            assert_eq!(g1.nodes[&fileref(5, 5)].direct_file_size_bytes, 100);
            assert_eq!(g1.nodes[&fileref(5, 5)].direct_file_count, 1);
            assert_eq!(g1.nodes[&fileref(30, 1)].direct_file_size_bytes, 500);
            assert_eq!(g1.nodes[&fileref(31, 1)].direct_file_size_bytes, 300);

            // dir 31 subtree = 300
            assert_eq!(g1.nodes[&fileref(31, 1)].subtree_size_bytes, 300);
            assert_eq!(g1.nodes[&fileref(31, 1)].subtree_file_count, 1);
            // dir 30 subtree = 500 (a.txt) + 300 (31 subtree)
            assert_eq!(g1.nodes[&fileref(30, 1)].subtree_size_bytes, 800);
            assert_eq!(g1.nodes[&fileref(30, 1)].subtree_file_count, 2);
            assert_eq!(g1.nodes[&fileref(30, 1)].subtree_dir_count, 2); // 30+31
            // dir 20 subtree = 800
            assert_eq!(g1.nodes[&fileref(20, 1)].subtree_size_bytes, 800);
            // 根 subtree = 100 (root.txt) + 800 (20 subtree)
            assert_eq!(g1.nodes[&fileref(5, 5)].subtree_size_bytes, 900);
            assert_eq!(g1.nodes[&fileref(5, 5)].subtree_file_count, 3);
            assert_eq!(g1.nodes[&fileref(5, 5)].subtree_dir_count, 4); // 5+20+30+31
        }

        // ===== 场景 2：陈旧 sequence 不污染 =====

        #[test]
        fn stale_sequence_dirs_do_not_pollute_new_dir() {
            // dir 30 的 parent 引用 record 20 seq=99（陈旧），但 record 20 实际 seq=1。
            // dir 30 应 orphan，不挂到 dir 20。
            let records = vec![
                root_record(),
                mk_dir(20, 1, fileref(5, 5), "Users"),
                MftRecord {
                    id: fileref(30, 1),
                    base_record: None,
                    names: vec![mk_name(20, 99, "ghost")], // stale
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert_eq!(g.diagnostics.stale_sequence_dirs, 1);
            assert_eq!(g.nodes[&fileref(30, 1)].reachable_from_root, false);
            // dir 20 不应有 30 作为子节点
            assert!(g.children.get(&fileref(20, 1)).is_none());
            // dir 20 subtree = 0（无文件、无子目录）
            assert_eq!(g.nodes[&fileref(20, 1)].subtree_size_bytes, 0);
        }

        #[test]
        fn stale_sequence_files_do_not_pollute_new_dir() {
            // 文件 40 parent 引用 record 20 seq=99（陈旧）。文件大小不计入 dir 20。
            let records = vec![
                root_record(),
                mk_dir(20, 1, fileref(5, 5), "Users"),
                MftRecord {
                    id: fileref(40, 1),
                    base_record: None,
                    names: vec![mk_name(20, 99, "stale_file.txt")],
                    logical_size: 999,
                    is_dir: false,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert_eq!(g.diagnostics.stale_sequence_files, 1);
            assert_eq!(g.nodes[&fileref(20, 1)].direct_file_size_bytes, 0);
            assert_eq!(g.nodes[&fileref(20, 1)].direct_file_count, 0);
        }

        // ===== 场景 3：排除多层子树 =====

        #[test]
        fn excluded_subtree_does_not_aggregate_into_ancestors() {
            // 树：5 -> 20(Users) -> 30(alice) -> 31(docs) -> 40(file.txt=1000)
            // 排除 30，整棵子树不计入 20 与 5。
            let records = vec![
                root_record(),
                mk_dir(20, 1, fileref(5, 5), "Users"),
                mk_dir(30, 1, fileref(20, 1), "alice"),
                mk_dir(31, 1, fileref(30, 1), "docs"),
                mk_file(40, 1, fileref(31, 1), "file.txt", 1000),
                mk_file(50, 1, fileref(20, 1), "shared.txt", 200),
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &["C:\\Users\\alice".to_string()], 'C');

            assert!(g.nodes[&fileref(30, 1)].excluded);
            assert!(g.nodes[&fileref(31, 1)].excluded);
            // 排除节点本身的子树大小 = 排除子树的 direct 之和（按聚合规则）
            // 30 direct=0, 31 direct=0, 40 file=1000 → 排除子树 total = 1000
            assert!(g.excluded_subtree_size_bytes >= 1000);
            // 20（祖先）不计入 alice 子树
            assert_eq!(g.nodes[&fileref(20, 1)].subtree_size_bytes, 200);
            // 根不计入 alice 子树
            assert_eq!(g.nodes[&fileref(5, 5)].subtree_size_bytes, 200);
        }

        // ===== 场景 4：深链、环、缺父不 hang =====

        #[test]
        fn deep_chain_does_not_stack_overflow() {
            // 10000 层深链
            const DEPTH: u64 = 10_000;
            let mut records = vec![root_record()];
            let mut prev = fileref(5, 5);
            for i in 0..DEPTH {
                let r = mk_dir(i + 100, 1, prev, &format!("d{i}"));
                prev = r.id;
                records.push(r);
            }
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            // 最深节点可达、有 path、有 depth
            assert_eq!(g.nodes[&fileref(100 + DEPTH - 1, 1)].reachable_from_root, true);
            assert_eq!(g.nodes[&fileref(100 + DEPTH - 1, 1)].depth, DEPTH as u32);
            assert!(g.nodes[&fileref(100 + DEPTH - 1, 1)]
                .path
                .starts_with("C:\\"));
        }

        #[test]
        fn cycle_does_not_hang() {
            // A 的第二个有效名指回自身（自引用）→ Phase 2 计入 cycle_nodes。
            // A 的第一个有效名指根 5，A 仍可达。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(20, 1),
                    base_record: None,
                    names: vec![
                        mk_name(5, 5, "A"),      // 第一个有效名：A 是根的子目录
                        mk_name(20, 1, "self"),  // 第二个有效名：自引用 = cycle
                    ],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            // 不 panic，自引用被检测为 cycle 节点
            assert_eq!(g.diagnostics.cycle_nodes, 1);
            // A 仍可达（第一个有效名指向根），path 以 A 结尾
            assert!(g.nodes[&fileref(20, 1)].reachable_from_root);
            assert!(g.nodes[&fileref(20, 1)].path.ends_with("A"));
        }

        #[test]
        fn cycle_mutual_reference_does_not_hang() {
            // A(parent=B), B(parent=A) — 互引用形成环，但均不可达。
            // 验证 build 不 hang、不 panic，且节点保留以便诊断。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(20, 1),
                    base_record: None,
                    names: vec![mk_name(21, 1, "A")],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
                MftRecord {
                    id: fileref(21, 1),
                    base_record: None,
                    names: vec![mk_name(20, 1, "B")],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            // A 与 B 均不可达
            assert!(!g.nodes[&fileref(20, 1)].reachable_from_root);
            assert!(!g.nodes[&fileref(21, 1)].reachable_from_root);
            // 节点仍在 nodes 中以便诊断
            assert!(g.nodes.contains_key(&fileref(20, 1)));
            assert!(g.nodes.contains_key(&fileref(21, 1)));
            // 不可达 = orphan
            assert!(g.diagnostics.unreachable_nodes >= 2);
        }

        #[test]
        fn missing_parent_does_not_hang_and_counts() {
            // dir 20 引用不存在的 record 99。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(20, 1),
                    base_record: None,
                    names: vec![mk_name(99, 1, "orphan")],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert_eq!(g.diagnostics.missing_parent, 1);
            assert!(!g.nodes[&fileref(20, 1)].reachable_from_root);
            assert!(g.children.get(&fileref(5, 5)).is_none());
        }

        // ===== 场景 5：根直接文件与一级目录子树统计分开 =====

        #[test]
        fn root_direct_files_and_child_subtree_are_separate() {
            // 根直接文件 + 一级目录 + 一级目录下的文件
            let records = vec![
                root_record(),
                mk_file(40, 1, fileref(5, 5), "at_root.txt", 777),
                mk_dir(20, 1, fileref(5, 5), "Users"),
                mk_file(50, 1, fileref(20, 1), "in_users.bin", 333),
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            let root = &g.nodes[&fileref(5, 5)];
            assert_eq!(root.direct_file_size_bytes, 777);
            assert_eq!(root.direct_file_count, 1);
            assert_eq!(root.direct_dir_count, 1); // 仅 Users 一个直接子目录
            // 根 subtree = 777 + dir 20 subtree (333)
            assert_eq!(root.subtree_size_bytes, 777 + 333);
            assert_eq!(root.subtree_file_count, 2);
            assert_eq!(root.subtree_dir_count, 2); // 5 + 20

            let dir20 = &g.nodes[&fileref(20, 1)];
            assert_eq!(dir20.direct_file_size_bytes, 333);
            assert_eq!(dir20.direct_file_count, 1);
            assert_eq!(dir20.direct_dir_count, 0);
            assert_eq!(dir20.subtree_size_bytes, 333);
        }

        // ===== 其他诊断场景 =====

        #[test]
        fn duplicate_dir_entry_counted_when_multiple_parents() {
            // dir 30 有两个有效名：父=20（主边）和父=21（重复入口）
            let records = vec![
                root_record(),
                mk_dir(20, 1, fileref(5, 5), "A"),
                mk_dir(21, 1, fileref(5, 5), "B"),
                MftRecord {
                    id: fileref(30, 1),
                    base_record: None,
                    names: vec![
                        mk_name(20, 1, "shared_in_a"),
                        mk_name(21, 1, "shared_in_b"),
                    ],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert_eq!(g.diagnostics.duplicate_dir_entry, 1);
            // dir 30 只挂到第一个 parent（20）
            assert!(g.children.get(&fileref(20, 1)).is_some());
            assert_eq!(g.children.get(&fileref(20, 1)).unwrap().len(), 1);
            assert!(g.children.get(&fileref(21, 1)).is_none());
        }

        #[test]
        fn system_metadata_size_aggregated_excluding_root() {
            // 系统元记录（0..15 除 5）的 logical_size 计入 system_metadata_size_bytes。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(0, 1),
                    base_record: None,
                    names: vec![],
                    logical_size: 1000, // $MFT
                    is_dir: false,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
                MftRecord {
                    id: fileref(1, 1),
                    base_record: None,
                    names: vec![],
                    logical_size: 2000, // $MFTMirr
                    is_dir: false,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
                // 系统目录 $Extend (record 11) — 即使 is_dir=true 也不入树
                MftRecord {
                    id: fileref(11, 11),
                    base_record: None,
                    names: vec![],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert_eq!(g.system_metadata_size_bytes, 1000 + 2000);
            // 根的 direct_file_size 不应包含 $MFT/$MFTMirr
            assert_eq!(g.nodes[&fileref(5, 5)].direct_file_size_bytes, 0);
        }

        #[test]
        fn root_missing_sets_diagnostic() {
            // 没有 record 5
            let records = vec![mk_dir(10, 1, fileref(5, 5), "Users")];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert!(g.diagnostics.root_missing);
            // graph 应为空
            assert!(g.nodes.is_empty());
        }

        #[test]
        fn is_path_excluded_matches_scancontext_semantics() {
            let exclude = vec![
                "c:\\users\\alice".to_string(),
                "c:\\windows".to_string(),
            ];
            // 完全相等
            assert!(is_path_excluded("c:\\users\\alice", &exclude));
            // 前缀 + 路径边界
            assert!(is_path_excluded(
                "c:\\users\\alice\\docs",
                &exclude
            ));
            // 前缀但不越过路径边界（应不匹配）
            assert!(!is_path_excluded(
                "c:\\users\\alice-backup",
                &exclude
            ));
            // 不匹配
            assert!(!is_path_excluded("c:\\program files", &exclude));
        }
    }
}
