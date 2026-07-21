use crate::error::{AppError, AppResult};
use std::path::Path;

/// 创建目录联接：link 指向 target。link 必须不存在或为已删除的空壳。
pub fn create(link: &Path, target: &Path) -> AppResult<()> {
    #[cfg(windows)]
    {
        junction::create(target, link)
            .map_err(|e| AppError::Junction(format!("create 失败: {e}")))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (link, target);
        Err(AppError::Junction("仅支持 Windows".into()))
    }
}

/// 删除 junction（只删链接壳，不删目标）。
pub fn remove(link: &Path) -> AppResult<()> {
    #[cfg(windows)]
    {
        junction::delete(link).map_err(|e| AppError::Junction(format!("remove 失败: {e}")))?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        Err(AppError::Junction("仅支持 Windows".into()))
    }
}

/// 解析 junction 指向的目标路径（绝对路径）。
pub fn resolve(link: &Path) -> AppResult<std::path::PathBuf> {
    #[cfg(windows)]
    {
        junction::get_target(link).map_err(|e| AppError::Junction(format!("resolve 失败: {e}")))
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        Err(AppError::Junction("仅支持 Windows".into()))
    }
}

/// link 是否是一个 junction（reparse point 且类型为 junction）。
pub fn exists(link: &Path) -> bool {
    #[cfg(windows)]
    {
        junction::exists(link).unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = link;
        false
    }
}

/// 校验：link 是 junction 且其目标目录真实存在且可访问。
pub fn verify(link: &Path) -> bool {
    if !exists(link) {
        return false;
    }
    match resolve(link) {
        Ok(target) => target.is_dir(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn create_junction_resolves_to_target() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"hi").unwrap();
        let link = root.path().join("link");
        create(&link, &target).unwrap();
        assert!(exists(&link));
        assert!(std::fs::read_link(&link).is_ok());
        // 通过链接读取内容
        assert_eq!(std::fs::read(link.join("a.txt")).unwrap(), b"hi");
    }

    #[test]
    fn resolve_returns_target_path() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.path().join("l");
        create(&link, &target).unwrap();
        let resolved = resolve(&link).unwrap();
        assert!(resolved.ends_with("t"));
    }

    #[test]
    fn remove_junction_keeps_target() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.txt"), b"hi").unwrap();
        let link = root.path().join("l");
        create(&link, &target).unwrap();
        remove(&link).unwrap();
        assert!(!exists(&link));
        assert!(target.join("a.txt").exists(), "删链接不应删目标数据");
    }

    #[test]
    fn verify_detects_broken_link() {
        let root = TempDir::new().unwrap();
        let target = root.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        let link = root.path().join("l");
        create(&link, &target).unwrap();
        std::fs::remove_dir_all(&target).unwrap();
        assert!(!verify(&link));
    }
}
