use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
        if self.phase == CopyPhase::Copying { 100 } else { 0 }
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

/// 文件操作抽象。生产用 RealFileOps，测试用 mock 实现。
/// 复制期间持续回调准备/传输详情，并通过 should_cancel 支持及时取消。
pub trait FileOps {
    /// 递归复制 src 目录到 dst。dst 不存在则创建。不跟随 src 内部 reparse point。
    fn copy_tree(
        &self,
        src: &Path,
        dst: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &dyn Fn() -> bool,
    ) -> AppResult<()>;

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

impl RealFileOps {
    fn measure_tree(
        &self,
        src: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &dyn Fn() -> bool,
    ) -> AppResult<(u64, u64)> {
        let mut stack = vec![src.to_path_buf()];
        let mut total_bytes = 0u64;
        let mut total_files = 0u64;
        let mut last_emit = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);

        while let Some(current) = stack.pop() {
            if should_cancel() {
                return Err(AppError::Cancelled);
            }
            if !current.exists() {
                continue;
            }
            if self.is_reparse_point(&current) && current != src {
                continue;
            }
            if current.is_dir() {
                for entry in std::fs::read_dir(&current)? {
                    stack.push(entry?.path());
                }
            } else {
                total_bytes = total_bytes.saturating_add(std::fs::metadata(&current)?.len());
                total_files += 1;
            }

            if last_emit.elapsed() >= Duration::from_millis(200) {
                on_progress(&CopyProgress {
                    phase: CopyPhase::Preparing,
                    completed_bytes: total_bytes,
                    total_bytes: None,
                    completed_files: total_files,
                    total_files: None,
                    current_path: Some(relative_display_path(src, &current)),
                });
                last_emit = Instant::now();
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
        Ok((total_bytes, total_files))
    }
}

impl FileOps for RealFileOps {
    fn copy_tree(
        &self,
        src: &Path,
        dst: &Path,
        on_progress: &dyn Fn(&CopyProgress),
        should_cancel: &dyn Fn() -> bool,
    ) -> AppResult<()> {
        let (total_bytes, total_files) = self.measure_tree(src, on_progress, should_cancel)?;
        std::fs::create_dir_all(dst)?;
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
        let mut completed_bytes = 0u64;
        let mut completed_files = 0u64;
        let mut last_emit = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .unwrap_or_else(Instant::now);
        while let Some((cur_src, cur_dst)) = stack.pop() {
            if should_cancel() {
                return Err(AppError::Cancelled);
            }
            // 跳过 reparse point 的内容递归，但仍创建占位（见 is_reparse_point 处理）
            let is_rp = self.is_reparse_point(&cur_src);
            if !cur_src.exists() {
                continue;
            }
            if is_rp && cur_src != *src {
                // 非 src 自身的 reparse point：创建空目录占位，不进入
                std::fs::create_dir_all(&cur_dst)?;
                continue;
            }
            if cur_src.is_dir() {
                std::fs::create_dir_all(&cur_dst)?;
                for entry in std::fs::read_dir(&cur_src)? {
                    let entry = entry?;
                    let child_src = entry.path();
                    let child_dst = cur_dst.join(entry.file_name());
                    stack.push((child_src, child_dst));
                }
            } else {
                copy_file_with_control(
                    &cur_src,
                    &cur_dst,
                    should_cancel,
                    &mut completed_bytes,
                    &mut last_emit,
                    |bytes| {
                        on_progress(&CopyProgress {
                            phase: CopyPhase::Copying,
                            completed_bytes: bytes,
                            total_bytes: Some(total_bytes),
                            completed_files,
                            total_files: Some(total_files),
                            current_path: Some(relative_display_path(src, &cur_src)),
                        });
                    },
                )?;
                completed_files += 1;
                if last_emit.elapsed() >= Duration::from_millis(100)
                    || completed_files == total_files
                {
                    on_progress(&CopyProgress {
                        phase: CopyPhase::Copying,
                        completed_bytes,
                        total_bytes: Some(total_bytes),
                        completed_files,
                        total_files: Some(total_files),
                        current_path: Some(relative_display_path(src, &cur_src)),
                    });
                    last_emit = Instant::now();
                }
            }
        }
        if total_files == 0 {
            on_progress(&CopyProgress {
                phase: CopyPhase::Copying,
                completed_bytes: total_bytes,
                total_bytes: Some(total_bytes),
                completed_files: 0,
                total_files: Some(0),
                current_path: None,
            });
        }
        Ok(())
    }

    fn manifest(&self, src: &Path) -> AppResult<Manifest> {
        let mut entries = Vec::new();
        let mut stack = vec![src.to_path_buf()];
        while let Some(cur) = stack.pop() {
            if !cur.exists() { continue; }
            if self.is_reparse_point(&cur) && cur != *src {
                // 记录 reparse point 为目录占位，不进入
                entries.push(ManifestEntry {
                    rel_path: rel_under(src, &cur),
                    is_dir: true, size: 0, mtime: 0, attrs: 0,
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
        Ok(Manifest { root: src.to_string_lossy().into(), entries })
    }

    fn diff_manifests(&self, a: &Manifest, b: &Manifest) -> Vec<String> {
        use std::collections::HashMap;
        let map_a: HashMap<&str, &ManifestEntry> = a.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
        let map_b: HashMap<&str, &ManifestEntry> = b.entries.iter().map(|e| (e.rel_path.as_str(), e)).collect();
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
            trash::delete(path).map_err(|e| AppError::Win32(format!("trash::delete: {e}")))?;
            Ok(())
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
    let mtime = meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    #[cfg(windows)]
    let attrs = { use std::os::windows::fs::MetadataExt; meta.file_attributes() };
    #[cfg(not(windows))]
    let attrs = 0u32;
    Ok(ManifestEntry {
        rel_path: rel_under(root, p),
        is_dir, size, mtime, attrs,
    })
}

fn rel_under(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().into())
}

fn relative_display_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn copy_file_with_control(
    src: &Path,
    dst: &Path,
    should_cancel: &dyn Fn() -> bool,
    completed_bytes: &mut u64,
    last_emit: &mut Instant,
    mut on_chunk: impl FnMut(u64),
) -> AppResult<()> {
    const BUFFER_SIZE: usize = 1024 * 1024;
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::File::create(dst)?;
    let mut buffer = vec![0u8; BUFFER_SIZE];

    loop {
        if should_cancel() {
            return Err(AppError::Cancelled);
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        *completed_bytes = completed_bytes.saturating_add(read as u64);
        if last_emit.elapsed() >= Duration::from_millis(100) {
            on_chunk(*completed_bytes);
            *last_emit = Instant::now();
        }
    }
    output.flush()?;
    std::fs::set_permissions(dst, std::fs::metadata(src)?.permissions())?;
    Ok(())
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
    use tempfile::TempDir;

    fn ops() -> RealFileOps { RealFileOps }

    #[test]
    fn copy_tree_copies_files_and_preserves_content() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub/b.txt"), b"world").unwrap();
        let dst = root.path().join("dst");
        ops().copy_tree(&src, &dst, &|_| {}, &|| false).unwrap();
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

        ops().copy_tree(
            &src,
            &dst,
            &|progress| events.borrow_mut().push(progress.clone()),
            &|| false,
        ).unwrap();

        let events = events.into_inner();
        assert!(events.iter().any(|event| event.phase == CopyPhase::Preparing));
        let last = events.iter()
            .rev()
            .find(|event| event.phase == CopyPhase::Copying)
            .unwrap();
        assert_eq!(last.completed_bytes, 3072);
        assert_eq!(last.total_bytes, Some(3072));
        assert_eq!(last.completed_files, 2);
        assert_eq!(last.total_files, Some(2));
        assert_eq!(last.percent(), 100);
        assert!(last.current_path.is_some());
    }

    #[test]
    fn copy_tree_honors_cancellation_while_preparing() {
        let root = TempDir::new().unwrap();
        let src = root.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.bin"), vec![1u8; 1024]).unwrap();
        let dst = root.path().join("dst");

        let result = ops().copy_tree(&src, &dst, &|_| {}, &|| true);
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
        ops().copy_tree(&src, &dst, &|_| {}, &|| false).unwrap();
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
        ops().copy_tree(&src, &dst, &|_| {}, &|| false).unwrap();
        let m1 = ops().manifest(&src).unwrap();
        let m2 = ops().manifest(&dst).unwrap();
        assert!(ops().diff_manifests(&m1, &m2).is_empty(), "复制后 manifest 应一致");
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
}
