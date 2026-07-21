//! 进程枚举：用于"占用检测"中按预设进程名判断目标应用是否在运行。
//!
//! 设计动机：win32::locked_processes 基于 Restart Manager，对**目录**路径
//! 返回 ERROR_ACCESS_DENIED → Ok(None)（T3 已确认），无法在目录级占用检测
//! 中生效。这里改用 ToolHelp API 枚举全系统进程名，由 safety 调用方与预设的
//! match_processes 列表做匹配。

use crate::error::{AppError, AppResult};

/// 枚举当前系统所有进程，返回**小写进程名**列表（去 `.exe` 后缀）。
///
/// 例：`"Explorer.EXE"` → `"explorer"`。
#[cfg(windows)]
pub fn running_process_names() -> AppResult<Vec<String>> {
    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    let mut out = Vec::new();
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|e| AppError::Win32(format!("CreateToolhelp32Snapshot: {e}")))?;
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(AppError::Win32(
                "CreateToolhelp32Snapshot 返回 INVALID_HANDLE_VALUE".into(),
            ));
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        // windows 0.62 中 First/Next 是两个独立函数（非统一 Process32W）。
        // First 先填充首个条目，Next 在循环里推进。
        let result = (|| -> AppResult<()> {
            Process32FirstW(snapshot, &mut entry)
                .map_err(|e| AppError::Win32(format!("Process32FirstW: {e}")))?;
            loop {
                let name = wide_to_string(&entry.szExeFile);
                let stripped = strip_exe_suffix(&name).to_lowercase();
                if !stripped.is_empty() {
                    out.push(stripped);
                }
                // Process32NextW 在无更多进程时返回 Err（FALSE 转 Result）
                match Process32NextW(snapshot, &mut entry) {
                    Ok(()) => continue,
                    Err(_) => break,
                }
            }
            Ok(())
        })();
        let _ = CloseHandle(snapshot);
        result?;
    }
    Ok(out)
}

#[cfg(not(windows))]
pub fn running_process_names() -> AppResult<Vec<String>> {
    Ok(Vec::new())
}

/// 宽字符数组（首个 \0 截断）转 String。
#[cfg(windows)]
fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// 去除 `.exe` / `.EXE` 等后缀（不区分大小写）。无后缀则原样返回。
fn strip_exe_suffix(name: &str) -> &str {
    if name.len() > 4
        && name
            .get(name.len() - 4..)
            .map(|s| s.eq_ignore_ascii_case(".exe"))
            .unwrap_or(false)
    {
        &name[..name.len() - 4]
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_suffix_works() {
        assert_eq!(strip_exe_suffix("Explorer.EXE"), "Explorer");
        assert_eq!(strip_exe_suffix("wechat.exe"), "wechat");
        assert_eq!(strip_exe_suffix("cargo"), "cargo");
        assert_eq!(strip_exe_suffix(""), "");
        assert_eq!(strip_exe_suffix(".exe"), ".exe"); // 太短，不算后缀
    }

    #[test]
    fn enum_returns_nonempty_on_windows() {
        let names = running_process_names().expect("应成功枚举");
        assert!(!names.is_empty());
        // 全部应小写、无 .exe 后缀
        for n in &names {
            assert!(!n.contains(".exe"), "未去后缀: {n}");
            assert_eq!(*n, n.to_lowercase(), "未小写: {n}");
        }
    }
}
