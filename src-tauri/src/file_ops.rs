use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyPhase {
    Preparing,
    Copying,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyProgress {
    pub phase: CopyPhase,
    pub completed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub completed_files: u64,
    pub total_files: Option<u64>,
    pub current_path: Option<PathBuf>,
}

impl CopyProgress {
    pub fn percent(&self) -> u8 {
        if let Some(total) = self.total_bytes.filter(|total| *total > 0) {
            return ((self.completed_bytes.saturating_mul(100) / total).min(100)) as u8;
        }
        if let Some(total) = self.total_files.filter(|total| *total > 0) {
            return ((self.completed_files.saturating_mul(100) / total).min(100)) as u8;
        }
        if self.phase == CopyPhase::Copying {
            100
        } else {
            0
        }
    }
}

/// 一条 manifest 记录：相对路径、类型、字节数、mtime、attributes。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestEntry {
    pub rel_path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix 秒
    pub mtime: i64,
    pub attrs: u32,
}

/// 一份目录的 manifest，用于复制后校验一致性。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    pub root: String,
    pub entries: Vec<ManifestEntry>,
}

/// `copy_tree` 的产出：实际读入的源条目与实际落盘的目标条目各一份 manifest，
/// 外加扫描阶段统计的总量。两份 manifest 由 migrator 的 b'/c' 直接 diff，
/// 不再额外遍历目录。
pub struct CopyOutcome {
    /// 实际创建的目录/reparse 占位 + 实际成功读入的文件条目（size=实际读入字节）。
    pub copied_manifest: Manifest,
    /// 对应目标条目创建/写入后取得的实际元数据（文件 size=目标落盘字节）。
    pub dst_manifest: Manifest,
    pub total_bytes: u64,
    pub total_files: u64,
}

/// 并发度解析：`None` 走默认 `min(available_parallelism, 8)`；`Some(n)` 原样使用
/// （`NonZeroUsize` 从类型层保证 >= 1）。
pub fn resolve_concurrency(override_: Option<NonZeroUsize>) -> usize {
    match override_ {
        Some(n) => n.get(),
        None => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(8),
    }
}

/// 单个文件的复制任务（扫描阶段产出，并发阶段消费）。
struct CopyTask {
    src: PathBuf,
    dst: PathBuf,
    rel_path: String,
}

/// 文件操作抽象。生产用 RealFileOps，测试用 mock 实现。
/// 复制期间持续回调准备/传输详情，并通过 should_cancel 支持及时取消。
pub trait FileOps {
    /// 递归复制 src 目录到 dst。dst 不存在则创建。不跟随 src 内部 reparse point。
    fn copy_tree(
        &self,
        src: &Path,
        dst: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &(dyn Fn() -> bool + Sync),
        concurrency: usize,
    ) -> AppResult<CopyOutcome>;

    /// 生成 src 目录的 manifest（不含 src 自身的 reparse point 内部，但含直接子项）。
    fn manifest(&self, src: &Path) -> AppResult<Manifest>;

    /// 对比两份 manifest，返回不一致项的相对路径（空=一致）。
    fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String>;

    /// 原子改名（同卷用 MoveFileEx，跨卷退化为复制+删除）。
    fn rename(&self, from: &Path, to: &Path) -> AppResult<()>;

    /// 移到回收站（allow undo）。
    fn to_recycle_bin(&self, path: &Path) -> AppResult<()>;

    /// 递归删除（用于清理 tmp；不走回收站）。
    fn remove_tree(&self, path: &Path) -> AppResult<()>;

    /// 路径是否为 reparse point（junction/symlink）。
    fn is_reparse_point(&self, path: &Path) -> bool;

    /// 目录是否存在且可读。
    fn dir_exists(&self, path: &Path) -> bool;

    /// 创建目录联接：link 指向 target。
    fn create_junction(&self, link: &Path, target: &Path) -> AppResult<()>;

    /// 删除 junction（只删链接壳，不删目标）。
    fn remove_junction(&self, link: &Path) -> AppResult<()>;

    /// 校验 junction 是否解析到有效目标。
    fn junction_resolves(&self, link: &Path) -> bool;
}

pub struct RealFileOps;

/// `trash` 在 Windows 的 COM 初始化失败时会 panic，而不是返回 `Err`。
/// 回收旧目录是迁移完成后的 best-effort 清理；把该 panic 转成普通错误，
/// 由 migrator 保留 oldPath 并标记为待清理，不能让已建好的 junction 被误报失败。
fn guard_recycle_bin<F>(operation: F) -> AppResult<()>
where
    F: FnOnce() -> AppResult<()>,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)).unwrap_or_else(|_| {
        Err(AppError::Win32(
            "回收站组件异常，旧目录已保留等待清理".into(),
        ))
    })
}

/// 复制单个普通文件，返回实际读写的字节数。不回调进度（进度由调用方的主线程汇报器统一发出）。
/// 每读完一个 buffer 块检查取消。
fn copy_file_counted(
    src: &Path,
    dst: &Path,
    should_cancel: &(dyn Fn() -> bool + Sync),
) -> AppResult<u64> {
    const BUFFER_SIZE: usize = 1024 * 1024;
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::File::create(dst)?;
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut copied = 0u64;
    loop {
        if should_cancel() {
            return Err(AppError::Cancelled);
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        copied = copied.saturating_add(read as u64);
    }
    output.flush()?;
    std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())?;
    Ok(copied)
}

impl FileOps for RealFileOps {
    fn copy_tree(
        &self,
        src: &Path,
        dst: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &(dyn Fn() -> bool + Sync),
        concurrency: usize,
    ) -> AppResult<CopyOutcome> {
        let mut copied_entries: Vec<ManifestEntry> = Vec::new();
        let mut dst_entries: Vec<ManifestEntry> = Vec::new();
        let mut tasks: Vec<CopyTask> = Vec::new();
        let mut total_bytes = 0u64;
        let mut total_files = 0u64;

        // 阶段①：单线程 stat-only 扫描，建目录、收集文件任务、记录目录/reparse 占位。
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf(), String::new())];
        let mut last_emit = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        while let Some((cur_src, cur_dst, rel)) = stack.pop() {
            if should_cancel() {
                return Err(AppError::Cancelled);
            }
            if !cur_src.exists() {
                continue;
            }
            let is_rp = self.is_reparse_point(&cur_src) && cur_src != *src;
            if is_rp {
                // 非 src 自身的 reparse point：建空目录占位；两份 manifest 记完全相同的
                // 占位条目（rel_path/is_dir/size 三者一致），保证该条目 diff 为空。
                std::fs::create_dir_all(&cur_dst)?;
                let placeholder = ManifestEntry {
                    rel_path: rel,
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                    attrs: 0,
                };
                copied_entries.push(placeholder.clone());
                dst_entries.push(placeholder);
                continue;
            }
            if cur_src.is_dir() {
                std::fs::create_dir_all(&cur_dst)?;
                if cur_src != *src {
                    copied_entries.push(ManifestEntry {
                        rel_path: rel.clone(),
                        is_dir: true,
                        size: 0,
                        mtime: 0,
                        attrs: 0,
                    });
                    dst_entries.push(ManifestEntry {
                        rel_path: rel.clone(),
                        is_dir: true,
                        size: 0,
                        mtime: 0,
                        attrs: 0,
                    });
                }
                for entry in std::fs::read_dir(&cur_src)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let child_rel = if rel.is_empty() {
                        name.to_string_lossy().replace('\\', "/")
                    } else {
                        format!("{}/{}", rel, name.to_string_lossy())
                    };
                    stack.push((
                        entry.path(),
                        cur_dst.join(&name),
                        child_rel,
                    ));
                }
            } else {
                // 文件：只 stat 预估总量并推入任务；不作为一致性基准。
                total_bytes = total_bytes.saturating_add(std::fs::metadata(&cur_src)?.len());
                total_files += 1;
                tasks.push(CopyTask {
                    src: cur_src,
                    dst: cur_dst,
                    rel_path: rel,
                });
                if last_emit.elapsed() >= Duration::from_millis(200) {
                    on_progress(&CopyProgress {
                        phase: CopyPhase::Preparing,
                        completed_bytes: total_bytes,
                        total_bytes: None,
                        completed_files: total_files,
                        total_files: None,
                        current_path: None,
                    });
                    last_emit = Instant::now();
                }
            }
        }
        on_progress(&CopyProgress {
            phase: CopyPhase::Preparing,
            completed_bytes: total_bytes,
            total_bytes: None,
            completed_files: total_files,
            total_files: None,
            current_path: None,
        });

        // 阶段②：并发复制。目录已在阶段①串行建好，worker 只往已存在目录填文件，无 create 竞态。
        // 注意：原子值用 Arc 包裹，让 N 个 worker 与主线程汇报器都能持有引用，避免被首个 worker 移动。
        let actual_bytes = Arc::new(AtomicU64::new(0));
        let actual_files = Arc::new(AtomicUsize::new(0));
        let copied_files: Arc<Mutex<Vec<ManifestEntry>>> = Arc::new(Mutex::new(Vec::with_capacity(tasks.len())));
        let dst_files: Arc<Mutex<Vec<ManifestEntry>>> = Arc::new(Mutex::new(Vec::with_capacity(tasks.len())));
        let first_error: Arc<Mutex<Option<AppError>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let queue: Arc<Mutex<VecDeque<CopyTask>>> = Arc::new(Mutex::new(tasks.into_iter().collect()));

        let n = concurrency.max(1);
        let active = Arc::new(AtomicUsize::new(n));

        std::thread::scope(|s| {
            for _ in 0..n {
                let queue = Arc::clone(&queue);
                let copied_files = Arc::clone(&copied_files);
                let dst_files = Arc::clone(&dst_files);
                let first_error = Arc::clone(&first_error);
                let stop = Arc::clone(&stop);
                let active = Arc::clone(&active);
                let actual_bytes = Arc::clone(&actual_bytes);
                let actual_files = Arc::clone(&actual_files);
                let ops = self;
                s.spawn(move || loop {
                    if stop.load(Ordering::Relaxed) || should_cancel() {
                        active.fetch_sub(1, Ordering::Relaxed);
                        break;
                    }
                    let task = queue.lock().unwrap().pop_front();
                    let Some(task) = task else {
                        active.fetch_sub(1, Ordering::Relaxed);
                        break;
                    };
                    // 打开前重新检查：已不存在或已不再是普通文件（含变 reparse/目录）则跳过，
                    // 交由 c' 对账，不算复制错误。
                    let still_plain = match std::fs::symlink_metadata(&task.src) {
                        Ok(m) => m.is_file() && !ops.is_reparse_point(&task.src),
                        Err(_) => false,
                    };
                    if !still_plain {
                        continue;
                    }
                    match copy_file_counted(&task.src, &task.dst, should_cancel) {
                        Ok(actual) => {
                            actual_bytes.fetch_add(actual, Ordering::Relaxed);
                            actual_files.fetch_add(1, Ordering::Relaxed);
                            copied_files.lock().unwrap().push(ManifestEntry {
                                rel_path: task.rel_path.clone(),
                                is_dir: false,
                                size: actual,
                                mtime: 0,
                                attrs: 0,
                            });
                            match std::fs::symlink_metadata(&task.dst) {
                                Ok(m) => dst_files.lock().unwrap().push(ManifestEntry {
                                    rel_path: task.rel_path,
                                    is_dir: false,
                                    size: m.len(),
                                    mtime: 0,
                                    attrs: 0,
                                }),
                                Err(e) => {
                                    let mut err = first_error.lock().unwrap();
                                    if err.is_none() {
                                        *err = Some(AppError::from(e));
                                    }
                                    stop.store(true, Ordering::Relaxed);
                                }
                            }
                        }
                        Err(e) => {
                            let mut err = first_error.lock().unwrap();
                            if err.is_none() {
                                *err = Some(e);
                            }
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                });
            }
            // 主线程进度汇报器：每 100ms 读原子值回调 on_progress，completed clamp 到 total。
            while active.load(Ordering::Relaxed) > 0 {
                if should_cancel() {
                    stop.store(true, Ordering::Relaxed);
                }
                let done_bytes = actual_bytes.load(Ordering::Relaxed).min(total_bytes);
                let done_files = (actual_files.load(Ordering::Relaxed) as u64).min(total_files);
                on_progress(&CopyProgress {
                    phase: CopyPhase::Copying,
                    completed_bytes: done_bytes,
                    total_bytes: Some(total_bytes),
                    completed_files: done_files,
                    total_files: Some(total_files),
                    current_path: None,
                });
                std::thread::sleep(Duration::from_millis(100));
                // 若队列已空且无 worker 活跃，主线程也退出（避免空转）。
                if queue.lock().unwrap().is_empty()
                    && active.load(Ordering::Relaxed) == 0
                {
                    break;
                }
            }
            // scope 返回前 join 所有 worker；worker 退出时通过 active 计数衰减。
            // （scope 自动 join，此处无需显式 join。）
        });

        // 最终一次进度（确保小文件快速完成时也至少发出一个 Copying 终态事件）。
        let done_bytes = actual_bytes.load(Ordering::Relaxed).min(total_bytes);
        let done_files = (actual_files.load(Ordering::Relaxed) as u64).min(total_files);
        on_progress(&CopyProgress {
            phase: CopyPhase::Copying,
            completed_bytes: done_bytes,
            total_bytes: Some(total_bytes),
            completed_files: done_files,
            total_files: Some(total_files),
            current_path: None,
        });

        // 错误/取消优先于成功。
        if let Some(e) = first_error.lock().unwrap().take() {
            return Err(e);
        }
        if should_cancel() {
            return Err(AppError::Cancelled);
        }

        copied_entries.extend(copied_files.lock().unwrap().drain(..));
        dst_entries.extend(dst_files.lock().unwrap().drain(..));

        Ok(CopyOutcome {
            copied_manifest: Manifest {
                root: src.to_string_lossy().into(),
                entries: copied_entries,
            },
            dst_manifest: Manifest {
                root: dst.to_string_lossy().into(),
                entries: dst_entries,
            },
            total_bytes,
            total_files,
        })
    }

    fn manifest(&self, src: &Path) -> AppResult<Manifest> {
        let mut entries = Vec::new();
        let mut stack = vec![src.to_path_buf()];
        while let Some(cur) = stack.pop() {
            if !cur.exists() {
                continue;
            }
            if self.is_reparse_point(&cur) && cur != *src {
                // 记录 reparse point 为目录占位，不进入
                entries.push(ManifestEntry {
                    rel_path: rel_under(src, &cur),
                    is_dir: true,
                    size: 0,
                    mtime: 0,
                    attrs: 0,
                });
                continue;
            }
            if cur.is_dir() {
                if cur != *src {
                    entries.push(entry_for(&cur, src, true)?);
                }
                for e in std::fs::read_dir(&cur)? {
                    stack.push(e?.path());
                }
            } else {
                entries.push(entry_for(&cur, src, false)?);
            }
        }
        Ok(Manifest {
            root: src.to_string_lossy().into(),
            entries,
        })
    }

    fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String> {
        use std::collections::HashMap;
        let map_a: HashMap<&str, &ManifestEntry> =
            a.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
        let map_b: HashMap<&str, &ManifestEntry> =
            b.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
        let mut diffs = Vec::new();
        let mut keys: std::collections::HashSet<&str> = map_a.keys().copied().collect();
        keys.extend(map_b.keys().copied());
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

    fn rename(&self, from: &Path, to: &Path) -> AppResult<()> {
        // 同卷直接 rename 原子；跨卷 std::fs::rename 会失败，退化为复制+删除
        match std::fs::rename(from, to) {
            Ok(()) => Ok(()),
            Err(_) => {
                std::fs::create_dir_all(to.parent().unwrap_or(Path::new(".")))?;
                copy_recursive(from, to)?;
                std::fs::remove_dir_all(from)?;
                Ok(())
            }
        }
    }

    fn to_recycle_bin(&self, path: &Path) -> AppResult<()> {
        #[cfg(windows)]
        {
            guard_recycle_bin(|| {
                trash::delete(path).map_err(|e| AppError::Win32(format!("trash::delete: {e}")))
            })
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(AppError::Win32("仅支持 Windows".into()))
        }
    }

    fn remove_tree(&self, path: &Path) -> AppResult<()> {
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    fn is_reparse_point(&self, path: &Path) -> bool {
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
            match std::fs::symlink_metadata(path) {
                Ok(m) => m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0,
                Err(_) => false,
            }
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            false
        }
    }

    fn dir_exists(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn create_junction(&self, link: &Path, target: &Path) -> AppResult<()> {
        crate::junction::create(link, target)
    }

    fn remove_junction(&self, link: &Path) -> AppResult<()> {
        crate::junction::remove(link)
    }

    fn junction_resolves(&self, link: &Path) -> bool {
        crate::junction::verify(link)
    }
}

fn entry_for(p: &Path, root: &Path, is_dir: bool) -> AppResult<ManifestEntry> {
    let meta = std::fs::symlink_metadata(p)?;
    let size = if is_dir { 0 } else { meta.len() };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(windows)]
    let attrs = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes()
    };
    #[cfg(not(windows))]
    let attrs = 0u32;
    Ok(ManifestEntry {
        rel_path: rel_under(root, p),
        is_dir,
        size,
        mtime,
        attrs,
    })
}

fn rel_under(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().into())
}

fn copy_recursive(src: &Path, dst: &Path) -> AppResult<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for e in std::fs::read_dir(src)? {
            let e = e?;
            copy_recursive(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;
    use tempfile::TempDir;

    fn ops() -> RealFileOps {
        RealFileOps
    }

    #[test]
    fn resolve_concurrency_defaults_to_available_cores_capped_at_8() {
        let default = resolve_concurrency(None);
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        assert_eq!(default, cores.min(8));
        assert!(default >= 1);
    }

    #[test]
    fn resolve_concurrency_override_is_used_verbatim() {
        // 覆盖值原样使用（不 clamp）；NonZeroUsize 从类型层保证 >= 1。
        assert_eq!(resolve_concurrency(Some(NonZeroUsize::new(1).unwrap())), 1);
        assert_eq!(resolve_concurrency(Some(NonZeroUsize::new(3).unwrap())), 3);
    }

    #[test]
    fn copy_tree_copies_files_and_preserves_content() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"world").unwrap();
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1).unwrap();
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"world");
    }

    #[test]
    fn copy_tree_reports_real_transfer_totals() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 1024]).unwrap();
        std::fs::write(src.join("b.bin"), vec![2u8; 2048]).unwrap();
        let dst = root.path().join("dst");
        let events = std::cell::RefCell::new(Vec::new());

        ops()
            .copy_tree(
                &src,
                &dst,
                &|progress| events.borrow_mut().push(progress.clone()),
                &|| false,
                1,
            )
            .unwrap();

        let events = events.into_inner();
        assert!(events
            .iter()
            .any(|event| event.phase == CopyPhase::Preparing));
        let last = events
            .iter()
            .rev()
            .find(|event| event.phase == CopyPhase::Copying)
            .unwrap();
        assert_eq!(last.completed_bytes, 3072);
        assert_eq!(last.total_bytes, Some(3072));
        assert_eq!(last.completed_files, 2);
        assert_eq!(last.total_files, Some(2));
        assert_eq!(last.percent(), 100);
        // 并发模型下进度由原子计数器汇总，current_path 统一为 None；
        // 串行（concurrency=1）也走同一实现，因此保持 None。
        assert!(last.current_path.is_none());
    }

    #[test]
    fn copy_tree_honors_cancellation_while_preparing() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 1024]).unwrap();
        let dst = root.path().join("dst");

        let result = ops().copy_tree(&src, &dst, &|_| {}, &|| true, 1);
        assert!(matches!(result, Err(AppError::Cancelled)));
        assert!(!dst.exists());
    }

    #[cfg(windows)]
    #[test]
    fn copy_tree_does_not_descend_into_reparse_point() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        let inner_link = src.join("link");
        let link_target = root.path().join("target");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&link_target).unwrap();
        std::fs::write(link_target.join("secret.txt"), b"x").unwrap();
        junction::create(&link_target, &inner_link).unwrap();
        // 复制 src 到 dst
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1).unwrap();
        // link 应作为占位存在（不跟随源 reparse point 的内容递归）
        assert!(dst.join("link").exists());
        // 不应在 dst 中递归进入 target 的内容（secret.txt 不应出现）
        assert!(!dst.join("link/secret.txt").exists());
    }

    #[test]
    fn manifest_then_diff_matches_for_identical_copy() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1).unwrap();
        let m1 = ops().manifest(&src).unwrap();
        let m2 = ops().manifest(&dst).unwrap();
        assert!(
            ops().diff_manifests(&m1, &m2).is_empty(),
            "复制后 manifest 应一致"
        );
    }

    #[test]
    fn diff_manifests_detects_size_change() {
        let root = TempDir::new().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("f.txt"), b"12345").unwrap();
        std::fs::write(b.join("f.txt"), b"123").unwrap();
        let m1 = ops().manifest(&a).unwrap();
        let m2 = ops().manifest(&b).unwrap();
        let diff = ops().diff_manifests(&m1, &m2);
        assert!(diff.iter().any(|p| p == "f.txt"), "应检测到 f.txt 不一致");
    }

    #[test]
    fn to_recycle_bin_removes_path() {
        let root = TempDir::new().unwrap();
        let victim = root.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("a.txt"), b"hi").unwrap();
        let res = ops().to_recycle_bin(&victim);
        // 回收站在 CI/某些环境可能不可用，允许失败但不 panic；可用时必须删掉
        if res.is_ok() {
            assert!(!victim.exists());
        }
    }

    #[test]
    fn recycle_bin_panic_is_returned_as_cleanup_error() {
        let error = guard_recycle_bin(|| -> AppResult<()> { panic!("simulated recycle panic") })
            .expect_err("第三方回收站组件 panic 必须降级为错误");
        assert!(error.to_string().contains("旧目录已保留"));
    }

    #[test]
    fn copy_tree_concurrent_matches_serial_for_many_small_files() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        for i in 0..200 {
            std::fs::write(src.join(format!("f{i}.txt")), vec![i as u8; 64]).unwrap();
        }
        for i in 0..50 {
            std::fs::write(src.join(format!("a/b/g{i}.txt")), vec![9u8; 128]).unwrap();
        }
        // 多 worker
        let dst_multi = root.path().join("dst_multi");
        let outcome_multi = ops()
            .copy_tree(&src, &dst_multi, &|_| {}, &|| false, 8)
            .unwrap();
        // 单 worker
        let dst_one = root.path().join("dst_one");
        let outcome_one = ops()
            .copy_tree(&src, &dst_one, &|_| {}, &|| false, 1)
            .unwrap();
        // 两份 outcome manifest 各自内部一致
        assert!(
            ops().diff_manifests(&outcome_multi.copied_manifest, &outcome_multi.dst_manifest).is_empty(),
            "多 worker 下 copied 与 dst manifest 必须一致"
        );
        // 并发与串行的 copied manifest 一致（条目集合相同）
        assert_eq!(outcome_multi.total_files, outcome_one.total_files);
        assert_eq!(outcome_multi.total_bytes, outcome_one.total_bytes);
        // 内容抽检
        assert_eq!(std::fs::read(dst_multi.join("f0.txt")).unwrap(), vec![0u8; 64]);
        assert_eq!(std::fs::read(dst_multi.join("a/b/g0.txt")).unwrap(), vec![9u8; 128]);
    }

    #[test]
    fn copy_tree_preserves_empty_dirs_and_outcome_manifests_agree() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("empty/sub")).unwrap();
        std::fs::write(src.join("keep.txt"), b"x").unwrap();
        let dst = root.path().join("dst");
        let outcome = ops()
            .copy_tree(&src, &dst, &|_| {}, &|| false, 4)
            .unwrap();
        // 空目录被保留
        assert!(dst.join("empty/sub").is_dir(), "空目录应被复制");
        assert_eq!(std::fs::read(dst.join("keep.txt")).unwrap(), b"x");
        // 两份 outcome manifest 真实 diff 为空（复制完整性 b'）
        assert!(
            ops().diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest).is_empty(),
            "copied 与 dst manifest 必须一致"
        );
        // copied manifest 应含 keep.txt（文件）与目录条目
        let rels: Vec<&str> = outcome.copied_manifest.entries.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(rels.contains(&"keep.txt"));
        assert!(rels.iter().any(|r| r.starts_with("empty")), "应含 empty 目录条目");
    }

    #[test]
    fn copy_tree_skips_files_removed_after_scan_without_error() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("keep.txt"), b"keep").unwrap();
        let gone_path = src.join("gone.txt");
        std::fs::write(&gone_path, b"gone").unwrap();
        let dst = root.path().join("dst");
        // 阶段①扫描末尾会发一次最终 Preparing 事件；在其回调里删除 gone.txt，
        // 使阶段② worker 打开它前 symlink_metadata 检查命中"已删除"-> 跳过。
        let gone = gone_path.clone();
        let outcome = ops()
            .copy_tree(
                &src,
                &dst,
                &|p| {
                    if p.phase == CopyPhase::Preparing {
                        let _ = std::fs::remove_file(&gone);
                    }
                },
                &|| false,
                1,
            )
            .unwrap();
        // 不报错；keep.txt 正常复制
        assert_eq!(std::fs::read(dst.join("keep.txt")).unwrap(), b"keep");
        assert!(!dst.join("gone.txt").exists(), "被删文件不应出现在 dst");
        // outcome 自洽：copied 与 dst 一致（均不含 gone）
        assert!(
            ops().diff_manifests(&outcome.copied_manifest, &outcome.dst_manifest).is_empty(),
            "源变化跳过的条目不应造成 copied/dst 不一致"
        );
    }

    #[test]
    fn copy_tree_mid_copy_cancellation_returns_cancelled() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        // 10MB 单文件：阶段①扫描 should_cancel 调用极少，阈值落在阶段②块复制中。
        std::fs::write(src.join("big.bin"), vec![0u8; 10 * 1024 * 1024]).unwrap();
        let dst = root.path().join("dst");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = counter.clone();
        let cancel = move || {
            // 前 5 次（扫描 + 取任务 + 前几块）false，之后 true -> 块复制中途取消。
            c.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 5
        };
        let result = ops().copy_tree(&src, &dst, &|_| {}, &cancel, 1);
        assert!(matches!(result, Err(AppError::Cancelled)), "复制中途取消应返回 Cancelled");
    }

    #[test]
    fn copy_tree_propagates_first_io_error() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("x.txt"), b"hello").unwrap();
        let dst = root.path().join("dst");
        std::fs::create_dir_all(dst.join("x.txt")).unwrap(); // 目标同名已是目录 -> create 失败
        let result = ops().copy_tree(&src, &dst, &|_| {}, &|| false, 1);
        assert!(result.is_err(), "dst 碰撞应作为 IO 错误传播");
        // 不应是 Cancelled；应是 Io 错误（File::create 对目录失败）
        assert!(!matches!(result, Err(AppError::Cancelled)));
    }

    #[test]
    fn copy_tree_progress_never_exceeds_total() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 1024]).unwrap();
        std::fs::write(src.join("b.bin"), vec![2u8; 2048]).unwrap();
        let dst = root.path().join("dst");
        let events = std::cell::RefCell::new(Vec::new());
        ops()
            .copy_tree(
                &src,
                &dst,
                &|p| events.borrow_mut().push(p.clone()),
                &|| false,
                4,
            )
            .unwrap();
        let events = events.into_inner();
        let mut last_completed = 0u64;
        for e in &events {
            if e.phase == CopyPhase::Copying {
                if let Some(total) = e.total_bytes {
                    assert!(e.completed_bytes <= total, "completed_bytes 不得超过 total_bytes");
                }
                if let Some(total) = e.total_files {
                    assert!(e.completed_files <= total, "completed_files 不得超过 total_files");
                }
                // 单调（主线程汇报器按原子累加值发出，clamp 后不减）
                assert!(e.completed_bytes >= last_completed, "进度不得回退");
                last_completed = e.completed_bytes;
            }
        }
    }
}
