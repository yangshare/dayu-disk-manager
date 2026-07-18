use crate::error::{AppError, AppResult};
use std::path::{Path, PathBuf};

/// 普通路径转 Win32 长路径（加 \\?\ 前缀，绕过 MAX_PATH 并保留字面字符）。
pub fn to_long_path(p: &str) -> String {
    let trimmed = p.trim();
    if trimmed.starts_with(r"\\?\") || trimmed.starts_with(r"\\.\") {
        return trimmed.into();
    }
    if let Some(rest) = trimmed.strip_prefix(r"\\") {
        // UNC 路径：\\server\share -> \\?\UNC\server\share
        return format!(r"\\?\UNC\{}", rest);
    }
    // 磁盘根（如 C:\）需加尾随反斜杠，使 \\?\C:\ 变为 \\?\C:\\
    if trimmed.len() == 3 && trimmed.as_bytes().get(1) == Some(&b':') && trimmed.ends_with('\\') {
        return format!(r"\\?\{}\", trimmed);
    }
    format!(r"\\?\{}", trimmed)
}

pub fn local_appdata_dayu_dir() -> AppResult<PathBuf> {
    let base = dirs::data_local_dir().ok_or_else(|| AppError::Win32("无法解析 %LOCALAPPDATA%".into()))?;
    Ok(base.join("dayu-disk-manager"))
}

#[cfg(windows)]
pub fn disk_free_bytes(path: &Path) -> AppResult<u64> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide = to_wide(&to_long_path(&path_to_str(path)));
    let mut free_to_caller: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR(wide.as_ptr()),
            Some(&mut free_to_caller as *mut u64),
            Some(&mut total as *mut u64),
            Some(&mut free as *mut u64),
        ).map_err(|e| AppError::Win32(format!("GetDiskFreeSpaceExW: {e}")))?;
    }
    Ok(free_to_caller)
}

#[cfg(not(windows))]
pub fn disk_free_bytes(_path: &Path) -> AppResult<u64> {
    Err(AppError::Win32("仅支持 Windows".into()))
}

#[cfg(windows)]
pub fn volume_info(path: &Path) -> AppResult<(String, bool)> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    // 卷信息需基于"卷根"（如 C:\），取路径所在盘根
    let root = volume_root(path)?;
    let wide = to_wide(&to_long_path(&root));
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    let mut fs_name = [0u16; 256];
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut serial as *mut u32),
            Some(&mut max_component as *mut u32),
            Some(&mut flags as *mut u32),
            Some(&mut fs_name),
        ).map_err(|e| AppError::Win32(format!("GetVolumeInformationW: {e}")))?;
    }
    let fs = from_wide(&fs_name).to_lowercase();
    let serial_hex = format!("{:08X}", serial);
    let is_ntfs = fs == "ntfs";
    Ok((serial_hex, is_ntfs))
}

#[cfg(not(windows))]
pub fn volume_info(_path: &Path) -> AppResult<(String, bool)> {
    Err(AppError::Win32("仅支持 Windows".into()))
}

/// 取路径所在盘根，如 C:\Users\xxx -> C:\
fn volume_root(path: &Path) -> AppResult<String> {
    let s = path_to_str(path);
    let s = s.trim_start_matches(r"\\?\").trim_start_matches(r"\\.\");
    if let Some(drive) = s.get(0..2) {
        if drive.as_bytes()[1] == b':' {
            return Ok(format!("{}\\", drive.to_uppercase()));
        }
    }
    Err(AppError::Win32(format!("无法解析盘根: {s}")))
}

/// Restart Manager 检测哪些进程锁定了某路径。无占用返回 None。
#[cfg(windows)]
pub fn locked_processes(path: &Path) -> AppResult<Option<Vec<String>>> {
    use windows::core::{PCWSTR, PWSTR, HSTRING};
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
    };
    let mut key: [u16; 256] = [0; 256];
    let mut handle: u32 = 0;
    let long = to_long_path(&path_to_str(path));
    let path_h = HSTRING::from(&long);
    unsafe {
        let rc = RmStartSession(&mut handle, Some(0), PWSTR(key.as_mut_ptr()));
        if rc.is_err() {
            return Err(AppError::Win32("RmStartSession 失败".into()));
        }
        let resources = [PCWSTR(path_h.as_ptr())];
        let reg = RmRegisterResources(handle, Some(&resources), None, None);
        let result = if reg.is_err() {
            Err(AppError::Win32("RmRegisterResources 失败".into()))
        } else {
            let mut nprocs_needed: u32 = 0;
            let mut nprocs: u32 = 64;
            let mut reason: u32 = 0;
            let mut buf = [RM_PROCESS_INFO::default(); 64];
            let rc2 = RmGetList(handle, &mut nprocs_needed, &mut nprocs, Some(buf.as_mut_ptr()), &mut reason);
            if rc2 == ERROR_ACCESS_DENIED {
                // Restart Manager 对目录路径返回 ACCESS_DENIED，视为无占用
                Ok(None)
            } else if rc2.is_err() {
                Err(AppError::Win32(format!("RmGetList 失败: code={}", rc2.0)))
            } else if nprocs == 0 {
                Ok(None)
            } else {
                let names: Vec<String> = buf[..nprocs as usize]
                    .iter()
                    .map(|p| from_wide_slice(&p.strAppName))
                    .collect();
                Ok(Some(names))
            }
        };
        let _ = RmEndSession(handle);
        result
    }
}

#[cfg(not(windows))]
pub fn locked_processes(_path: &Path) -> AppResult<Option<Vec<String>>> {
    Ok(None)
}

// ===== 辅助：宽字符转换 =====
#[cfg(windows)]
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

#[cfg(windows)]
fn from_wide_slice(buf: &[u16]) -> String {
    from_wide(buf)
}

fn path_to_str(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn long_path_adds_prefix_for_drive() {
        assert_eq!(to_long_path("C:/Users/x"), r"\\?\C:/Users/x");
        // 正斜杠也接受，Win32 文件 API 兼容；统一不强行转反斜杠以免引入双重转义
    }

    #[test]
    fn long_path_presapes_drive_root() {
        assert_eq!(to_long_path("C:\\"), r"\\?\C:\\");
    }

    #[test]
    fn disk_free_space_nonzero_on_temp() {
        let dir = TempDir::new().unwrap();
        let free = disk_free_bytes(dir.path()).unwrap();
        assert!(free > 0);
    }

    #[test]
    fn volume_serial_nonempty_on_temp() {
        let dir = TempDir::new().unwrap();
        let (serial, is_ntfs) = volume_info(dir.path()).unwrap();
        assert!(!serial.is_empty());
        // CI 上可能非 NTFS，is_ntfs 只断言不 panic
        let _ = is_ntfs;
    }

    #[test]
    fn local_appdata_dir_resolves() {
        let d = local_appdata_dayu_dir().unwrap();
        assert!(d.to_string_lossy().contains("dayu-disk-manager"));
    }

    #[test]
    fn locked_processes_on_empty_dir_is_none() {
        let dir = TempDir::new().unwrap();
        // 空目录不应被占用
        assert!(locked_processes(dir.path()).unwrap().is_none());
    }
}
