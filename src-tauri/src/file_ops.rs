use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
/// on_progress(percent: 0..=100) 在复制期间被回调。
pub trait FileOps {
    /// 递归复制 src 目录到 dst。dst 不存在则创建。不跟随 src 内部 reparse point。
    fn copy_tree(&self, src: &Path, dst: &Path, on_progress: &dyn Fn(u8)) -> AppResult<()>;

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
}

pub struct RealFileOps;

impl FileOps for RealFileOps {
    fn copy_tree(&self, src: &Path, dst: &Path, on_progress: &dyn Fn(u8)) -> AppResult<()> {
        std::fs::create_dir_all(dst)?;
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
        while let Some((cur_src, cur_dst)) = stack.pop() {
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
                std::fs::copy(&cur_src, &cur_dst)?;
                on_progress(0); // 真实实现可按字节累计；此处仅保证回调被调
            }
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
        ops().copy_tree(&src, &dst, &|_| {}).unwrap();
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"world");
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
        ops().copy_tree(&src, &dst, &|_| {}).unwrap();
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
        ops().copy_tree(&src, &dst, &|_| {}).unwrap();
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
