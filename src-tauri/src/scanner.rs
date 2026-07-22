use crate::models::{
    AccessState, ChildPage, Config, CurrentPhase, FastScanFailure, Migration, MigrationStatus,
    Preset, PresetCategory, RevealLevel, RootFileSummary, ScanDiagnostics, ScanItemStatus,
    ScanMode, ScanProgressEvent, ScanSource, TreeNode,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::junction;
#[cfg(windows)]
use crate::mft::{enumerate_mft, MftFileReader};
use crate::mft::{select_effective_names, FileRef, MftError, MftIndex};
#[cfg(windows)]
use crate::win32::{open_volume, read_volume_data, VolumeError};

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

// ===== T9: 可注入 filesystem 读取器 =====

/// 单条目录/文件条目。
#[derive(Debug, Clone)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    /// 文件才有效，目录为 0。
    pub file_size: u64,
    /// reparse point 的 tag；非 reparse 为 None。
    pub reparse_tag: Option<u32>,
}

/// 读目录时遇到的错误。
#[derive(Debug, Clone)]
pub enum FsEntryError {
    AccessDenied,
    Io { message: String },
}

/// `read_dir` 的返回项：成功条目或单条 entry 错误。
pub type FsEntryResult = Result<FsEntry, (String, FsEntryError)>;

/// 可注入的文件系统读取器，便于测试 AccessDenied/reparse/IO 错误。
pub trait FsReader: Send + Sync {
    /// 读目录条目。
    ///
    /// 外层 `Err` 表示整目录失败（AccessDenied 或其他 IO）；
    /// 内层 `Result` 表示单个条目的成功/失败，用于把普通 entry 错误
    /// 计入诊断而不是静默标 Accessible。
    #[allow(clippy::type_complexity)]
    fn read_dir(&self, path: &str) -> Result<Vec<FsEntryResult>, FsEntryError>;
}

/// 生产实现：使用 `std::fs::read_dir` + `symlink_metadata`，不跟随 reparse target。
pub struct RealFsReader;

impl FsReader for RealFsReader {
    #[allow(clippy::type_complexity)]
    fn read_dir(&self, path: &str) -> Result<Vec<FsEntryResult>, FsEntryError> {
        let entries =
            std::fs::read_dir(path).map_err(|e| map_io_error(e.kind(), e.raw_os_error()))?;
        let mut result = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    result.push(Err((
                        String::new(),
                        FsEntryError::Io {
                            message: e.to_string(),
                        },
                    )));
                    continue;
                }
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = match std::fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) => {
                    result.push(Err((name, map_io_error(e.kind(), e.raw_os_error()))));
                    continue;
                }
            };
            let is_dir = meta.is_dir();
            let file_size = if is_dir { 0 } else { meta.len() };
            let reparse_tag = if metadata_is_reparse_point(&meta) {
                Some(u32::MAX)
            } else {
                None
            };
            result.push(Ok(FsEntry {
                name,
                is_dir,
                file_size,
                reparse_tag,
            }));
        }
        Ok(result)
    }
}

fn map_io_error(kind: std::io::ErrorKind, raw: Option<i32>) -> FsEntryError {
    if kind == std::io::ErrorKind::PermissionDenied || raw == Some(5) {
        FsEntryError::AccessDenied
    } else {
        FsEntryError::Io {
            message: std::io::Error::from(kind).to_string(),
        }
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

pub(crate) fn normalize(p: &str) -> String {
    p.replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

fn represents_migrated_link(status: &MigrationStatus) -> bool {
    !matches!(status, MigrationStatus::TargetPendingDelete)
}

pub(crate) struct ScanContext {
    preset_by_path: HashMap<String, usize>,
    min_bytes: u64,
}

impl ScanContext {
    pub(crate) fn new(cfg: &Config) -> Self {
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
            preset_by_path,
            min_bytes: cfg.scan.min_size_mb.saturating_mul(1024 * 1024),
        }
    }

    pub(crate) fn preset_index(&self, normalized_path: &str) -> Option<usize> {
        self.preset_by_path.get(normalized_path).copied()
    }
}

fn migration_scan_status(migration: &Migration, link_valid: bool) -> ScanItemStatus {
    if !link_valid {
        ScanItemStatus::LinkBroken
    } else if migration.status == MigrationStatus::Active {
        ScanItemStatus::Migrated
    } else {
        ScanItemStatus::MigrationPending
    }
}

fn scan_status_priority(status: &ScanItemStatus) -> u8 {
    match status {
        // 仅用于 matching_migration 的优先级选择。ExistingLink 不在迁移候选集中
        //（由阶段 1 的 junction 分支单独赋值），故与 Contains 变体同归 0。
        ScanItemStatus::LinkBroken => 4,
        ScanItemStatus::MigrationPending => 3,
        ScanItemStatus::Migrated => 2,
        ScanItemStatus::ExistingLink
        | ScanItemStatus::ContainsMigrated
        | ScanItemStatus::ContainsLink => 0,
    }
}

fn is_exact_migrated_status(status: &ScanItemStatus) -> bool {
    matches!(
        status,
        ScanItemStatus::Migrated | ScanItemStatus::MigrationPending | ScanItemStatus::LinkBroken
    )
}

/// 标注 MFT 目录图，并返回根目录直接文件汇总。
///
/// `link_valid` 通常应传 [`junction::verify`]；`target_size` 通常应传
/// [`dir_size`]。两个回调保留为参数，避免单元测试依赖真实文件系统。
/// MFT 的 skipped record 由 [`annotate_graph_with_skipped_records`] 传入；
/// 此兼容包装默认按 0 处理。
pub fn annotate_graph(
    graph: &mut DirectoryGraph,
    cfg: &Config,
    migrations: &[Migration],
    link_valid: &dyn Fn(&Path) -> bool,
    target_size: &dyn Fn(&Path) -> u64,
) -> RootFileSummary {
    annotate_graph_with_callbacks(
        graph,
        cfg,
        migrations,
        &junction::exists,
        link_valid,
        target_size,
        0,
    )
}

/// 带 MFT skipped record 计数的图标注入口。
pub fn annotate_graph_with_skipped_records(
    graph: &mut DirectoryGraph,
    cfg: &Config,
    migrations: &[Migration],
    link_valid: &dyn Fn(&Path) -> bool,
    target_size: &dyn Fn(&Path) -> u64,
    skipped_records: u64,
) -> RootFileSummary {
    annotate_graph_with_callbacks(
        graph,
        cfg,
        migrations,
        &junction::exists,
        link_valid,
        target_size,
        skipped_records,
    )
}

/// 可注入 junction 分类回调的图标注实现。
///
/// `junction_exists` 只会对带有 reparse tag 且路径非空的节点调用；这使得
/// 任意 reparse tag 不会被误分类为 junction，同时测试无需创建真实 junction。
pub fn annotate_graph_with_callbacks(
    graph: &mut DirectoryGraph,
    cfg: &Config,
    migrations: &[Migration],
    junction_exists: &dyn Fn(&Path) -> bool,
    link_valid: &dyn Fn(&Path) -> bool,
    target_size: &dyn Fn(&Path) -> u64,
    skipped_records: u64,
) -> RootFileSummary {
    let context = ScanContext::new(cfg);

    // 先从 children 建反向索引。后续状态/可见性传播均沿该索引上行，
    // 每条链都带 visited 与 nodes.len() 上界，损坏图也不会导致无限循环。
    let mut parents: HashMap<FileRef, Vec<FileRef>> = HashMap::new();
    for (parent, children) in &graph.children {
        if !graph.nodes.contains_key(parent) {
            continue;
        }
        for child in children {
            if graph.nodes.contains_key(child) {
                let entry = parents.entry(*child).or_default();
                if !entry.contains(parent) {
                    entry.push(*parent);
                }
            }
        }
    }

    // 迁移路径预索引：与 preset_by_path 同理，一次性规范化所有 represented
    // migration 的 source，避免阶段 1 对每个节点 × 每条 migration 重复 normalize
    // （O(V×M) 堆分配）。同一 source 可有多条 migration，保留 Vec 以便按优先级择优。
    let mut migration_index: HashMap<String, Vec<&Migration>> = HashMap::new();
    for migration in migrations {
        if !represents_migrated_link(&migration.status) {
            continue;
        }
        let normalized_source = normalize(&migration.source);
        if normalized_source.is_empty() {
            continue;
        }
        migration_index
            .entry(normalized_source)
            .or_default()
            .push(migration);
    }

    // 阶段 1：预设与 junction 分类，并为每个节点选择精确迁移状态。
    for node in graph.nodes.values_mut() {
        node.matched_preset = None;
        node.category = None;
        node.auto_migrate = false;
        node.is_junction = false;
        node.scan_status = None;
        node.migration_id = None;
        node.linked_target_size_bytes = None;
        node.visible = false;

        let normalized_path = normalize(&node.path);
        if !normalized_path.is_empty() {
            if let Some(index) = context.preset_index(&normalized_path) {
                if let Some(preset) = cfg.presets.get(index) {
                    node.matched_preset = Some(preset.id.clone());
                    node.category = Some(preset.category.clone());
                    node.auto_migrate = preset.auto_migrate;
                }
            }
        }

        if node.reparse_tag.is_some() && !node.path.is_empty() {
            node.is_junction = junction_exists(Path::new(&node.path));
        }

        let matching_migration = migration_index
            .get(&normalized_path)
            .and_then(|candidates| {
                candidates
                    .iter()
                    .map(|&migration| {
                        let status = migration_scan_status(
                            migration,
                            link_valid(Path::new(&migration.source)),
                        );
                        (migration, status)
                    })
                    .max_by_key(|(_, status)| scan_status_priority(status))
            });

        if let Some((migration, status)) = matching_migration {
            node.migration_id = Some(migration.id.clone());
            let target = Path::new(&migration.target);
            if target.exists() {
                node.linked_target_size_bytes = Some(target_size(target));
            }
            node.scan_status = Some(status);
        } else if node.is_junction {
            node.scan_status = Some(ScanItemStatus::ExistingLink);
        }
    }

    // 阶段 2：从精确迁移状态/确认 junction 向祖先传播。迁移状态优先于链接
    // 状态，精确状态最后保持不变。
    let node_refs: Vec<FileRef> = graph.nodes.keys().copied().collect();
    let mut contains_migrated: HashSet<FileRef> = graph
        .nodes
        .iter()
        .filter_map(|(file_ref, node)| {
            node.scan_status
                .as_ref()
                .filter(|status| is_exact_migrated_status(status))
                .map(|_| *file_ref)
        })
        .collect();
    let mut contains_link: HashSet<FileRef> = graph
        .nodes
        .iter()
        .filter_map(|(file_ref, node)| node.is_junction.then_some(*file_ref))
        .collect();

    let hop_limit = graph.nodes.len();
    let mut propagation_queue = VecDeque::new();
    for source in node_refs {
        if contains_migrated.contains(&source) || contains_link.contains(&source) {
            propagation_queue.push_back(source);
        }
    }

    // Each marker is inserted into a node at most once, so the two visited sets
    // also bound propagation through cycles. The hop limit is retained as a
    // defensive upper bound for malformed graphs with unexpectedly large queues.
    let mut propagation_events = 0usize;
    let max_propagation_events = hop_limit.saturating_mul(2);
    while let Some(current) = propagation_queue.pop_front() {
        propagation_events = propagation_events.saturating_add(1);
        if propagation_events > max_propagation_events {
            break;
        }
        let source_migrated = contains_migrated.contains(&current);
        let source_link = contains_link.contains(&current);
        if let Some(source_parents) = parents.get(&current) {
            for parent in source_parents {
                let mut changed = false;
                if source_migrated {
                    changed |= contains_migrated.insert(*parent);
                }
                if source_link {
                    changed |= contains_link.insert(*parent);
                }
                if changed {
                    propagation_queue.push_back(*parent);
                }
            }
        }
    }

    for (file_ref, node) in graph.nodes.iter_mut() {
        if node.scan_status.is_some() {
            continue;
        }
        if contains_migrated.contains(file_ref) {
            node.scan_status = Some(ScanItemStatus::ContainsMigrated);
        } else if contains_link.contains(file_ref) {
            node.scan_status = Some(ScanItemStatus::ContainsLink);
        }
    }

    // 阶段 3：先计算强制可见节点，再沿父链补齐导航祖先。
    let min_bytes = context.min_bytes;
    let forced_visible: HashSet<FileRef> = graph
        .nodes
        .iter()
        .filter_map(|(file_ref, node)| {
            let forced = node.subtree_size_bytes >= min_bytes
                || node.matched_preset.is_some()
                || node.scan_status.is_some()
                || node.access_state == AccessState::Inaccessible;
            forced.then_some(*file_ref)
        })
        .collect();
    let mut visible_refs = forced_visible.clone();
    let mut visibility_queue: VecDeque<FileRef> = forced_visible.into_iter().collect();
    while let Some(current) = visibility_queue.pop_front() {
        if let Some(source_parents) = parents.get(&current) {
            for parent in source_parents {
                if visible_refs.insert(*parent) {
                    visibility_queue.push_back(*parent);
                }
            }
        }
    }
    for (file_ref, node) in graph.nodes.iter_mut() {
        node.visible = !node.excluded && visible_refs.contains(file_ref);
    }

    build_root_summary(graph, skipped_records)
}

/// 从图中实际根 `FileRef` 汇总根目录直接文件与已知系统元数据。
///
/// T4 的 `DirectoryGraph` 不携带 MFT skipped record，因此由调用方显式传入；
/// `skipped_records > 0` 时结果标记为不完整。
///
/// **filesystem 模式语义（T9 接入时处理）：** 此处始终返回
/// `Some(graph.system_metadata_size_bytes)`，因为 `DirectoryGraph.system_metadata_size_bytes`
/// 是 `u64`（MFT 路径汇总 0..15 系统记录）。filesystem 降级路径没有 NTFS 系统元数据
/// 概念，T9 接入时应让该路径返回 `None`（例如用独立标志位区分 MFT/filesystem 来源，
/// 或在 filesystem 路径不调用本函数而由 T9 自行构造 summary）。本函数当前只服务于
/// MFT 路径。
pub fn build_root_summary(graph: &DirectoryGraph, skipped_records: u64) -> RootFileSummary {
    let (direct_file_size_bytes, direct_file_count) = graph
        .nodes
        .get(&graph.root)
        .map(|node| (node.direct_file_size_bytes, node.direct_file_count))
        .unwrap_or((0, 0));
    let system_metadata_size_bytes = Some(graph.system_metadata_size_bytes);
    let total_known_size_bytes =
        direct_file_size_bytes.saturating_add(graph.system_metadata_size_bytes);
    RootFileSummary {
        direct_file_size_bytes,
        direct_file_count,
        system_metadata_size_bytes,
        total_known_size_bytes,
        incomplete: skipped_records > 0,
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
    /// `$REPARSE_POINT` tag。`Some(_)` 表示该目录携带 reparse 属性。
    pub reparse_tag: Option<u32>,
    /// 是否为本工具创建的 junction。T4 留 `false`，由 T5/T8 借 `junction::verify` 填充。
    pub is_junction: bool,
    /// 命中的预设 id。
    pub matched_preset: Option<String>,
    /// 命中预设的分类。
    pub category: Option<PresetCategory>,
    /// 命中预设是否允许自动迁移。
    pub auto_migrate: bool,
    /// MFT 图默认未知；filesystem 扫描可填充可访问性。
    pub access_state: AccessState,
    /// 迁移/链接状态标注。
    pub scan_status: Option<ScanItemStatus>,
    /// 匹配迁移记录的 id。
    pub migration_id: Option<String>,
    /// 迁移目标目录的大小，不覆盖源盘占用统计。
    pub linked_target_size_bytes: Option<u64>,
    /// 是否进入可见树。
    pub visible: bool,
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
    /// 三色 DFS 检测到的循环节点数（按 record 级别去重）。
    pub cycle_nodes: u64,
    /// 建图后未从根可达的目录节点数。
    pub unreachable_nodes: u64,
    /// 根记录 5 不在 `records` 中，或不是目录。
    pub root_missing: bool,
    /// 文件父引用命中一条 `is_dir == false` 且 sequence 一致的 record。
    pub non_dir_parent_files: u64,
    /// 目录父引用命中一条 `is_dir == false` 且 sequence 一致的 record。
    pub non_dir_parent_dirs: u64,
}

/// 线性构建的目录图。
///
/// `nodes` 与 `children` 仅含从根可达的目录节点与边（orphan/cycle/缺父节点
/// 在 Phase 5 末尾物理剔除，但保留在 `diagnostics.unreachable_nodes` 中）。
/// `excluded` 节点保留在 `nodes` 中（T5 需对 excluded 节点做预设/状态标注，
/// 剔除会让 T5 丢失信息）。`root_missing` 时 `nodes` 与 `children` 均为空，
/// `root` 字段为占位 `(5, 0)`——调用方必须先检查 `diagnostics.root_missing`。
/// `system_metadata_size_bytes` 是记录 0..15 除根 5 之外的 logical_size 汇总。
#[derive(Debug, Clone)]
pub struct DirectoryGraph {
    pub nodes: HashMap<FileRef, DirectoryNode>,
    pub root: FileRef,
    pub children: HashMap<FileRef, Vec<FileRef>>,
    pub system_metadata_size_bytes: u64,
    pub diagnostics: GraphDiagnostics,
    /// 被排除节点的 `subtree_size_bytes` 之和（仅计顶层 excluded 节点，避免重复）。
    pub excluded_subtree_size_bytes: u64,
}

impl DirectoryGraph {
    /// 不可达目录节点计数（orphan / cycle / 缺父），作为 ScanDiagnostics.orphan_entries 的代理。
    pub fn orphan_count(&self) -> u64 {
        self.diagnostics.unreachable_nodes
    }
}

/// 从 `MftIndex` 线性构建 `DirectoryGraph`。
///
/// - `excluded_paths`：归一化前的原始排除路径列表（与 `Config.scan.exclude_paths`
///   同语义，会在内部用 [`expand_env`] + [`normalize`] 归一化）。
/// - `root_drive`：根 5 的盘符字母（如 `'C'`）。根路径格式化为 `r"{drive}:\"`。
///
/// 调用方负责传 `&[String]`（T4 不读 `Config`）。
pub fn build_graph(
    index: &MftIndex,
    excluded_paths: &[String],
    root_drive: char,
) -> DirectoryGraph {
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
                    reparse_tag: record.reparse_tag,
                    is_junction: false, // T5/T8 才用 junction::verify 填
                    matched_preset: None,
                    category: None,
                    auto_migrate: false,
                    access_state: AccessState::Unknown,
                    scan_status: None,
                    migration_id: None,
                    linked_target_size_bytes: None,
                    visible: false,
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
                    parent_node.direct_file_count = parent_node.direct_file_count.saturating_add(1);
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
                        diagnostics.non_dir_parent_files += 1;
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
        // F9: 单条目录 record 的自引用仅在 record 级首次计入一次。
        let mut self_ref_counted = false;
        for name in &effective_names {
            let parent_ref = name.parent;

            // 防御：非根目录的"自引用"（不应出现在合法 MFT 中）→ 当作循环。
            if parent_ref == record.id {
                if !self_ref_counted {
                    diagnostics.cycle_nodes += 1;
                    self_ref_counted = true;
                }
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
                        // 父存在但不在 nodes（系统记录或非目录）：计入 non_dir_parent_dirs。
                        diagnostics.non_dir_parent_dirs += 1;
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

    // F5: 稳定 sibling 顺序——按 (record_no, name) 排序，保证跨进程一致。
    for (_, kids) in children.iter_mut() {
        kids.sort_by(|a, b| {
            a.0.record_no
                .cmp(&b.0.record_no)
                .then_with(|| a.1.cmp(&b.1))
        });
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
        // F8: 同一循环节点被多条反向边命中时，仅在 record 级首次计入一次。
        let mut cycle_set: HashSet<FileRef> = HashSet::new();
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
                            // 灰 = 循环；按 record 级去重。
                            if cycle_set.insert(*child_ref) {
                                diagnostics.cycle_nodes += 1;
                            }
                            continue;
                        }
                        Some(2) => {
                            // 黑 = 已完成（first-parent-wins 下不应发生）
                            continue;
                        }
                        _ => {} // 白
                    }
                    color.insert(*child_ref, 1);
                    let parent_path = nodes.get(&node).map(|n| n.path.clone()).unwrap_or_default();
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

    let mut graph = DirectoryGraph {
        nodes,
        root: root_file_ref,
        children: children_index,
        system_metadata_size_bytes,
        diagnostics,
        excluded_subtree_size_bytes: 0,
    };

    // Phase 4/5：排除标记 + 后序聚合（抽出为独立函数供 filesystem 路径复用）。
    aggregate_and_exclude(&mut graph, &normalized_excluded);

    // F1: 从 nodes 中物理剔除 reachable_from_root == false 的节点（orphan /
    // cycle / 缺父）；excluded 节点因其 reachable_from_root 为 true 保留在 nodes 中。
    // F4: root_missing 时由于没有可达锚点，全部节点都是 unreachable，清空 nodes。
    let unreachable_set: HashSet<FileRef> = graph
        .nodes
        .iter()
        .filter(|(_, n)| !n.reachable_from_root)
        .map(|(k, _)| *k)
        .collect();
    for r in &unreachable_set {
        graph.nodes.remove(r);
    }
    // 移除孤儿边：父或子任一为 orphan 时整条边剔除；excluded 节点保留。
    graph.children.retain(|parent, kids| {
        !unreachable_set.contains(parent) && kids.iter().all(|k| !unreachable_set.contains(k))
    });
    // 剔除后，剩余节点的 direct_dir_count 重算以反映实际可达子目录数。
    for (parent, kids) in &graph.children {
        if let Some(parent_node) = graph.nodes.get_mut(parent) {
            parent_node.direct_dir_count = kids.len() as u32;
        }
    }

    graph
}

/// 对 `DirectoryGraph` 执行排除标记 + 后序聚合（从 `build_graph` 抽出的 Phase 4/5）。
///
/// filesystem 路径在 coordinator 构好 nodes/children/root 后调用本函数，即可得到与
/// MFT 路径一致的 `subtree_*`、`excluded`、`excluded_subtree_size_bytes` 语义。
pub fn aggregate_and_exclude(graph: &mut DirectoryGraph, normalized_excluded: &[String]) {
    // Phase 4：排除子树标记。
    for node in graph.nodes.values_mut() {
        if node.path.is_empty() {
            continue;
        }
        let normalized = normalize(&node.path);
        if is_path_excluded(&normalized, normalized_excluded) {
            node.excluded = true;
        }
    }

    // 由 children 建反向父索引，用于计算顶层 excluded 子树。
    let mut parent_of: HashMap<FileRef, FileRef> = HashMap::new();
    for (parent, kids) in &graph.children {
        for kid in kids {
            parent_of.insert(*kid, *parent);
        }
    }

    // Phase 5：后序 O(V+E) 聚合。
    let mut post_order: Vec<FileRef> = Vec::new();
    if graph.nodes.contains_key(&graph.root) {
        let mut stack: Vec<(FileRef, bool)> = vec![(graph.root, false)];
        let mut visited: HashSet<FileRef> = HashSet::new();
        visited.insert(graph.root);
        while let Some((node, processing)) = stack.pop() {
            if processing {
                post_order.push(node);
                continue;
            }
            stack.push((node, true));
            if let Some(kids) = graph.children.get(&node) {
                for kid in kids.iter().rev() {
                    if visited.insert(*kid) {
                        stack.push((*kid, false));
                    }
                }
            }
        }
    }

    let mut aggregate: HashMap<FileRef, (u64, u64, u64)> = HashMap::new();
    let mut excluded_subtree_size_bytes: u64 = 0;

    for node_ref in &post_order {
        let node = graph
            .nodes
            .get(node_ref)
            .expect("post_order 仅含已入 nodes 的节点");
        let mut size = node.direct_file_size_bytes;
        let mut file_count = node.direct_file_count;
        let mut dir_count: u64 = 1; // 包含自身

        if !node.excluded {
            // 非 excluded 节点：聚合非 excluded 子的 subtree。
            if let Some(kids) = graph.children.get(node_ref) {
                for kid in kids {
                    if let Some(kid_node) = graph.nodes.get(kid) {
                        if kid_node.excluded {
                            continue;
                        }
                        if let Some(&(ks, kfc, kdc)) = aggregate.get(kid) {
                            size = size.saturating_add(ks);
                            file_count = file_count.saturating_add(kfc);
                            dir_count = dir_count.saturating_add(kdc);
                        }
                    }
                }
            }
        } else {
            // excluded 节点聚合全部子节点（含 excluded 子），用于 excluded_subtree_size_bytes。
            if let Some(kids) = graph.children.get(node_ref) {
                for kid in kids {
                    if let Some(&(ks, kfc, kdc)) = aggregate.get(kid) {
                        size = size.saturating_add(ks);
                        file_count = file_count.saturating_add(kfc);
                        dir_count = dir_count.saturating_add(kdc);
                    }
                }
            }
        }

        aggregate.insert(*node_ref, (size, file_count, dir_count));
    }

    // 写回 subtree_* 字段。
    for (node_ref, (size, file_count, dir_count)) in &aggregate {
        if let Some(node) = graph.nodes.get_mut(node_ref) {
            node.subtree_size_bytes = *size;
            node.subtree_file_count = *file_count;
            node.subtree_dir_count = *dir_count;
        }
    }

    // excluded_subtree_size_bytes 只累加顶层 excluded 节点。
    for (node_ref, (size, _, _)) in &aggregate {
        let node_is_excluded = graph
            .nodes
            .get(node_ref)
            .map(|n| n.excluded)
            .unwrap_or(false);
        if !node_is_excluded {
            continue;
        }
        let is_top = match parent_of.get(node_ref) {
            None => true,
            Some(p) => graph.nodes.get(p).map(|n| !n.excluded).unwrap_or(true),
        };
        if is_top {
            excluded_subtree_size_bytes = excluded_subtree_size_bytes.saturating_add(*size);
        }
    }

    graph.excluded_subtree_size_bytes = excluded_subtree_size_bytes;
}

// ===== TreeStore =====

/// 不可变树快照，供分页 / reveal / recommended 查询。
#[derive(Debug, Clone)]
pub struct TreeStore {
    pub(crate) scan_id: String,
    pub(crate) source: ScanSource,
    pub(crate) root_file_summary: RootFileSummary,
    pub(crate) nodes: HashMap<String, TreeNode>,
    pub(crate) children: HashMap<String, Vec<String>>,
    pub(crate) parent: HashMap<String, String>,
    pub(crate) roots: Vec<String>,
    pub(crate) filtered_root_count: u32,
    pub(crate) recommended: Vec<String>,
}

impl TreeStore {
    /// 从各分片构造 `TreeStore`。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_parts(
        scan_id: String,
        source: ScanSource,
        root_file_summary: RootFileSummary,
        nodes: HashMap<String, TreeNode>,
        children: HashMap<String, Vec<String>>,
        parent: HashMap<String, String>,
        roots: Vec<String>,
        filtered_root_count: u32,
        recommended: Vec<String>,
    ) -> Self {
        Self {
            scan_id,
            source,
            root_file_summary,
            nodes,
            children,
            parent,
            roots,
            filtered_root_count,
            recommended,
        }
    }

    pub fn scan_id(&self) -> &str {
        &self.scan_id
    }

    pub fn source(&self) -> ScanSource {
        self.source
    }

    pub fn root_file_summary(&self) -> &RootFileSummary {
        &self.root_file_summary
    }

    pub fn filtered_root_count(&self) -> u32 {
        self.filtered_root_count
    }

    pub fn roots(&self) -> Vec<TreeNode> {
        self.roots
            .iter()
            .filter_map(|p| self.nodes.get(p).cloned())
            .collect()
    }

    pub fn node(&self, path: &str) -> Option<&TreeNode> {
        self.nodes.get(&normalize(path))
    }

    pub fn children_page(&self, path: &str, offset: u32, limit: u32) -> ChildPage {
        let limit = limit.clamp(1, 500);
        let all = self
            .children
            .get(&normalize(path))
            .cloned()
            .unwrap_or_default();
        let total = all.len() as u32;
        if offset >= total {
            return ChildPage {
                items: Vec::new(),
                total,
                next_offset: None,
            };
        }
        let end = (offset + limit).min(total);
        let items = all[offset as usize..end as usize]
            .iter()
            .filter_map(|p| self.nodes.get(p).cloned())
            .collect();
        let next_offset = if end < total { Some(end) } else { None };
        ChildPage {
            items,
            total,
            next_offset,
        }
    }

    pub fn recommended(&self) -> Vec<TreeNode> {
        self.recommended
            .iter()
            .filter_map(|p| self.nodes.get(p).cloned())
            .collect()
    }

    pub fn reveal_pages(
        &self,
        path: &str,
        limit: u32,
    ) -> Result<Vec<RevealLevel>, crate::error::AppError> {
        let target_norm = normalize(path);
        if !self.nodes.contains_key(&target_norm) {
            return Err(crate::error::AppError::Store(format!(
                "reveal 目标不存在: {}",
                path
            )));
        }
        let limit = limit.clamp(1, 500);

        // 沿 parent 索引从 target 回溯到根，得到 [target, ..., 一级子]。
        // 一级子的 parent == root_norm；root_norm 不在 nodes 中，回溯遇到它即停止
        // （根不作为可 reveal 的层级节点，只作为第一层的 parent_path）。
        let mut chain: Vec<String> = vec![target_norm.clone()];
        let mut cur = target_norm.clone();
        while let Some(p) = self.parent.get(&cur) {
            if *p == cur {
                break; // 防御自环
            }
            if !self.nodes.contains_key(p) {
                break; // p 是根（不在 nodes），停止回溯
            }
            chain.push(p.clone());
            cur = p.clone();
        }
        // chain: [target, ..., 一级子] -> 反转为 [一级子, ..., target]
        chain.reverse();

        let root_norm = normalize(r"c:\");
        let root_display = r"c:\".to_string();
        let mut levels: Vec<RevealLevel> = Vec::new();
        for (i, child_norm) in chain.iter().enumerate() {
            // 该层的 parent：第 0 层是根，其余层是 chain[i-1]。
            let parent_norm = if i == 0 {
                root_norm.clone()
            } else {
                chain[i - 1].clone()
            };
            let parent_display = if i == 0 {
                root_display.clone()
            } else {
                self.nodes
                    .get(&chain[i - 1])
                    .map(|n| n.path.clone())
                    .unwrap_or_else(|| chain[i - 1].clone())
            };
            // 定位到包含 child_norm 的实际页（目标可能不在该层第一页）。
            let kids = self.children.get(&parent_norm).cloned().unwrap_or_default();
            let idx = kids.iter().position(|p| p == child_norm).unwrap_or(0) as u32;
            let page_offset = (idx / limit) * limit;
            let page = self.children_page(&parent_display, page_offset, limit);
            levels.push(RevealLevel {
                parent_path: parent_display,
                page,
            });
        }
        Ok(levels)
    }
}

// ===== Materialize =====

/// 将 `DirectoryGraph` 物化为不可变 `TreeStore`。
///
/// 只保留 `visible == true` 的目录节点；
/// 内部路径 key 使用统一的大小写无关规范化形式（`crate::scanner::normalize`）。
pub fn materialize(
    graph: &DirectoryGraph,
    root_file_summary: RootFileSummary,
    source: ScanSource,
    scan_id: String,
) -> TreeStore {
    // 第 1 步：FileRef -> 规范化 path 映射（仅 visible 节点）
    let mut ref_to_path: HashMap<FileRef, String> = HashMap::new();
    for (fr, node) in &graph.nodes {
        if node.visible {
            ref_to_path.insert(*fr, normalize(&node.path));
        }
    }

    let root_norm = normalize(r"c:\");

    // 第 2 步：构建 children（size 降序，path 升序）+ parent 两个 map
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut parent: HashMap<String, String> = HashMap::new();

    for (parent_fr, child_frs) in &graph.children {
        let parent_norm = if *parent_fr == graph.root {
            root_norm.clone()
        } else if let Some(pn) = graph.nodes.get(parent_fr) {
            if !pn.visible {
                continue;
            }
            normalize(&pn.path)
        } else {
            continue;
        };

        let mut visible_children: Vec<(u64, String)> = Vec::new();
        for child_fr in child_frs {
            if let Some(child_node) = graph.nodes.get(child_fr) {
                if child_node.visible {
                    let child_norm = normalize(&child_node.path);
                    visible_children.push((child_node.subtree_size_bytes, child_norm.clone()));
                    parent.insert(child_norm, parent_norm.clone());
                }
            }
        }
        // size 降序；同 size 用规范化 path 升序。
        visible_children.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let child_paths: Vec<String> = visible_children.into_iter().map(|(_, p)| p).collect();
        children.insert(parent_norm.clone(), child_paths);
    }

    // 第 3 步：构建 nodes map（visible 节点 -> TreeNode）
    let mut nodes: HashMap<String, TreeNode> = HashMap::new();
    for (fr, node) in &graph.nodes {
        if !node.visible {
            continue;
        }
        let norm_path = ref_to_path
            .get(fr)
            .cloned()
            .unwrap_or_else(|| normalize(&node.path));
        let child_list = children.get(&norm_path).cloned().unwrap_or_default();
        let child_count = child_list.len() as u32;
        let filtered_child_count = graph
            .children
            .get(fr)
            .map(|kids| {
                kids.iter()
                    .filter(|c| {
                        graph
                            .nodes
                            .get(c)
                            .map(|n| !n.visible && !n.excluded)
                            .unwrap_or(false)
                    })
                    .count() as u32
            })
            .unwrap_or(0);

        nodes.insert(
            norm_path.clone(),
            TreeNode {
                path: node.path.clone(),
                display_name: node.display_name.clone(),
                size_bytes: node.subtree_size_bytes,
                linked_target_size_bytes: node.linked_target_size_bytes,
                file_count: node.subtree_file_count,
                dir_count: node.subtree_dir_count,
                depth: node.depth,
                is_reparse: node.reparse_tag.is_some(),
                reparse_tag: node.reparse_tag,
                is_junction: node.is_junction,
                access_state: node.access_state,
                matched_preset: node.matched_preset.clone(),
                category: node.category.clone(),
                auto_migrate: node.auto_migrate,
                scan_status: node.scan_status.clone(),
                migration_id: node.migration_id.clone(),
                child_count,
                filtered_child_count,
            },
        );
    }

    // 第 4 步：roots（direct children of root, size 降序）
    let mut root_children: Vec<(u64, String)> = children
        .get(&root_norm)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| nodes.get(&p).map(|n| (n.size_bytes, p)))
        .collect();
    root_children.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let roots: Vec<String> = root_children.into_iter().map(|(_, p)| p).collect();

    // filtered_root_count: graph.children[graph.root] 中 !visible && !excluded 的子数
    let filtered_root_count = graph
        .children
        .get(&graph.root)
        .map(|kids| {
            kids.iter()
                .filter(|c| {
                    graph
                        .nodes
                        .get(c)
                        .map(|n| !n.visible && !n.excluded)
                        .unwrap_or(false)
                })
                .count() as u32
        })
        .unwrap_or(0);

    // 第 5 步：recommended
    let mut recommended_candidates: Vec<(u64, String)> = Vec::new();
    for (norm_path, node) in &nodes {
        if node.matched_preset.is_some()
            && node.scan_status.is_none()
            && !node.is_reparse
            && node.access_state != AccessState::Inaccessible
        {
            recommended_candidates.push((node.size_bytes, norm_path.clone()));
        }
    }
    recommended_candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let recommended: Vec<String> = recommended_candidates.into_iter().map(|(_, p)| p).collect();

    TreeStore::from_parts(
        scan_id,
        source,
        root_file_summary,
        nodes,
        children,
        parent,
        roots,
        filtered_root_count,
        recommended,
    )
}

// ===== T9: 有界并发 filesystem 扫描 worker / coordinator =====

/// 磁盘类型，用于并发策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriveKind {
    Fixed,
    Removable,
    Network,
    Other,
}

/// 根据盘符类型决定 worker 数：固定磁盘 `min(available_parallelism, 4)` 并 clamp 到 1..=8；
/// 可移动/网络介质降到 1。
pub fn concurrency_for(drive_type_fn: &dyn Fn(char) -> DriveKind, root_drive: char) -> usize {
    match drive_type_fn(root_drive) {
        DriveKind::Removable | DriveKind::Network => 1,
        _ => {
            let avail = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            avail.min(4).clamp(1, 8)
        }
    }
}

#[cfg(windows)]
fn win32_drive_kind(drive: char) -> DriveKind {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    // Win32 GetDriveTypeW 返回值（WinBase.h；硬编码避免引入额外 feature）：
    //   0 = DRIVE_UNKNOWN, 1 = DRIVE_NO_ROOT_DIR, 2 = DRIVE_REMOVABLE,
    //   3 = DRIVE_FIXED,   4 = DRIVE_REMOTE,    5 = DRIVE_CDROM,
    //   6 = DRIVE_RAMDISK.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    let path = format!("{}:\\", drive);
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let kind = unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) };
    match kind {
        x if x == DRIVE_REMOVABLE => DriveKind::Removable,
        x if x == DRIVE_REMOTE => DriveKind::Network,
        x if x == DRIVE_FIXED => DriveKind::Fixed,
        _ => DriveKind::Other,
    }
}

#[cfg(not(windows))]
fn win32_drive_kind(_drive: char) -> DriveKind {
    DriveKind::Fixed
}

/// 为 filesystem 路径生成稳定 FileRef：同路径跨进程/跨运行一致。
fn stable_hash_path(path: &str) -> u64 {
    // FNV-1a：简单、稳定、无 std hash 版本依赖。
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in path.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// worker 产出的单目录观察。
struct DirectoryObservation {
    pub norm_path: String,
    pub display_path: String,
    pub parent_norm_path: Option<String>,
    pub display_name: String,
    pub depth: u32,
    pub direct_files: Vec<(String, u64)>,
    /// (子目录名, 若该 entry 本身是 reparse point 则为 Some(tag)，否则 None)
    pub direct_dirs: Vec<(String, Option<u32>)>,
    pub reparse_tag: Option<u32>,
    pub access_denied: bool,
    pub entry_errors: Vec<(String, String)>,
}

/// 等待 worker 读取的目录任务单元。
#[derive(Clone)]
struct DirWorkItem {
    pub norm_path: String,
    pub display_path: String,
    pub parent_norm_path: Option<String>,
    pub display_name: String,
    pub depth: u32,
    pub reparse_tag: Option<u32>,
}

fn process_fs_item(
    reader: &dyn FsReader,
    item: DirWorkItem,
    cancel: &AtomicBool,
) -> DirectoryObservation {
    let item_norm_path = item.norm_path;
    if cancel.load(Ordering::Relaxed) {
        return DirectoryObservation {
            norm_path: item_norm_path,
            display_path: item.display_path,
            parent_norm_path: item.parent_norm_path,
            display_name: item.display_name,
            depth: item.depth,
            direct_files: Vec::new(),
            direct_dirs: Vec::new(),
            reparse_tag: item.reparse_tag,
            access_denied: false,
            entry_errors: Vec::new(),
        };
    }

    // Reparse point：记录 tag 后停止下钻，不读其 target 内容。
    if item.reparse_tag.is_some() {
        return DirectoryObservation {
            norm_path: item_norm_path,
            display_path: item.display_path,
            parent_norm_path: item.parent_norm_path,
            display_name: item.display_name,
            depth: item.depth,
            direct_files: Vec::new(),
            direct_dirs: Vec::new(),
            reparse_tag: item.reparse_tag,
            access_denied: false,
            entry_errors: Vec::new(),
        };
    }

    match reader.read_dir(&item_norm_path) {
        Err(FsEntryError::AccessDenied) => DirectoryObservation {
            norm_path: item_norm_path,
            display_path: item.display_path,
            parent_norm_path: item.parent_norm_path,
            display_name: item.display_name,
            depth: item.depth,
            direct_files: Vec::new(),
            direct_dirs: Vec::new(),
            reparse_tag: None,
            access_denied: true,
            entry_errors: Vec::new(),
        },
        Err(FsEntryError::Io { message }) => DirectoryObservation {
            norm_path: item_norm_path,
            display_path: item.display_path,
            parent_norm_path: item.parent_norm_path,
            display_name: item.display_name,
            depth: item.depth,
            direct_files: Vec::new(),
            direct_dirs: Vec::new(),
            reparse_tag: None,
            access_denied: false,
            entry_errors: vec![(String::new(), message)],
        },
        Ok(entries) => {
            let mut direct_files = Vec::new();
            let mut direct_dirs = Vec::new();
            let mut entry_errors = Vec::new();
            for entry_result in entries {
                match entry_result {
                    Ok(entry) => {
                        if entry.is_dir {
                            direct_dirs.push((entry.name, entry.reparse_tag));
                        } else {
                            direct_files.push((entry.name, entry.file_size));
                        }
                    }
                    Err((name, err)) => {
                        let msg = match err {
                            FsEntryError::AccessDenied => "AccessDenied".to_string(),
                            FsEntryError::Io { message } => message,
                        };
                        entry_errors.push((name, msg));
                    }
                }
            }
            DirectoryObservation {
                norm_path: item_norm_path,
                display_path: item.display_path,
                parent_norm_path: item.parent_norm_path,
                display_name: item.display_name,
                depth: item.depth,
                direct_files,
                direct_dirs,
                reparse_tag: None,
                access_denied: false,
                entry_errors,
            }
        }
    }
}

fn fs_worker(
    reader: Arc<dyn FsReader>,
    queue: Arc<WorkQueue>,
    obs_tx: mpsc::Sender<DirectoryObservation>,
    cancel: Arc<AtomicBool>,
) {
    loop {
        let item = {
            let mut q = queue.queue.lock().unwrap();
            loop {
                if cancel.load(Ordering::Relaxed) || queue.closed.load(Ordering::Relaxed) {
                    return;
                }
                if let Some(item) = q.pop_front() {
                    break item;
                }
                let result = queue
                    .cvar
                    .wait_timeout(q, Duration::from_millis(100))
                    .unwrap();
                q = result.0;
            }
        };
        let obs = process_fs_item(reader.as_ref(), item, &cancel);
        if obs_tx.send(obs).is_err() {
            return;
        }
    }
}

struct WorkQueue {
    queue: Mutex<VecDeque<DirWorkItem>>,
    cvar: Condvar,
    closed: AtomicBool,
}

/// 有界并发 coordinator：worker 只产观察，单线程构图 + 聚合。
fn coordinator_run(
    reader: Arc<dyn FsReader>,
    root_drive: char,
    excluded_paths: &[String],
    worker_count: usize,
    cancel: Arc<AtomicBool>,
    on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
) -> Result<(DirectoryGraph, u64, u64), ScanDriveError> {
    let normalized_excluded: Vec<String> = excluded_paths
        .iter()
        .filter_map(|p| {
            let normalized = normalize(&expand_env(p));
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect();

    let worker_count = worker_count.clamp(1, 8);
    let queue_capacity = worker_count.saturating_mul(4).max(1);
    let queue = Arc::new(WorkQueue {
        queue: Mutex::new(VecDeque::new()),
        cvar: Condvar::new(),
        closed: AtomicBool::new(false),
    });
    let (obs_tx, obs_rx) = mpsc::channel::<DirectoryObservation>();

    let root_norm = normalize(&format!("{}:\\", root_drive));
    let root_display = format!("{}:\\", root_drive);
    {
        let mut q = queue.queue.lock().unwrap();
        q.push_back(DirWorkItem {
            norm_path: root_norm.clone(),
            display_path: root_display.clone(),
            parent_norm_path: None,
            display_name: root_drive.to_string(),
            depth: 0,
            reparse_tag: None,
        });
    }
    queue.cvar.notify_all();

    for _ in 0..worker_count {
        let reader = reader.clone();
        let queue = queue.clone();
        let obs_tx = obs_tx.clone();
        let cancel = cancel.clone();
        thread::spawn(move || fs_worker(reader, queue, obs_tx, cancel));
    }
    drop(obs_tx);

    let mut graph = DirectoryGraph {
        nodes: HashMap::new(),
        root: FileRef {
            record_no: stable_hash_path(&root_norm),
            sequence: 1,
        },
        children: HashMap::new(),
        system_metadata_size_bytes: 0,
        diagnostics: GraphDiagnostics::default(),
        excluded_subtree_size_bytes: 0,
    };
    let mut path_to_ref: HashMap<String, FileRef> = HashMap::new();
    let mut pending_dirs: VecDeque<DirWorkItem> = VecDeque::new();
    let mut outstanding: usize = 1; // 根已入队
    let mut scanned_files: u64 = 0;
    let mut entry_errors: u64 = 0;
    let mut completed: u64 = 0;
    let progress_interval: u64 = 4096;

    loop {
        if cancel.load(Ordering::Relaxed) {
            pending_dirs.clear();
            queue.closed.store(true, Ordering::Relaxed);
            queue.cvar.notify_all();
            while obs_rx.recv_timeout(Duration::from_millis(100)).is_ok() {}
            return Err(ScanDriveError::Cancelled);
        }

        // 尽量把 pending 目录推进有界队列。
        while let Some(item) = pending_dirs.front() {
            let item = item.clone();
            let mut q = queue.queue.lock().unwrap();
            if q.len() >= queue_capacity {
                break;
            }
            q.push_back(item);
            pending_dirs.pop_front();
            queue.cvar.notify_one();
        }

        match obs_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(obs) => {
                outstanding = outstanding.saturating_sub(1);

                let file_ref = FileRef {
                    record_no: stable_hash_path(&obs.norm_path),
                    sequence: 1,
                };
                path_to_ref.insert(obs.norm_path.clone(), file_ref);

                let direct_file_size_bytes = obs
                    .direct_files
                    .iter()
                    .map(|(_, size)| *size)
                    .fold(0u64, u64::saturating_add);
                let direct_file_count = obs.direct_files.len() as u64;
                scanned_files = scanned_files.saturating_add(direct_file_count);

                let node = DirectoryNode {
                    file_ref,
                    path: obs.display_path.clone(),
                    display_name: obs.display_name,
                    depth: obs.depth,
                    reparse_tag: obs.reparse_tag,
                    is_junction: false,
                    matched_preset: None,
                    category: None,
                    auto_migrate: false,
                    access_state: if obs.access_denied {
                        AccessState::Inaccessible
                    } else {
                        AccessState::Unknown
                    },
                    scan_status: None,
                    migration_id: None,
                    linked_target_size_bytes: None,
                    visible: false,
                    direct_file_size_bytes,
                    direct_file_count,
                    direct_dir_count: obs.direct_dirs.len() as u32,
                    subtree_size_bytes: 0,
                    subtree_file_count: 0,
                    subtree_dir_count: 0,
                    excluded: false,
                    reachable_from_root: true,
                };
                graph.nodes.insert(file_ref, node);

                // entry_errors 计数（非 AccessDenied，AccessDenied 已转 access_state）。
                entry_errors = entry_errors.saturating_add(obs.entry_errors.len() as u64);

                if let Some(parent_norm) = obs.parent_norm_path {
                    if let Some(parent_ref) = path_to_ref.get(&parent_norm) {
                        graph
                            .children
                            .entry(*parent_ref)
                            .or_default()
                            .push(file_ref);
                    }
                } else {
                    graph.root = file_ref;
                }

                // 非 reparse / 非 AccessDenied 才继续下钻子目录。
                if obs.reparse_tag.is_none() && !obs.access_denied {
                    for (dir_name, child_reparse) in &obs.direct_dirs {
                        let child_display = if obs.display_path.ends_with('\\') {
                            format!("{}{}", obs.display_path, dir_name)
                        } else {
                            format!("{}\\{}", obs.display_path, dir_name)
                        };
                        let child_norm = normalize(&child_display);
                        pending_dirs.push_back(DirWorkItem {
                            norm_path: child_norm,
                            display_path: child_display,
                            parent_norm_path: Some(obs.norm_path.clone()),
                            display_name: dir_name.clone(),
                            depth: obs.depth + 1,
                            reparse_tag: *child_reparse,
                        });
                        outstanding = outstanding.saturating_add(1);
                    }
                }

                completed = completed.saturating_add(1);
                if completed.is_multiple_of(progress_interval) {
                    on_progress(ScanProgressEvent {
                        scanned_records: 0,
                        scanned_dirs: completed,
                        scanned_files,
                        estimated_record_slots: 0,
                        current_phase: CurrentPhase::WalkingFs,
                    });
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                let q = queue.queue.lock().unwrap();
                if outstanding == 0 && pending_dirs.is_empty() && q.is_empty() {
                    break;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    queue.closed.store(true, Ordering::Relaxed);
    queue.cvar.notify_all();
    while obs_rx.recv_timeout(Duration::from_millis(100)).is_ok() {}

    // 稳定 sibling 顺序：按 display_name 排序，保证测试可重复。
    let graph_ref = &mut graph;
    for kids in graph_ref.children.values_mut() {
        kids.sort_by_key(|fr| {
            graph_ref
                .nodes
                .get(fr)
                .map(|n| n.display_name.clone())
                .unwrap_or_default()
        });
    }

    aggregate_and_exclude(graph_ref, &normalized_excluded);

    Ok((graph, scanned_files, entry_errors))
}

// ===== T7：结构化扫描引擎 =====

/// 扫描任务内部错误，commands 层映射为 `ScanDriveResult`。
#[derive(Debug)]
pub enum ScanDriveError {
    NeedsElevation,
    FastScanFailure(FastScanFailure),
    Cancelled,
}

/// 扫描成功产物：不可变树快照 + 诊断计数。
pub struct ScanOutcome {
    pub store: Arc<TreeStore>,
    pub diagnostics: ScanDiagnostics,
}

/// 可注入扫描引擎边界。生产用 `RealScanEngine`，测试用 `MockScanEngine`。
pub trait ScanEngine: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        mode: ScanMode,
        root_drive: char,
        cfg: Config,
        migrations: Vec<Migration>,
        excluded_paths: Vec<String>,
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
    ) -> Result<ScanOutcome, ScanDriveError>;
}

/// 生产扫描引擎：编排 MFT / filesystem 两条路线。
pub struct RealScanEngine;

impl ScanEngine for RealScanEngine {
    fn run(
        &self,
        mode: ScanMode,
        root_drive: char,
        cfg: Config,
        migrations: Vec<Migration>,
        excluded_paths: Vec<String>,
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
    ) -> Result<ScanOutcome, ScanDriveError> {
        match mode {
            ScanMode::Auto | ScanMode::Mft => self.mft_scan(
                root_drive,
                &cfg,
                &migrations,
                &excluded_paths,
                cancel,
                on_progress,
            ),
            ScanMode::Filesystem => self.fs_scan(
                root_drive,
                &cfg,
                &migrations,
                &excluded_paths,
                cancel,
                on_progress,
            ),
        }
    }
}

impl RealScanEngine {
    #[cfg(windows)]
    fn mft_scan(
        &self,
        root_drive: char,
        cfg: &Config,
        migrations: &[Migration],
        excluded_paths: &[String],
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
    ) -> Result<ScanOutcome, ScanDriveError> {
        let vol = open_volume(root_drive).map_err(map_volume_error)?;
        let volume_data = read_volume_data(&vol).map_err(map_volume_error)?;
        let reader = MftFileReader::open(&vol, volume_data).map_err(map_mft_error)?;
        let record_count =
            volume_data.mft_valid_data_length / volume_data.bytes_per_file_record_segment as u64;

        let index = enumerate_mft(
            &reader,
            volume_data,
            &mut || cancel.load(Ordering::Relaxed),
            &mut |scanned| {
                on_progress(ScanProgressEvent {
                    scanned_records: scanned,
                    scanned_dirs: 0,
                    scanned_files: 0,
                    estimated_record_slots: record_count,
                    current_phase: CurrentPhase::ReadingMft,
                });
            },
        )
        .map_err(map_mft_error)?;

        on_progress(ScanProgressEvent {
            scanned_records: index.scanned_records,
            scanned_dirs: 0,
            scanned_files: index.scanned_files,
            estimated_record_slots: record_count,
            current_phase: CurrentPhase::Aggregating,
        });

        let mut graph = build_graph(&index, excluded_paths, root_drive);

        on_progress(ScanProgressEvent {
            scanned_records: index.scanned_records,
            scanned_dirs: graph.nodes.len() as u64,
            scanned_files: index.scanned_files,
            estimated_record_slots: record_count,
            current_phase: CurrentPhase::Annotating,
        });

        let root_summary = annotate_graph_with_callbacks(
            &mut graph,
            cfg,
            migrations,
            &junction::exists,
            &|source| junction::verify(source),
            &dir_size,
            index.skipped_records,
        );

        let scan_id = generate_scan_id();
        let store = materialize(&graph, root_summary, ScanSource::Mft, scan_id);

        let diagnostics = ScanDiagnostics {
            scanned_records: index.scanned_records,
            scanned_dirs: graph.nodes.len() as u64,
            scanned_files: index.scanned_files,
            skipped_records: index.skipped_records,
            orphan_entries: graph.orphan_count(),
            hard_link_entries: index.hard_link_entries,
            unresolved_extensions: index.unresolved_extensions,
        };

        Ok(ScanOutcome {
            store: Arc::new(store),
            diagnostics,
        })
    }

    #[cfg(not(windows))]
    fn mft_scan(
        &self,
        _root_drive: char,
        _cfg: &Config,
        _migrations: &[Migration],
        _excluded_paths: &[String],
        _cancel: Arc<AtomicBool>,
        _on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
    ) -> Result<ScanOutcome, ScanDriveError> {
        Err(ScanDriveError::FastScanFailure(FastScanFailure::Io {
            code: None,
        }))
    }

    fn fs_scan(
        &self,
        root_drive: char,
        cfg: &Config,
        migrations: &[Migration],
        excluded_paths: &[String],
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
    ) -> Result<ScanOutcome, ScanDriveError> {
        self.fs_scan_with_reader(
            root_drive,
            cfg,
            migrations,
            excluded_paths,
            cancel,
            on_progress,
            Arc::new(RealFsReader),
            &win32_drive_kind,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn fs_scan_with_reader(
        &self,
        root_drive: char,
        cfg: &Config,
        migrations: &[Migration],
        excluded_paths: &[String],
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgressEvent) + Send + Sync>,
        reader: Arc<dyn FsReader>,
        drive_type_fn: &dyn Fn(char) -> DriveKind,
    ) -> Result<ScanOutcome, ScanDriveError> {
        on_progress(ScanProgressEvent {
            scanned_records: 0,
            scanned_dirs: 0,
            scanned_files: 0,
            estimated_record_slots: 0,
            current_phase: CurrentPhase::WalkingFs,
        });

        let worker_count = concurrency_for(drive_type_fn, root_drive);
        let (mut graph, scanned_files, entry_errors) = coordinator_run(
            reader,
            root_drive,
            excluded_paths,
            worker_count,
            cancel.clone(),
            on_progress.clone(),
        )?;

        on_progress(ScanProgressEvent {
            scanned_records: 0,
            scanned_dirs: graph.nodes.len() as u64,
            scanned_files,
            estimated_record_slots: 0,
            current_phase: CurrentPhase::Annotating,
        });

        // 标注阶段只取副作用（preset/visible/excluded 等），丢弃其 RootFileSummary。
        let _ = annotate_graph_with_callbacks(
            &mut graph,
            cfg,
            migrations,
            &junction::exists,
            &|source| junction::verify(source),
            &dir_size,
            0,
        );

        // filesystem 降级路径自构 RootFileSummary：无系统元数据，结果不完整。
        let (root_direct_size, root_direct_count) = graph
            .nodes
            .get(&graph.root)
            .map(|n| (n.direct_file_size_bytes, n.direct_file_count))
            .unwrap_or((0, 0));
        let root_summary = RootFileSummary {
            direct_file_size_bytes: root_direct_size,
            direct_file_count: root_direct_count,
            system_metadata_size_bytes: None,
            total_known_size_bytes: root_direct_size,
            incomplete: true,
        };

        let scan_id = generate_scan_id();
        let store = materialize(&graph, root_summary, ScanSource::Filesystem, scan_id);

        let diagnostics = ScanDiagnostics {
            scanned_records: 0,
            scanned_dirs: graph.nodes.len() as u64,
            scanned_files,
            skipped_records: entry_errors,
            orphan_entries: graph.orphan_count(),
            hard_link_entries: 0,
            unresolved_extensions: 0,
        };

        Ok(ScanOutcome {
            store: Arc::new(store),
            diagnostics,
        })
    }
}

fn generate_scan_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

#[cfg(windows)]
fn map_volume_error(e: VolumeError) -> ScanDriveError {
    match e {
        VolumeError::AccessDenied => ScanDriveError::NeedsElevation,
        VolumeError::UnsupportedFilesystem { actual } => {
            ScanDriveError::FastScanFailure(FastScanFailure::UnsupportedFilesystem { actual })
        }
        VolumeError::InvalidVolumeData => {
            ScanDriveError::FastScanFailure(FastScanFailure::InvalidVolumeData)
        }
        VolumeError::Io { code, .. } => ScanDriveError::FastScanFailure(FastScanFailure::Io {
            code: Some(code as i32),
        }),
    }
}

fn map_mft_error(e: MftError) -> ScanDriveError {
    match e {
        MftError::NeedElevation => ScanDriveError::NeedsElevation,
        MftError::UnsupportedFilesystem { actual } => {
            ScanDriveError::FastScanFailure(FastScanFailure::UnsupportedFilesystem { actual })
        }
        MftError::UnsupportedNtfsVersion { major, minor } => {
            ScanDriveError::FastScanFailure(FastScanFailure::UnsupportedNtfsVersion {
                major,
                minor,
            })
        }
        MftError::InvalidVolumeData => {
            ScanDriveError::FastScanFailure(FastScanFailure::InvalidVolumeData)
        }
        MftError::RootRecordMissing => {
            ScanDriveError::FastScanFailure(FastScanFailure::RootRecordMissing)
        }
        MftError::ExcessiveRecordErrors { skipped, scanned } => {
            ScanDriveError::FastScanFailure(FastScanFailure::ExcessiveRecordErrors {
                skipped,
                scanned,
            })
        }
        MftError::BadRecord { .. } => {
            ScanDriveError::FastScanFailure(FastScanFailure::Io { code: None })
        }
        MftError::Io(e) => ScanDriveError::FastScanFailure(FastScanFailure::Io {
            code: e.raw_os_error(),
        }),
        MftError::Cancelled => ScanDriveError::Cancelled,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

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
                unresolved_extensions: 0,
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
                    g1.nodes[&key].path, g2.nodes[&key].path,
                    "path mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].path, g3.nodes[&key].path,
                    "path mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].depth, g2.nodes[&key].depth,
                    "depth mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].subtree_size_bytes, g2.nodes[&key].subtree_size_bytes,
                    "subtree_size mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].subtree_file_count, g2.nodes[&key].subtree_file_count,
                    "subtree_file_count mismatch for {:?}",
                    key
                );
                assert_eq!(
                    g1.nodes[&key].subtree_dir_count, g2.nodes[&key].subtree_dir_count,
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
            // dir 30 是 unreachable orphan，被物理剔除；诊断在 diagnostics 中保留。
            assert!(!g.nodes.contains_key(&fileref(30, 1)));
            assert_eq!(g.diagnostics.unreachable_nodes, 1);
            // dir 20 不应有 30 作为子节点
            assert!(!g.children.contains_key(&fileref(20, 1)));
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
            assert!(g.nodes[&fileref(100 + DEPTH - 1, 1)].reachable_from_root);
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
                        mk_name(5, 5, "A"),     // 第一个有效名：A 是根的子目录
                        mk_name(20, 1, "self"), // 第二个有效名：自引用 = cycle
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

            // A 与 B 均不可达 — F1 已物理剔除
            assert!(!g.nodes.contains_key(&fileref(20, 1)));
            assert!(!g.nodes.contains_key(&fileref(21, 1)));
            // 诊断在 diagnostics 中保留
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
            // F1: orphan 已被剔除，诊断保留
            assert!(!g.nodes.contains_key(&fileref(20, 1)));
            assert_eq!(g.diagnostics.unreachable_nodes, 1);
            assert!(!g.children.contains_key(&fileref(5, 5)));
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
                    names: vec![mk_name(20, 1, "shared_in_a"), mk_name(21, 1, "shared_in_b")],
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
            assert!(g.children.contains_key(&fileref(20, 1)));
            assert_eq!(g.children.get(&fileref(20, 1)).unwrap().len(), 1);
            assert!(!g.children.contains_key(&fileref(21, 1)));
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

        // F4 强化：root_missing 状态下，即便 Phase 1a 已建非系统目录节点，
        // 返回的 graph 必须清空 nodes 与 children，调用方不会 panic。
        #[test]
        fn root_missing_with_non_system_dir_clears_nodes() {
            // record 5 不存在，record 20（合法目录）已建 —— 修复前 g.nodes[20] 仍存在。
            // 修复后 root_missing 状态下 nodes 为空，调用方无 panic 风险。
            let records = vec![
                mk_dir(20, 1, fileref(5, 5), "Users"),
                mk_dir(30, 1, fileref(20, 1), "alice"),
                mk_file(40, 1, fileref(30, 1), "f.txt", 999),
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            assert!(g.diagnostics.root_missing);
            assert!(g.nodes.is_empty(), "root_missing 时 nodes 必须清空");
            assert!(g.children.is_empty(), "root_missing 时 children 必须清空");
        }

        // F2 强化：unreachable orphan 节点的字节不进入可达父的 subtree_size_bytes。
        // 由于 first-parent-wins 设计使 children_index 仅含可达/已挂载边，
        // Phase 5 聚合时通过 `if let Some(...)` 显式跳过不在 aggregate 的 cycle/orphan 子
        // （防御性修复，对抗畸形 MFT 数据或未来放宽 first-parent-wins）。
        #[test]
        fn cycle_three_nodes_do_not_backflow_files_to_reachable_parent() {
            // A=22 -> B=30 -> C=33；C 还指向 A（duplicate_dir_entry）。
            // C 有直接文件 80 (500B)。所有节点经 first-parent-wins 都可达（root → ... → C）。
            // 验证：cycle 不在 children_index 引发实际灰点（first-parent-wins 防御），
            //       且 C 的 500B 进入 B/A/root 的聚合。
            let records = vec![
                root_record(),
                mk_dir(22, 1, fileref(5, 5), "A"),
                mk_dir(30, 1, fileref(22, 1), "B"),
                MftRecord {
                    id: fileref(33, 1),
                    base_record: None,
                    names: vec![
                        mk_name(30, 1, "C"),     // first parent: B
                        mk_name(22, 1, "C_dup"), // duplicate entry, points back to A
                    ],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
                mk_file(80, 1, fileref(33, 1), "big.bin", 500),
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            // 仅 duplicate_dir_entry (没有真实 cycle，因为 A 是 C 的第二 parent 而非第一)
            assert_eq!(g.diagnostics.duplicate_dir_entry, 1);
            assert_eq!(g.diagnostics.cycle_nodes, 0);
            // C 的 500B 正确沿 A→B→root 链逐级聚合
            let c_node = &g.nodes[&fileref(33, 1)];
            let b_node = &g.nodes[&fileref(30, 1)];
            let a_node = &g.nodes[&fileref(22, 1)];
            let root = &g.nodes[&fileref(5, 5)];
            assert_eq!(c_node.subtree_size_bytes, 500);
            assert_eq!(b_node.subtree_size_bytes, 500);
            assert_eq!(a_node.subtree_size_bytes, 500);
            assert_eq!(root.subtree_size_bytes, 500);
            // F1: 没有任何 orphan 残留
            assert_eq!(g.diagnostics.unreachable_nodes, 0);
        }

        // F3 强化：excluded 节点含非 excluded 子目录，子目录的字节必须计入 excluded_subtree_size_bytes。
        #[test]
        fn excluded_subtree_size_includes_nonexcluded_child_subdir_bytes() {
            // 5 -> 20(Users) -> 30(alice) [excluded] -> 31(docs) [NOT excluded] -> 40(file=1000)
            //              -> 32(pic) [also excluded, deep in excluded]
            // 期望：excluded_subtree_size_bytes 包含 docs 子目录的 1000 字节 + 30 自身 direct。
            let records = vec![
                root_record(),
                mk_dir(20, 1, fileref(5, 5), "Users"),
                mk_dir(30, 1, fileref(20, 1), "alice"), // excluded
                mk_dir(31, 1, fileref(30, 1), "docs"),  // NOT excluded
                mk_file(40, 1, fileref(31, 1), "file.txt", 1000),
                mk_dir(32, 1, fileref(30, 1), "pic"), // excluded (child of 30)
                mk_file(41, 1, fileref(32, 1), "img.png", 200),
                mk_file(50, 1, fileref(30, 1), "readme.md", 100), // 30 direct
                mk_file(60, 1, fileref(20, 1), "shared.bin", 333),
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &["C:\\Users\\alice".to_string()], 'C');

            assert!(g.nodes[&fileref(30, 1)].excluded);
            assert!(g.nodes[&fileref(31, 1)].excluded); // docs inherited via parent path
            assert!(g.nodes[&fileref(32, 1)].excluded);
            // 顶层 excluded = 30（parent 20 未 excluded）；32 是 30 的子，其字节已计入 30.subtree。
            // 30 subtree = 30 direct (100) + docs subtree (1000) + pic subtree (200) = 1300
            assert_eq!(
                g.excluded_subtree_size_bytes, 1300,
                "excluded_subtree_size_bytes 必须含顶层 excluded 节点 30 的全部子树字节（含 docs 非 excluded 子）"
            );
            // 20 与根不应含 alice 子树字节
            assert_eq!(g.nodes[&fileref(20, 1)].subtree_size_bytes, 333);
            assert_eq!(g.nodes[&fileref(5, 5)].subtree_size_bytes, 333);
        }

        // F5 强化：同份 index 跑多次，children 列表按记录号稳定排序（不依赖 HashMap 随机序）。
        #[test]
        fn sibling_order_deterministic_by_record_no() {
            // 根下挂 A=99, B=22, C=30, D=77 (顺序随机)——children 列表必须按 record_no 升序
            // 22 < 30 < 77 < 99（顺序确定）。record_no >= 16（避免 < 16 的 NTFS 元记录）。
            let records = vec![
                root_record(),
                mk_dir(99, 1, fileref(5, 5), "A"),
                mk_dir(22, 1, fileref(5, 5), "B"),
                mk_dir(30, 1, fileref(5, 5), "C"),
                mk_dir(77, 1, fileref(5, 5), "D"),
            ];
            let index = mk_index(records);

            // 多次跑结果一致（HashMap 顺序不影响稳定排序）
            let g1 = build_graph(&index, &[], 'C');
            let g2 = build_graph(&index, &[], 'C');
            let g3 = build_graph(&index, &[], 'C');

            let root_kids1 = &g1.children[&fileref(5, 5)];
            let root_kids2 = &g2.children[&fileref(5, 5)];
            let root_kids3 = &g3.children[&fileref(5, 5)];

            assert_eq!(root_kids1, root_kids2);
            assert_eq!(root_kids1, root_kids3);
            // 顺序必须按 record_no 升序：22, 30, 77, 99
            assert_eq!(
                root_kids1,
                &vec![
                    fileref(22, 1),
                    fileref(30, 1),
                    fileref(77, 1),
                    fileref(99, 1),
                ]
            );
        }

        // F10 强化：父记录存在但非目录应计入 non_dir_parent_* 诊断（不再静默）。
        #[test]
        fn non_dir_parent_diagnostics_are_recorded() {
            // record 20 = 文件（非目录），但被目录 record 30 与文件 record 40 当作父引用。
            // sequence 必须匹配（否则走 stale_sequence 路径），且 record 20 在 records 中。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(20, 1),
                    base_record: None,
                    names: vec![],
                    logical_size: 0,
                    is_dir: false,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
                MftRecord {
                    id: fileref(30, 1),
                    base_record: None,
                    names: vec![mk_name(20, 1, "ghost_dir")],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
                MftRecord {
                    id: fileref(40, 1),
                    base_record: None,
                    names: vec![mk_name(20, 1, "ghost_file")],
                    logical_size: 999,
                    is_dir: false,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            // 目录 30 命中 non_dir_parent_dirs；文件 40 命中 non_dir_parent_files
            assert_eq!(g.diagnostics.non_dir_parent_dirs, 1);
            assert_eq!(g.diagnostics.non_dir_parent_files, 1);
        }

        // F8/F9 强化：cycle_nodes 在多重 reverse edge 与多重自引用下不被重复计入。
        #[test]
        fn cycle_nodes_deduped_across_multiple_reverse_edges() {
            // root -> A(22); A 有三个子 B(30)/C(33)/D(40)，其中 B/C/D 的多条 name 反复
            // 回指 A。在 first-parent-wins 下，这些回指都进入 duplicate_dir_entry，不构成
            // 真实 DFS 灰点回访。但 F8 的实现保证了即便有真正的灰点命中，记录级别去重。
            // 这里用 self-ref 路径触发多次 cycle_nodes（验证 F9 修复路径）。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(22, 1),
                    base_record: None,
                    names: vec![
                        mk_name(5, 5, "A"),      // first parent: root (valid)
                        mk_name(22, 1, "self1"), // first self-ref
                        mk_name(22, 1, "self2"), // second self-ref（应被 F9 去重）
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

            // F9: 同一 record 多个自指有效名仅计一次
            assert_eq!(
                g.diagnostics.cycle_nodes, 1,
                "单 record 多个自指有效名仅在 record 级首次计入一次"
            );
            // A 是可达的（first parent = root），不应被 F1 剔除
            assert!(g.nodes.contains_key(&fileref(22, 1)));
        }

        // F9 强化：单 record 多自指有效名仅在 record 级首次计入一次。
        #[test]
        fn cycle_nodes_self_ref_counted_once_per_record() {
            // D 的全部有效名都自指（3 个自指有效名）。
            // F9 修法：cycle_nodes = 1（record 级去重）。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(30, 1),
                    base_record: None,
                    names: vec![
                        mk_name(30, 1, "self1"),
                        mk_name(30, 1, "self2"),
                        mk_name(30, 1, "self3"),
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

            // 三个自指有效名应只计一次
            assert_eq!(g.diagnostics.cycle_nodes, 1);
        }

        // F6 强化：诊断不再含 orphan_entries 字段（编译期即可证明）。
        #[test]
        fn orphan_entries_field_removed() {
            // 类型级验证：访问 orphan_entries 字段必须编译失败。
            // 这里仅断言 diagnostics 已无 orphan_entries 字段引用（运行时不可观测）。
            // 通过 build_graph 行为间接保证：count 字段为 unreachable_nodes，
            // 不再有与 unreachable_nodes 等价的别名。
            let records = vec![
                root_record(),
                MftRecord {
                    id: fileref(20, 1),
                    base_record: None,
                    names: vec![mk_name(99, 1, "orphan_dir")],
                    logical_size: 0,
                    is_dir: true,
                    in_use: true,
                    reparse_tag: None,
                    has_nonresident_attr_list: false,
                },
            ];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');

            // orphan 不再被双计：unreachable_nodes 仅反映真实不可达数（剔除的 orphan 数）。
            assert_eq!(g.diagnostics.unreachable_nodes, 1);
        }

        // F7 强化：DirectoryNode 不再有 is_reparse 字段（编译期保证）。
        // 通过 build_graph 行为间接验证：reparse_tag 为 None 的节点，反向读取即得到 None。
        #[test]
        fn directory_node_reparse_tag_only_no_is_reparse() {
            let mut rec = root_record();
            rec.reparse_tag = Some(0xA0000001); // IO_REPARSE_TAG_MOUNT_POINT
            let records = vec![rec];
            let index = mk_index(records);
            let g = build_graph(&index, &[], 'C');
            assert_eq!(
                g.nodes[&fileref(5, 5)].reparse_tag,
                Some(0xA0000001),
                "reparse_tag 字段独立保留 is_reparse 行为"
            );
        }

        #[test]
        fn is_path_excluded_matches_scancontext_semantics() {
            let exclude = vec!["c:\\users\\alice".to_string(), "c:\\windows".to_string()];
            // 完全相等
            assert!(is_path_excluded("c:\\users\\alice", &exclude));
            // 前缀 + 路径边界
            assert!(is_path_excluded("c:\\users\\alice\\docs", &exclude));
            // 前缀但不越过路径边界（应不匹配）
            assert!(!is_path_excluded("c:\\users\\alice-backup", &exclude));
            // 不匹配
            assert!(!is_path_excluded("c:\\program files", &exclude));
        }
    }

    // ===== T5：树状态标注、可见性与根汇总测试 =====
    mod annotation {
        use super::*;
        use crate::models::{PresetCategory, ScanConfig};
        use std::collections::HashMap;

        pub(super) fn fileref(record_no: u64, sequence: u16) -> FileRef {
            FileRef {
                record_no,
                sequence,
            }
        }

        pub(super) fn node(
            record_no: u64,
            path: &str,
            depth: u32,
            subtree_size_bytes: u64,
            reparse_tag: Option<u32>,
            excluded: bool,
        ) -> DirectoryNode {
            DirectoryNode {
                file_ref: fileref(record_no, 1),
                path: path.into(),
                display_name: path.rsplit('\\').next().unwrap_or(path).into(),
                depth,
                reparse_tag,
                is_junction: false,
                direct_file_size_bytes: 0,
                direct_file_count: 0,
                direct_dir_count: 0,
                subtree_size_bytes,
                subtree_file_count: 0,
                subtree_dir_count: 1,
                excluded,
                reachable_from_root: true,
                matched_preset: None,
                category: None,
                auto_migrate: false,
                access_state: AccessState::Unknown,
                scan_status: None,
                migration_id: None,
                linked_target_size_bytes: None,
                visible: false,
            }
        }

        pub(super) fn make_graph(
            nodes: Vec<DirectoryNode>,
            edges: &[(u64, u64)],
            root_record_no: u64,
        ) -> DirectoryGraph {
            let mut node_map = HashMap::new();
            for node in nodes {
                node_map.insert(node.file_ref, node);
            }
            let mut children: HashMap<FileRef, Vec<FileRef>> = HashMap::new();
            for &(parent, child) in edges {
                children
                    .entry(fileref(parent, 1))
                    .or_default()
                    .push(fileref(child, 1));
            }
            DirectoryGraph {
                nodes: node_map,
                root: fileref(root_record_no, 1),
                children,
                system_metadata_size_bytes: 0,
                diagnostics: GraphDiagnostics::default(),
                excluded_subtree_size_bytes: 0,
            }
        }

        pub(super) fn config(min_size_mb: u64, presets: Vec<Preset>) -> Config {
            Config {
                schema_version: 1,
                repository: "D:\\repo".into(),
                scan: ScanConfig {
                    min_size_mb,
                    exclude_paths: Vec::new(),
                },
                presets,
            }
        }

        pub(super) fn preset(path: &str) -> Preset {
            Preset {
                id: "cache".into(),
                name: "Cache".into(),
                category: PresetCategory::DevCache,
                match_paths: vec![path.into()],
                match_processes: Vec::new(),
                auto_migrate: true,
                target_subdir: "cache".into(),
            }
        }

        pub(super) fn migration(
            id: &str,
            source: &str,
            target: &Path,
            status: MigrationStatus,
        ) -> Migration {
            Migration {
                id: id.into(),
                schema_version: 1,
                source: source.into(),
                target: target.to_string_lossy().into(),
                old_path: String::new(),
                preset: None,
                created_at: "2026-07-20T00:00:00Z".into(),
                status,
                source_volume_serial: "C".into(),
                target_volume_serial: "D".into(),
                recycle_bin_ref: String::new(),
                pending_cleanup: None,
            }
        }

        #[test]
        fn annotation_marks_preset_junction_and_target_size_without_changing_occupancy() {
            let target = tempfile::tempdir().unwrap();
            let path = "C:\\Users\\alice\\Cache";
            let mut graph = make_graph(
                vec![
                    node(5, "C:\\", 0, 4096, None, false),
                    node(20, path, 1, 4096, Some(0xA0000003), false),
                ],
                &[(5, 20)],
                5,
            );
            let cfg = config(1, vec![preset(path)]);
            let migrations = vec![migration(
                "m1",
                path,
                target.path(),
                MigrationStatus::Active,
            )];
            annotate_graph_with_callbacks(
                &mut graph,
                &cfg,
                &migrations,
                &|_| true,
                &|_| true,
                &|_| 777,
                0,
            );

            let annotated = &graph.nodes[&fileref(20, 1)];
            assert_eq!(annotated.matched_preset.as_deref(), Some("cache"));
            assert_eq!(annotated.category, Some(PresetCategory::DevCache));
            assert!(annotated.auto_migrate);
            assert!(annotated.is_junction);
            assert_eq!(annotated.scan_status, Some(ScanItemStatus::Migrated));
            assert_eq!(annotated.migration_id.as_deref(), Some("m1"));
            assert_eq!(annotated.linked_target_size_bytes, Some(777));
            assert_eq!(annotated.subtree_size_bytes, 4096);
        }

        #[test]
        fn annotation_status_priority_is_broken_then_pending_then_migrated_then_existing() {
            let target = tempfile::tempdir().unwrap();
            let path = "C:\\data";
            let cfg = config(0, Vec::new());

            let statuses = vec![
                MigrationStatus::Active,
                MigrationStatus::PendingManualConfirm,
                MigrationStatus::OldPendingDelete,
            ];
            let migrations = statuses
                .into_iter()
                .enumerate()
                .map(|(i, status)| migration(&format!("m{i}"), path, target.path(), status))
                .collect::<Vec<_>>();
            let mut broken = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, path, 1, 0, Some(1), false),
                ],
                &[(5, 20)],
                5,
            );
            annotate_graph_with_callbacks(
                &mut broken,
                &cfg,
                &migrations,
                &|_| true,
                &|_| false,
                &|_| 0,
                0,
            );
            assert_eq!(
                broken.nodes[&fileref(20, 1)].scan_status,
                Some(ScanItemStatus::LinkBroken)
            );

            let pending = vec![migration(
                "pending",
                path,
                target.path(),
                MigrationStatus::PendingManualConfirm,
            )];
            let mut graph_pending = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, path, 1, 0, Some(1), false),
                ],
                &[(5, 20)],
                5,
            );
            annotate_graph_with_callbacks(
                &mut graph_pending,
                &cfg,
                &pending,
                &|_| true,
                &|_| true,
                &|_| 0,
                0,
            );
            assert_eq!(
                graph_pending.nodes[&fileref(20, 1)].scan_status,
                Some(ScanItemStatus::MigrationPending)
            );

            let active = vec![migration(
                "active",
                path,
                target.path(),
                MigrationStatus::Active,
            )];
            let mut graph_active = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, path, 1, 0, Some(1), false),
                ],
                &[(5, 20)],
                5,
            );
            annotate_graph_with_callbacks(
                &mut graph_active,
                &cfg,
                &active,
                &|_| true,
                &|_| true,
                &|_| 0,
                0,
            );
            assert_eq!(
                graph_active.nodes[&fileref(20, 1)].scan_status,
                Some(ScanItemStatus::Migrated)
            );

            let mut existing = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, path, 1, 0, Some(1), false),
                ],
                &[(5, 20)],
                5,
            );
            annotate_graph_with_callbacks(
                &mut existing,
                &cfg,
                &[],
                &|_| true,
                &|_| true,
                &|_| 0,
                0,
            );
            assert_eq!(
                existing.nodes[&fileref(20, 1)].scan_status,
                Some(ScanItemStatus::ExistingLink)
            );
        }

        #[test]
        fn annotation_reparse_without_confirmed_junction_is_not_existing_link() {
            let path = "C:\\not-a-junction";
            let mut graph = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, path, 1, 0, Some(1), false),
                ],
                &[(5, 20)],
                5,
            );
            annotate_graph_with_callbacks(
                &mut graph,
                &config(0, Vec::new()),
                &[],
                &|_| false,
                &|_| true,
                &|_| 0,
                0,
            );
            assert!(!graph.nodes[&fileref(20, 1)].is_junction);
            assert_eq!(graph.nodes[&fileref(20, 1)].scan_status, None);
        }

        #[test]
        fn annotation_propagates_migrated_before_link_and_stops_on_cycles() {
            let target = tempfile::tempdir().unwrap();
            let mut graph = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, "C:\\parent", 1, 0, None, false),
                    node(30, "C:\\parent\\migrated", 2, 0, None, false),
                    node(40, "C:\\parent\\link", 2, 0, Some(1), false),
                ],
                &[(5, 20), (20, 30), (20, 40), (30, 20)],
                5,
            );
            let migrations = vec![migration(
                "m1",
                "C:\\parent\\migrated",
                target.path(),
                MigrationStatus::Active,
            )];
            annotate_graph_with_callbacks(
                &mut graph,
                &config(0, Vec::new()),
                &migrations,
                &|_| true,
                &|_| true,
                &|_| 0,
                0,
            );
            assert_eq!(
                graph.nodes[&fileref(30, 1)].scan_status,
                Some(ScanItemStatus::Migrated)
            );
            assert_eq!(
                graph.nodes[&fileref(40, 1)].scan_status,
                Some(ScanItemStatus::ExistingLink)
            );
            assert_eq!(
                graph.nodes[&fileref(20, 1)].scan_status,
                Some(ScanItemStatus::ContainsMigrated)
            );
            assert_eq!(
                graph.nodes[&fileref(5, 1)].scan_status,
                Some(ScanItemStatus::ContainsMigrated)
            );
        }
    }

    mod visibility {
        use super::annotation::*;
        use super::*;

        #[test]
        fn visibility_uses_subtree_size_and_forced_ancestor_chain() {
            let mut graph = make_graph(
                vec![
                    node(5, "C:\\", 0, 1, None, false),
                    node(20, "C:\\big", 1, 1024 * 1024, None, false),
                    node(30, "C:\\big\\small", 2, 1, None, false),
                    node(40, "C:\\preset", 1, 1, None, false),
                    node(50, "C:\\inaccessible", 1, 1, None, false),
                    node(60, "C:\\hidden", 1, 1, None, false),
                ],
                &[(5, 20), (20, 30), (5, 40), (5, 50), (5, 60)],
                5,
            );
            graph.nodes.get_mut(&fileref(50, 1)).unwrap().access_state = AccessState::Inaccessible;
            let mut preset = preset("C:\\preset");
            preset.id = "preset".into();
            annotate_graph_with_callbacks(
                &mut graph,
                &config(1, vec![preset]),
                &[],
                &|_| false,
                &|_| true,
                &|_| 0,
                0,
            );
            assert!(graph.nodes[&fileref(5, 1)].visible); // navigation ancestor
            assert!(graph.nodes[&fileref(20, 1)].visible); // large directory
            assert!(!graph.nodes[&fileref(30, 1)].visible); // ordinary small descendant
            assert!(graph.nodes[&fileref(40, 1)].visible); // preset
            assert!(graph.nodes[&fileref(50, 1)].visible); // inaccessible
            assert!(!graph.nodes[&fileref(60, 1)].visible);
        }

        #[test]
        fn visibility_min_zero_shows_normal_nodes_but_never_excluded_nodes() {
            let mut graph = make_graph(
                vec![
                    node(5, "C:\\", 0, 0, None, false),
                    node(20, "C:\\normal", 1, 0, None, false),
                    node(30, "C:\\excluded", 1, u64::MAX, None, true),
                ],
                &[(5, 20), (5, 30)],
                5,
            );
            annotate_graph_with_callbacks(
                &mut graph,
                &config(0, Vec::new()),
                &[],
                &|_| false,
                &|_| true,
                &|_| 0,
                0,
            );
            assert!(graph.nodes[&fileref(5, 1)].visible);
            assert!(graph.nodes[&fileref(20, 1)].visible);
            assert!(!graph.nodes[&fileref(30, 1)].visible);
        }
    }

    mod root_summary {
        use super::annotation::*;
        use super::*;

        #[test]
        fn root_summary_uses_actual_root_ref_and_saturates_total() {
            let root = fileref(5, 42);
            let mut root_node = node(5, "C:\\", 0, 0, None, false);
            root_node.file_ref = root;
            root_node.direct_file_size_bytes = u64::MAX - 3;
            root_node.direct_file_count = 7;
            let mut graph = make_graph(vec![root_node], &[], 5);
            graph.root = root;
            graph.system_metadata_size_bytes = 10;

            let summary = build_root_summary(&graph, 2);
            assert_eq!(summary.direct_file_size_bytes, u64::MAX - 3);
            assert_eq!(summary.direct_file_count, 7);
            assert_eq!(summary.system_metadata_size_bytes, Some(10));
            assert_eq!(summary.total_known_size_bytes, u64::MAX);
            assert!(summary.incomplete);
        }

        #[test]
        fn root_summary_is_complete_when_no_mft_records_are_skipped() {
            let mut root_node = node(5, "C:\\", 0, 123, None, false);
            root_node.direct_file_size_bytes = 123;
            root_node.direct_file_count = 1;
            let graph = make_graph(vec![root_node], &[], 5);
            let summary = build_root_summary(&graph, 0);
            assert_eq!(summary.system_metadata_size_bytes, Some(0));
            assert!(!summary.incomplete);
            assert_eq!(summary.total_known_size_bytes, 123);
        }
    }

    // ===== T6：materialize 与 TreeStore 分页/reveal 测试 =====
    mod materialize_tests {
        use super::*;
        use crate::models::{RootFileSummary, ScanSource};
        use std::collections::HashMap;

        fn fr(no: u64) -> FileRef {
            FileRef {
                record_no: no,
                sequence: 1,
            }
        }

        /// 构造一个 visible 目录节点（其余字段取默认/零值）。
        fn vnode(no: u64, path: &str, depth: u32, size: u64) -> DirectoryNode {
            DirectoryNode {
                file_ref: fr(no),
                path: path.into(),
                display_name: path.rsplit('\\').next().unwrap_or(path).into(),
                depth,
                reparse_tag: None,
                is_junction: false,
                direct_file_size_bytes: 0,
                direct_file_count: 0,
                direct_dir_count: 0,
                subtree_size_bytes: size,
                subtree_file_count: 0,
                subtree_dir_count: 1,
                excluded: false,
                reachable_from_root: true,
                matched_preset: None,
                category: None,
                auto_migrate: false,
                access_state: AccessState::Unknown,
                scan_status: None,
                migration_id: None,
                linked_target_size_bytes: None,
                visible: true,
            }
        }

        fn graph(nodes: Vec<DirectoryNode>, edges: &[(u64, u64)]) -> DirectoryGraph {
            let mut nm = HashMap::new();
            for n in nodes {
                nm.insert(n.file_ref, n);
            }
            let mut children: HashMap<FileRef, Vec<FileRef>> = HashMap::new();
            for &(p, c) in edges {
                children.entry(fr(p)).or_default().push(fr(c));
            }
            DirectoryGraph {
                nodes: nm,
                root: fr(5),
                children,
                system_metadata_size_bytes: 0,
                diagnostics: GraphDiagnostics::default(),
                excluded_subtree_size_bytes: 0,
            }
        }

        fn empty_summary() -> RootFileSummary {
            RootFileSummary {
                direct_file_size_bytes: 0,
                direct_file_count: 0,
                system_metadata_size_bytes: None,
                total_known_size_bytes: 0,
                incomplete: false,
            }
        }

        #[test]
        fn materialize_filters_invisible_nodes() {
            let mut b = vnode(11, r"C:\B", 1, 200);
            b.visible = false;
            let g = graph(vec![vnode(10, r"C:\A", 1, 100), b], &[(5, 10), (5, 11)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            assert!(store.node(r"C:\A").is_some());
            assert!(
                store.node(r"C:\B").is_none(),
                "不可见节点不应进入 TreeStore"
            );
            let roots: Vec<String> = store.roots().into_iter().map(|n| n.path).collect();
            assert_eq!(roots, vec![r"C:\A".to_string()]);
        }

        #[test]
        fn materialize_size_descending_with_path_tiebreak() {
            let d = vnode(20, r"C:\D", 1, 200);
            let a = vnode(21, r"C:\A", 1, 100);
            let b = vnode(22, r"C:\B", 1, 100);
            let c = vnode(23, r"C:\C", 1, 100);
            let g = graph(vec![d, a, b, c], &[(5, 20), (5, 21), (5, 22), (5, 23)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let roots: Vec<String> = store.roots().into_iter().map(|n| n.path).collect();
            // size 降序：D(200) 第一；A/B/C 同 100，按规范化 path 升序
            assert_eq!(
                roots,
                vec![
                    r"C:\D".to_string(),
                    r"C:\A".to_string(),
                    r"C:\B".to_string(),
                    r"C:\C".to_string()
                ]
            );
        }

        #[test]
        fn materialize_child_count_and_filtered_count() {
            let a = vnode(30, r"C:\A", 1, 500);
            let a1 = vnode(31, r"C:\A\A1", 2, 100);
            let mut a2 = vnode(32, r"C:\A\A2", 2, 50);
            a2.visible = false; // filtered：非排除、不可见
            let mut a3 = vnode(33, r"C:\A\A3", 2, 50);
            a3.visible = false;
            a3.excluded = true; // excluded：不计入 filtered
            let g = graph(
                vec![a, a1, a2, a3],
                &[(5, 30), (30, 31), (30, 32), (30, 33)],
            );
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let node_a = store.node(r"C:\A").unwrap();
            assert_eq!(node_a.child_count, 1, "只有 A1 可见");
            assert_eq!(node_a.filtered_child_count, 1, "只有 A2 是非排除不可见");
        }

        #[test]
        fn materialize_recommended_eligibility() {
            let mut a = vnode(40, r"C:\A", 1, 100);
            a.matched_preset = Some("p1".into());
            let mut b = vnode(41, r"C:\B", 1, 100);
            b.matched_preset = Some("p1".into());
            b.scan_status = Some(ScanItemStatus::Migrated);
            let mut c = vnode(42, r"C:\C", 1, 100);
            c.matched_preset = Some("p1".into());
            c.reparse_tag = Some(0xA0000003);
            let mut d = vnode(43, r"C:\D", 1, 100);
            d.matched_preset = Some("p1".into());
            d.access_state = AccessState::Inaccessible;
            let e = vnode(44, r"C:\E", 1, 100); // 无预设
            let g = graph(
                vec![a, b, c, d, e],
                &[(5, 40), (5, 41), (5, 42), (5, 43), (5, 44)],
            );
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let rec: Vec<String> = store.recommended().into_iter().map(|n| n.path).collect();
            assert_eq!(rec, vec![r"C:\A".to_string()], "只有 A 满足全部推荐条件");
        }

        #[test]
        fn materialize_roots_and_filtered_root_count() {
            let a = vnode(50, r"C:\A", 1, 100);
            let mut b = vnode(51, r"C:\B", 1, 50);
            b.visible = false; // filtered
            let mut c = vnode(52, r"C:\C", 1, 50);
            c.visible = false;
            c.excluded = true;
            let g = graph(vec![a, b, c], &[(5, 50), (5, 51), (5, 52)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let roots: Vec<String> = store.roots().into_iter().map(|n| n.path).collect();
            assert_eq!(roots, vec![r"C:\A".to_string()]);
            assert_eq!(
                store.filtered_root_count(),
                1,
                "只有 B 是非排除不可见的一级子"
            );
        }

        #[test]
        fn children_page_limit_clamp() {
            let mut nodes = Vec::new();
            let mut edges = Vec::new();
            for i in 0..501u64 {
                let path = format!(r"C:\N{}", i);
                nodes.push(vnode(100 + i, &path, 1, 1000 - i));
                edges.push((5, 100 + i));
            }
            let g = graph(nodes, &edges);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let p1 = store.children_page(r"C:\", 0, 0);
            assert_eq!(p1.items.len(), 1, "limit=0 clamp 到 1");
            let p500 = store.children_page(r"C:\", 0, 1000);
            assert_eq!(p500.items.len(), 500, "limit=1000 clamp 到 500");
            assert_eq!(p500.total, 501);
            assert_eq!(p500.next_offset, Some(500));
        }

        #[test]
        fn children_page_offset_boundary() {
            let a = vnode(60, r"C:\A", 1, 300);
            let b = vnode(61, r"C:\B", 1, 200);
            let c = vnode(62, r"C:\C", 1, 100);
            let g = graph(vec![a, b, c], &[(5, 60), (5, 61), (5, 62)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let over = store.children_page(r"C:\", 3, 10);
            assert_eq!(over.items.len(), 0);
            assert_eq!(over.total, 3);
            assert_eq!(over.next_offset, None);
            let mid = store.children_page(r"C:\", 1, 1);
            assert_eq!(mid.items.len(), 1);
            assert_eq!(mid.items[0].path, r"C:\B"); // size 降序 A,B,C -> offset1 是 B
            assert_eq!(mid.next_offset, Some(2));
        }

        #[test]
        fn reveal_pages_cross_page_chain() {
            let a = vnode(70, r"C:\A", 1, 300);
            let b = vnode(71, r"C:\A\B", 2, 200);
            let t = vnode(72, r"C:\A\B\Target", 3, 100);
            let g = graph(vec![a, b, t], &[(5, 70), (70, 71), (71, 72)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let levels = store.reveal_pages(r"C:\A\B\Target", 100).unwrap();
            assert_eq!(levels.len(), 3);
            assert_eq!(levels[0].parent_path, r"c:\");
            assert!(levels[0].page.items.iter().any(|n| n.path == r"C:\A"));
            assert_eq!(levels[1].parent_path, r"C:\A");
            assert!(levels[1].page.items.iter().any(|n| n.path == r"C:\A\B"));
            assert_eq!(levels[2].parent_path, r"C:\A\B");
            assert!(levels[2]
                .page
                .items
                .iter()
                .any(|n| n.path == r"C:\A\B\Target"));
        }

        #[test]
        fn reveal_pages_missing_target_returns_error() {
            let a = vnode(80, r"C:\A", 1, 100);
            let g = graph(vec![a], &[(5, 80)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            assert!(store.reveal_pages(r"C:\Nope", 100).is_err());
        }

        #[test]
        fn reveal_pages_locates_target_off_first_page() {
            // 250 个一级子，size 递减（1000..750）确保按 size 降序后 index 与创建顺序一致。
            // 目标 = 第 200 个子（index 199）。limit=100 -> page_offset=100（第二页 index 100..199）。
            let mut nodes = Vec::new();
            let mut edges = Vec::new();
            for i in 0..250u64 {
                let path = format!(r"C:\D{}", i);
                nodes.push(vnode(200 + i, &path, 1, 1000 - i));
                edges.push((5, 200 + i));
            }
            let target_path = format!(r"C:\D{}", 199);
            let g = graph(nodes, &edges);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let levels = store.reveal_pages(&target_path, 100).unwrap();
            assert_eq!(levels.len(), 1, "一级目标只有一层");
            assert_eq!(levels[0].parent_path, r"c:\");
            assert_eq!(levels[0].page.items.len(), 100);
            assert!(
                levels[0].page.items.iter().any(|n| n.path == target_path),
                "目标应在其所在页内被定位"
            );
            assert!(
                !levels[0].page.items.iter().any(|n| n.path == r"C:\D0"),
                "第一页首个不应出现在目标所在页"
            );
        }

        #[test]
        fn children_page_limit_clamp_preserves_total() {
            // 3 个子；limit 极大被 clamp 到 500，但 total 仍为实际子数 3。
            let a = vnode(90, r"C:\A", 1, 300);
            let b = vnode(91, r"C:\B", 1, 200);
            let c = vnode(92, r"C:\C", 1, 100);
            let g = graph(vec![a, b, c], &[(5, 90), (5, 91), (5, 92)]);
            let store = materialize(&g, empty_summary(), ScanSource::Mft, "s1".into());
            let page = store.children_page(r"C:\", 0, 10000);
            assert_eq!(page.items.len(), 3, "clamp 到 500 但不足 500 返回全部");
            assert_eq!(page.total, 3);
            assert_eq!(page.next_offset, None);
            // limit=0 clamp 到 1，total 不受影响
            let page1 = store.children_page(r"C:\", 0, 0);
            assert_eq!(page1.items.len(), 1);
            assert_eq!(page1.total, 3, "total 不受 limit clamp 影响");
        }
    }

    // ===== T9：有界并发 filesystem 降级扫描测试 =====
    mod fs_fallback {
        use super::*;
        use crate::models::{ScanConfig, ScanSource};
        use std::collections::HashMap;

        #[allow(clippy::type_complexity)]
        type MockTree = HashMap<String, Vec<FsEntryResult>>;

        struct MockFsReader {
            dirs: MockTree,
            per_call_delay: Duration,
        }

        impl MockFsReader {
            fn new(dirs: MockTree) -> Self {
                Self {
                    dirs,
                    per_call_delay: Duration::ZERO,
                }
            }

            fn with_delay(dirs: MockTree, per_call_delay: Duration) -> Self {
                Self {
                    dirs,
                    per_call_delay,
                }
            }
        }

        impl FsReader for MockFsReader {
            fn read_dir(&self, path: &str) -> Result<Vec<FsEntryResult>, FsEntryError> {
                if !self.per_call_delay.is_zero() {
                    std::thread::sleep(self.per_call_delay);
                }
                match self.dirs.get(&normalize(path)).cloned() {
                    Some(entries) if entries.len() == 1 => match &entries[0] {
                        // 测试约定：单条 Err((String::new(), AccessDenied)) 表示整目录访问被拒。
                        Err((name, FsEntryError::AccessDenied)) if name.is_empty() => {
                            Err(FsEntryError::AccessDenied)
                        }
                        _ => Ok(entries),
                    },
                    Some(entries) => Ok(entries),
                    None => Err(FsEntryError::Io {
                        message: format!("not found: {}", path),
                    }),
                }
            }
        }

        fn entry(name: &str, is_dir: bool, size: u64) -> FsEntryResult {
            Ok(FsEntry {
                name: name.into(),
                is_dir,
                file_size: size,
                reparse_tag: None,
            })
        }

        fn reparse_dir(name: &str, tag: u32) -> FsEntryResult {
            Ok(FsEntry {
                name: name.into(),
                is_dir: true,
                file_size: 0,
                reparse_tag: Some(tag),
            })
        }

        fn err_entry(name: &str, err: FsEntryError) -> FsEntryResult {
            Err((name.into(), err))
        }

        fn tree() -> MockTree {
            let mut dirs = HashMap::new();
            dirs.insert(
                normalize("C:\\"),
                vec![
                    entry("pagefile.sys", false, 1024),
                    entry("Users", true, 0),
                    entry("ProgramData", true, 0),
                ],
            );
            dirs.insert(
                normalize("C:\\Users"),
                vec![entry("alice", true, 0), entry("bob", true, 0)],
            );
            dirs.insert(
                normalize("C:\\Users\\alice"),
                vec![
                    entry("docs", true, 0),
                    entry("a.txt", false, 100),
                    entry("b.txt", false, 200),
                ],
            );
            dirs.insert(
                normalize("C:\\Users\\alice\\docs"),
                vec![entry("c.txt", false, 300)],
            );
            dirs.insert(
                normalize("C:\\Users\\bob"),
                vec![entry("d.txt", false, 400)],
            );
            dirs.insert(normalize("C:\\ProgramData"), vec![]);
            dirs
        }

        fn run_coordinator(
            dirs: MockTree,
            excluded_paths: &[String],
            worker_count: usize,
        ) -> (DirectoryGraph, u64, u64) {
            let reader: Arc<dyn FsReader> = Arc::new(MockFsReader::new(dirs));
            coordinator_run(
                reader,
                'C',
                excluded_paths,
                worker_count,
                Arc::new(AtomicBool::new(false)),
                Arc::new(|_| {}),
            )
            .unwrap()
        }

        fn run_fs_scan(dirs: MockTree, excluded_paths: &[String]) -> ScanOutcome {
            let engine = RealScanEngine;
            let cfg = Config {
                schema_version: 1,
                repository: "D:\\repo".into(),
                scan: ScanConfig {
                    min_size_mb: 0,
                    exclude_paths: excluded_paths.to_vec(),
                },
                presets: Vec::new(),
            };
            engine
                .fs_scan_with_reader(
                    'C',
                    &cfg,
                    &[],
                    excluded_paths,
                    Arc::new(AtomicBool::new(false)),
                    Arc::new(|_| {}),
                    Arc::new(MockFsReader::new(dirs)),
                    &|_| DriveKind::Fixed,
                )
                .unwrap()
        }

        #[test]
        fn fs_scan_produces_real_tree_matching_mft_contract() {
            let (graph, _, _) = run_coordinator(tree(), &[], 2);
            assert!(graph.nodes.len() > 1);
            assert!(!graph.children.is_empty());

            let root = graph.nodes.get(&graph.root).unwrap();
            assert_eq!(root.path, "C:\\");
            assert_eq!(root.display_name, "C");

            let users_norm = normalize("C:\\Users");
            let users_ref = graph
                .nodes
                .iter()
                .find(|(_, n)| normalize(&n.path) == users_norm)
                .map(|(r, _)| *r)
                .unwrap();
            assert!(graph
                .children
                .get(&graph.root)
                .unwrap()
                .contains(&users_ref));
        }

        #[test]
        fn root_id_comes_from_actual_insertion() {
            let (graph, _, _) = run_coordinator(tree(), &[], 2);
            let root = graph.nodes.get(&graph.root).unwrap();
            assert_eq!(root.path, "C:\\");
            // 根 id 不是硬编码的占位值，而是 coordinator 插入根节点后从 nodes 读取的 hash。
            assert_eq!(graph.root.record_no, stable_hash_path(&normalize("C:\\")));
        }

        #[test]
        fn file_count_accumulated_globally() {
            let (graph, scanned_files, _) = run_coordinator(tree(), &[], 2);
            assert_eq!(scanned_files, 5); // pagefile + a + b + c + d
            let root = graph.nodes.get(&graph.root).unwrap();
            assert_eq!(root.direct_file_count, 1); // pagefile.sys
            assert_eq!(root.subtree_file_count, 5);
        }

        #[test]
        fn excluded_subtree_marked() {
            let excluded = vec!["C:\\Users\\alice".to_string()];
            let (graph, _, _) = run_coordinator(tree(), &excluded, 2);
            let alice_norm = normalize("C:\\Users\\alice");
            let alice = graph
                .nodes
                .values()
                .find(|n| normalize(&n.path) == alice_norm)
                .unwrap();
            assert!(alice.excluded);
            assert!(!alice.visible);

            let users_norm = normalize("C:\\Users");
            let users = graph
                .nodes
                .values()
                .find(|n| normalize(&n.path) == users_norm)
                .unwrap();
            // alice 子树不计入 Users 祖先
            assert_eq!(users.subtree_file_count, 1); // bob/d.txt
        }

        #[test]
        fn reparse_does_not_descend_into_target() {
            let mut dirs = tree();
            // 在根目录下挂一个 reparse point。worker 对每个 entry 处理：发现 reparse_tag 后
            // 仍入图但不把它的 entry name 当作普通子目录读 target。
            let root_entries = dirs.get_mut(&normalize("C:\\")).unwrap();
            root_entries.push(reparse_dir("junction", 0xA0000003));
            // 模拟 reparse target 内部的内容目录，确认不会被读取。
            dirs.insert(
                normalize("C:\\junction"),
                vec![entry("inside.txt", false, 9999)],
            );

            let (graph, scanned_files, _) = run_coordinator(dirs, &[], 2);
            let junction_norm = normalize("C:\\junction");
            let junction = graph
                .nodes
                .values()
                .find(|n| normalize(&n.path) == junction_norm);
            assert!(junction.is_some(), "reparse 点本身应入图");
            let junction = junction.unwrap();
            assert_eq!(junction.reparse_tag, Some(0xA0000003));
            // 未下钻 target，inside.txt 不入图、不计入 files。
            assert!(!graph
                .nodes
                .values()
                .any(|n| n.path == "C:\\junction\\inside.txt"));
            assert_eq!(scanned_files, 5);
        }

        #[test]
        fn access_denied_marked_inaccessible() {
            let mut dirs = tree();
            dirs.insert(
                normalize("C:\\Users\\bob"),
                vec![Err((String::new(), FsEntryError::AccessDenied))],
            );
            let (graph, _, _) = run_coordinator(dirs, &[], 2);
            let bob_norm = normalize("C:\\Users\\bob");
            let bob = graph
                .nodes
                .values()
                .find(|n| normalize(&n.path) == bob_norm)
                .unwrap();
            assert_eq!(bob.access_state, AccessState::Inaccessible);
            assert_eq!(bob.direct_dir_count, 0);
        }

        #[test]
        fn entry_error_not_silently_accessible() {
            let mut dirs = tree();
            dirs.get_mut(&normalize("C:\\")).unwrap().push(err_entry(
                "badfile.dll",
                FsEntryError::Io {
                    message: "crc error".into(),
                },
            ));
            let (graph, _, _) = run_coordinator(dirs, &[], 2);
            // 该 entry 未产生节点，也不应把任何现有节点误标 Accessible。
            let root = graph.nodes.get(&graph.root).unwrap();
            assert_eq!(root.access_state, AccessState::Unknown);
        }

        #[test]
        fn cancel_does_not_publish_partial_tree() {
            let cancel = Arc::new(AtomicBool::new(false));
            let cancel_setter = cancel.clone();
            // 每个 read_dir 强制 10ms 延迟，确保 5ms 后设置 cancel 时扫描仍在进行。
            let reader: Arc<dyn FsReader> =
                Arc::new(MockFsReader::with_delay(tree(), Duration::from_millis(10)));

            thread::spawn(move || {
                thread::sleep(Duration::from_millis(5));
                cancel_setter.store(true, Ordering::Relaxed);
            });

            let result = coordinator_run(reader, 'C', &[], 2, cancel, Arc::new(|_| {}));
            assert!(matches!(result, Err(ScanDriveError::Cancelled)));
        }

        #[test]
        fn worker_count_upper_bound() {
            let fixed = |_: char| DriveKind::Fixed;
            let removable = |_: char| DriveKind::Removable;
            let network = |_: char| DriveKind::Network;

            let fixed_count = concurrency_for(&fixed, 'C');
            assert!((1..=8).contains(&fixed_count));

            assert_eq!(concurrency_for(&removable, 'C'), 1);
            assert_eq!(concurrency_for(&network, 'C'), 1);
        }

        #[test]
        fn root_file_summary_injection() {
            let outcome = run_fs_scan(tree(), &[]);
            let summary = outcome.store.root_file_summary();
            assert_eq!(summary.system_metadata_size_bytes, None);
            assert!(summary.incomplete);
            assert_eq!(summary.direct_file_count, 1); // pagefile.sys
            assert_eq!(summary.direct_file_size_bytes, 1024);
            assert_eq!(summary.total_known_size_bytes, 1024);
            assert_eq!(outcome.diagnostics.scanned_records, 0);
            assert_eq!(outcome.diagnostics.hard_link_entries, 0);
            assert_eq!(outcome.store.source(), ScanSource::Filesystem);
        }

        #[test]
        fn root_direct_files_injected() {
            let mut dirs = tree();
            dirs.insert(
                normalize("C:\\"),
                vec![
                    entry("pagefile.sys", false, 1024),
                    entry("hiberfil.sys", false, 2048),
                    entry("Users", true, 0),
                    entry("ProgramData", true, 0),
                ],
            );
            let outcome = run_fs_scan(dirs, &[]);
            let summary = outcome.store.root_file_summary();
            assert_eq!(summary.direct_file_count, 2);
            assert_eq!(summary.direct_file_size_bytes, 3072);
        }

        #[test]
        fn subtree_aggregation_matches_mft_semantics() {
            // 与 T4 graph_construction_is_order_independent 同构：
            // C:\ root.txt=100
            // C:\Users\alice\docs\b.txt=300
            // C:\Users\alice\a.txt=500
            let mut dirs = HashMap::new();
            dirs.insert(
                normalize("C:\\"),
                vec![entry("root.txt", false, 100), entry("Users", true, 0)],
            );
            dirs.insert(normalize("C:\\Users"), vec![entry("alice", true, 0)]);
            dirs.insert(
                normalize("C:\\Users\\alice"),
                vec![entry("docs", true, 0), entry("a.txt", false, 500)],
            );
            dirs.insert(
                normalize("C:\\Users\\alice\\docs"),
                vec![entry("b.txt", false, 300)],
            );

            let (graph, _, _) = run_coordinator(dirs, &[], 2);
            let root = graph.nodes.get(&graph.root).unwrap();
            assert_eq!(root.direct_file_size_bytes, 100);
            assert_eq!(root.direct_file_count, 1);
            assert_eq!(root.subtree_size_bytes, 900);
            assert_eq!(root.subtree_file_count, 3);
            assert_eq!(root.subtree_dir_count, 4); // C:\ + Users + alice + docs
        }
    }
}
