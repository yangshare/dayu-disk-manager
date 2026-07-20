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
    // 磁盘根（如 C:\）标准 \\?\ 长路径形式为 \\?\C:\（单尾随反斜杠）
    if trimmed.len() == 3 && trimmed.as_bytes().get(1) == Some(&b':') && trimmed.ends_with('\\') {
        return format!(r"\\?\{}", trimmed);
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
    // GetDiskFreeSpaceExW requires the path (or one of its parents) to exist.
    // A migration repository is intentionally allowed to be created lazily, so
    // probe the nearest existing ancestor instead of rejecting a new directory.
    let probe_path = existing_path_for_probe(path);
    let wide = to_wide(&to_long_path(&path_to_str(&probe_path)));
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
                // Restart Manager 只对文件路径有效，对目录路径 RmGetList 会返回
                // ERROR_ACCESS_DENIED（Win32 已知限制，目录可能含万级文件无法下钻）。
                // 因此本函数对目录路径无效——此处保留 Ok(None) 语义（不改签名/行为）。
                // 目录级占用检测应由上层（safety，T9）用预设进程名（match_processes）实现。
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

/// Return `path` when it exists, otherwise walk up to the nearest existing
/// parent.  This keeps disk/volume probing useful for paths that will be
/// created during migration (for example `D:\\Migrated` on an existing D:).
#[cfg(windows)]
fn existing_path_for_probe(path: &Path) -> PathBuf {
    let mut candidate = path.to_path_buf();
    while !candidate.exists() {
        let Some(parent) = candidate.parent() else { break };
        if parent == candidate {
            break;
        }
        candidate = parent.to_path_buf();
    }
    candidate
}

// ===== MFT ABI 边界（T0：最小只读） =====
//
// 这一段是 T0 任务的 ABI 边界锁定：直接使用 windows crate 0.62 生成的
// NTFS_*/FSCTL_* 绑定，不定义删减字段的同名 repr(C) 结构。所有可能引发
// 对齐 UB 的位置都通过字节读取或 read_unaligned 访问字段。

/// 卷句柄与文件系统名称，供 read_volume_data / read_mft_record 复用。
///
/// 仅 T0 最小只读：句柄由 `open_volume` 创建，由 `Drop` 关闭。
#[cfg(windows)]
pub struct VolumeHandle {
    handle: windows::Win32::Foundation::HANDLE,
    /// 实际文件系统名称（小写），非 NTFS 时供错误构造使用。
    /// T0 保留字段，T1 整合到完整接口后可能暴露。
    #[allow(dead_code)]
    fs_name: String,
}

#[cfg(windows)]
impl Drop for VolumeHandle {
    fn drop(&mut self) {
        // 释放时忽略关闭错误：句柄生命周期已结束，调用方无法恢复。
        let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// 卷读取失败的分类错误。T0 仅引入枚举本身，T1 才整合到全局 AppError。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeError {
    /// `ERROR_ACCESS_DENIED` 精确映射——非管理员、卷被独占等。
    AccessDenied,
    /// 卷并非 NTFS（`fs_name` 为实际文件系统名称，例如 `fat32`）。
    UnsupportedFilesystem { actual: String },
    /// 输出缓冲返回的字节数或字段值不合法（截断、零填充被误当字段等）。
    InvalidData,
    /// 其它 Win32 I/O 错误，保留数值 code 与触发操作名。
    Io { code: u32, operation: &'static str },
}

impl std::fmt::Display for VolumeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VolumeError::AccessDenied => f.write_str("访问被拒绝（需要管理员权限或卷被独占）"),
            VolumeError::UnsupportedFilesystem { actual } => write!(
                f,
                "不支持的文件系统（仅支持 NTFS，实际为 {actual}）"
            ),
            VolumeError::InvalidData => f.write_str("卷数据缓冲不合法或被截断"),
            VolumeError::Io { code, operation } => {
                write!(f, "Win32 I/O 错误：操作={operation} code={code}")
            }
        }
    }
}

impl std::error::Error for VolumeError {}

/// 单条 MFT 记录的字节视图（`file_reference` 取低 48 位记录号语义，
/// `bytes` 为 `FileRecordBuffer` 的实际有效字节）。
#[derive(Debug, Clone)]
pub struct RawFileRecord {
    pub file_reference: u64,
    pub bytes: Vec<u8>,
}

/// 卷几何与版本信息，已校验字段。`slot_count` 为按
/// `bytes_per_file_record_segment` 向上取整得到的总槽位数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VolumeData {
    pub bytes_per_sector: u32,
    pub bytes_per_cluster: u32,
    pub bytes_per_file_record_segment: u32,
    pub mft_valid_data_length: u64,
    pub major_version: u16,
    pub minor_version: u16,
    pub slot_count: u64,
}

// ===== 纯函数：缓冲解析（无 Win32 调用，单测可驱动） =====
//
// 这些函数接收 &[u8] 或对齐缓冲，把可单测的校验逻辑从 Win32 调用中剥离。
// 真正的 DeviceIoControl 只在 read_volume_data / read_mft_record 里调用。
//
// NTFS_VOLUME_DATA_BUFFER 字段偏移（按 windows crate 0.62 绑定的 repr(C)）：
//   VolumeSerialNumber: i64 @ 0
//   NumberSectors: i64 @ 8
//   TotalClusters: i64 @ 16
//   FreeClusters: i64 @ 24
//   TotalReserved: i64 @ 32
//   BytesPerSector: u32 @ 40
//   BytesPerCluster: u32 @ 44
//   BytesPerFileRecordSegment: u32 @ 48
//   ClustersPerFileRecordSegment: u32 @ 52
//   MftValidDataLength: i64 @ 56
//   ... 后续字段本任务不读
//
// 直接以字节读取字段，避免把任意 Vec<u8> 指针重解释成对齐结构造成 UB。

/// 校验并解析 NTFS_VOLUME_DATA_BUFFER（无扩展）字节切片。
///
/// `bytes_returned` 是 DeviceIoControl 实际写入的字节数，必须覆盖到
/// 至少 MftValidDataLength 字段（offset 56 + 8 = 64 字节）。
fn parse_volume_data_buffer(bytes: &[u8]) -> Result<VolumeData, VolumeError> {
    // 至少需要读到 MftValidDataLength 末尾（offset 56 + 8 = 64）。
    const MIN_BYTES_FOR_VOLUME_DATA: usize = 64;
    if bytes.len() < MIN_BYTES_FOR_VOLUME_DATA {
        return Err(VolumeError::InvalidData);
    }
    let bytes_per_sector = read_u32_at(bytes, 40)?;
    let bytes_per_cluster = read_u32_at(bytes, 44)?;
    let bytes_per_file_record_segment = read_u32_at(bytes, 48)?;
    let mft_valid_data_length = read_u64_at(bytes, 56)?;

    if bytes_per_file_record_segment == 0 {
        // 避免除零；只可能由零填充被误解析成字段触发。
        return Err(VolumeError::InvalidData);
    }

    let slot_count = mft_valid_data_length
        .div_ceil(u64::from(bytes_per_file_record_segment));

    Ok(VolumeData {
        bytes_per_sector,
        bytes_per_cluster,
        bytes_per_file_record_segment,
        mft_valid_data_length,
        // 版本号在 NTFS_EXTENDED_VOLUME_DATA 中，此处暂为 0/0；扩展解析后覆盖。
        major_version: 0,
        minor_version: 0,
        slot_count,
    })
}

/// 校验并解析 NTFS_EXTENDED_VOLUME_DATA 字节切片。
///
/// 关键事实（简报 0.1）：
/// - `ByteCount` 位于 offset 0，版本号**不**在 offset 0。
/// - 字段布局（windows 0.62 绑定）：
///     ByteCount: u32 @ 0
///     MajorVersion: u16 @ 4
///     MinorVersion: u16 @ 6
///     BytesPerPhysicalSector: u32 @ 8
///     ...
/// - 必须按实际 `bytes_returned` 判断扩展结构是否完整，且校验 `ByteCount`
///   真实反映内容大小（不是预分配容量）。
fn parse_extended_volume_data(
    bytes: &[u8],
    bytes_returned: usize,
) -> Result<(u16, u16), VolumeError> {
    // 至少要包含 ByteCount(4) + Major(2) + Minor(2) = 8 字节。
    const MIN_EXTENDED_BYTES: usize = 8;
    if bytes_returned < MIN_EXTENDED_BYTES {
        // 扩展结构不完整——按简报：返回 InvalidData，不得把零填充区解析成版本号。
        return Err(VolumeError::InvalidData);
    }
    if bytes.len() < bytes_returned {
        // bytes_returned 超过切片容量，本身就是错误契约。
        return Err(VolumeError::InvalidData);
    }
    let byte_count = read_u32_at(bytes, 0)?;
    // ByteCount 是驱动实际写入的字节数（不含自身字段），用于校验完整性。
    // 我们要求它至少覆盖到 MinorVersion（4 字节）。
    if (byte_count as usize) < 4 {
        return Err(VolumeError::InvalidData);
    }
    // ByteCount + 4（自身头部）不能多于驱动实际返回的字节数。
    if (byte_count as usize + 4) > bytes_returned {
        return Err(VolumeError::InvalidData);
    }
    let major = read_u16_at(bytes, 4)?;
    let minor = read_u16_at(bytes, 6)?;
    Ok((major, minor))
}

/// 从字节切片读取小端 u16，越界返回 InvalidData。
fn read_u16_at(bytes: &[u8], offset: usize) -> Result<u16, VolumeError> {
    let slice = bytes.get(offset..offset + 2).ok_or(VolumeError::InvalidData)?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

/// 从字节切片读取小端 u32，越界返回 InvalidData。
fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32, VolumeError> {
    let slice = bytes.get(offset..offset + 4).ok_or(VolumeError::InvalidData)?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// 从字节切片读取小端 u64，越界返回 InvalidData。
fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64, VolumeError> {
    let slice = bytes.get(offset..offset + 8).ok_or(VolumeError::InvalidData)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(slice);
    Ok(u64::from_le_bytes(arr))
}

/// `NTFS_FILE_RECORD_OUTPUT_BUFFER` 中 `FileRecordBuffer` 的字节偏移。
///
/// 用 `offset_of!` 取值（简报 0.1 要求），而非硬编码 8/12/16。
/// 该函数在 windows 平台返回真实绑定偏移；非 windows 平台返回字节
/// 解析器所用的硬编码常量（仅用于让示例与测试在非 windows 上也编译）。
#[cfg(windows)]
fn file_record_buffer_offset() -> usize {
    use windows::Win32::System::Ioctl::NTFS_FILE_RECORD_OUTPUT_BUFFER;
    // SAFETY: offset_of! 是 const 求值，不解引用任何指针。
    core::mem::offset_of!(NTFS_FILE_RECORD_OUTPUT_BUFFER, FileRecordBuffer)
}

#[cfg(not(windows))]
fn file_record_buffer_offset() -> usize {
    // FileReferenceNumber(i64)@0 + FileRecordLength(u32)@8 -> FileRecordBuffer@12
    // repr(C) 下 [u8; 1] 紧跟在 u32 后，无填充，对齐为 1。
    12
}

/// 校验并解析 `FSCTL_GET_NTFS_FILE_RECORD` 的输出缓冲。
///
/// 参数：
/// - `output_buf`：传给 DeviceIoControl 的输出缓冲（字节视图）
/// - `bytes_returned`：DeviceIoControl 实际写入的字节数
/// - `capacity`：output_buf 总容量（== output_buf.len()）
///
/// 同时校验三件事（简报 0.1）：
/// 1. `bytes_returned` 足够容纳 output header（FileReferenceNumber + FileRecordLength）。
/// 2. `FileRecordLength` 不越界（offset + len <= capacity）。
/// 3. 切出的记录字节不超出 `bytes_returned`。
///
/// 成功时返回 `(file_reference_low48, record_bytes)`。
fn parse_file_record_output(
    output_buf: &[u8],
    bytes_returned: usize,
    capacity: usize,
) -> Result<(u64, &[u8]), VolumeError> {
    if capacity == 0 || output_buf.len() < capacity {
        return Err(VolumeError::InvalidData);
    }
    let header_offset = file_record_buffer_offset();
    // output header 至少包含 8 字节 FileReferenceNumber + 4 字节 FileRecordLength。
    // 用 header_offset（>= 12）作为最小可用大小。
    if bytes_returned < header_offset {
        return Err(VolumeError::InvalidData);
    }
    // bytes_returned 不能超过容量。
    if bytes_returned > capacity {
        return Err(VolumeError::InvalidData);
    }

    let file_reference = read_u64_at(output_buf, 0)?;
    // FileRecordLength 紧跟 FileReferenceNumber 之后（offset 8）。
    let file_record_length = read_u32_at(output_buf, 8)? as usize;

    let end = header_offset
        .checked_add(file_record_length)
        .ok_or(VolumeError::InvalidData)?;
    if end > capacity {
        return Err(VolumeError::InvalidData);
    }
    if end > bytes_returned {
        return Err(VolumeError::InvalidData);
    }

    let record_bytes = output_buf
        .get(header_offset..end)
        .ok_or(VolumeError::InvalidData)?;
    // 仅保留低 48 位（记录号），丢弃高 16 位序列号。
    let file_ref_low48 = file_reference & 0x0000_FFFF_FFFF_FFFF;
    Ok((file_ref_low48, record_bytes))
}

/// 把 `windows::core::Error` 映射到 `VolumeError`。
///
/// 简报 0.1：`ERROR_ACCESS_DENIED` 精确映射为 AccessDenied，其余保留 code。
#[cfg(windows)]
fn map_win32_error(e: windows::core::Error, operation: &'static str) -> VolumeError {
    use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
    let hr = e.code().0 as u32;
    // HRESULT_FROM_WIN32(code) = (code & 0xFFFF) | (7 << 16) | 0x80000000
    // 反推：低 16 位即 Win32 错误码。
    let win32_code = if (hr & 0xFFFF_0000) == 0x8007_0000 {
        hr & 0x0000_FFFF
    } else {
        hr
    };
    if win32_code == ERROR_ACCESS_DENIED.0 {
        VolumeError::AccessDenied
    } else {
        VolumeError::Io {
            code: win32_code,
            operation,
        }
    }
}

// ===== 真实 Win32 调用（仅 windows 平台编译） =====

/// 打开卷设备（如 `\\.\C:`），要求只读访问。
///
/// T0 最小版本：
/// - `dwDesiredAccess` 取 `FILE_READ_ATTRIBUTES`（只读，不需要管理员读取数据）；
///   后续 IOCTL 实际只需要卷属性读取权限。
/// - `dwShareMode` 允许 READ|WRITE|DELETE 共享，避免与其它进程冲突。
/// - 非管理员对部分卷会返回 `ERROR_ACCESS_DENIED`，本函数保留该错误。
#[cfg(windows)]
pub fn open_volume(drive_letter: char) -> Result<VolumeHandle, VolumeError> {
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_CREATION_DISPOSITION, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_SHARE_DELETE, FILE_READ_ATTRIBUTES, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    let drive = drive_letter.to_ascii_uppercase();
    if !drive.is_ascii_alphabetic() {
        return Err(VolumeError::Io {
            code: 0,
            operation: "open_volume/parse_letter",
        });
    }
    let path = format!(r"\\.\{}:", drive);

    // 通过现有 volume_info 路径（同盘符）取得实际文件系统名称，避免对非 NTFS 卷
    // 调用 NTFS 专属 IOCTL（简报 0.1 要求）。
    let fs_root = PathBuf::from(format!(r"{}:\", drive));
    let (_, is_ntfs) = volume_info(&fs_root).map_err(|_e| VolumeError::Io {
        code: 0,
        operation: "open_volume/volume_info",
    })?;
    // 重新查询一次拿到名称（volume_info 当前只回 bool；若非 NTFS 在此构造错误）。
    let fs_name = if is_ntfs {
        "ntfs".to_string()
    } else {
        // 真正查实际文件系统名（用于错误信息）。
        match query_fs_name(&fs_root) {
            Some(name) => name,
            None => "unknown".to_string(),
        }
    };
    if fs_name != "ntfs" {
        return Err(VolumeError::UnsupportedFilesystem { actual: fs_name });
    }

    let wide = to_wide(&path);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0),
            None,
            FILE_CREATION_DISPOSITION(OPEN_EXISTING.0),
            FILE_FLAGS_AND_ATTRIBUTES(FILE_ATTRIBUTE_NORMAL.0),
            None,
        )
    }
    .map_err(|e| map_win32_error(e, "CreateFileW"))?;

    Ok(VolumeHandle { handle, fs_name })
}

#[cfg(windows)]
fn query_fs_name(path: &Path) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;
    let root = volume_root(path).ok()?;
    let wide = to_wide(&to_long_path(&root));
    let mut serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut flags: u32 = 0;
    let mut fs_name_buf = [0u16; 256];
    unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_ptr()),
            None,
            Some(&mut serial as *mut u32),
            Some(&mut max_component as *mut u32),
            Some(&mut flags as *mut u32),
            Some(&mut fs_name_buf),
        )
        .ok()?;
    }
    Some(from_wide(&fs_name_buf).to_lowercase())
}

/// 读取卷几何 + NTFS 扩展版本。返回 `VolumeData`（已校验）。
#[cfg(windows)]
pub fn read_volume_data(vol: &VolumeHandle) -> Result<VolumeData, VolumeError> {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::FSCTL_GET_NTFS_VOLUME_DATA;

    // 用对齐到 i64（8 字节）的缓冲区存放输出。
    // NTFS_VOLUME_DATA_BUFFER + NTFS_EXTENDED_VOLUME_DATA 可能都被写到这里。
    // 微软文档：输出缓冲先放 NTFS_VOLUME_DATA_BUFFER，若容量更大则追加
    // NTFS_EXTENDED_VOLUME_DATA。两者大小都按目标结构对齐。
    let mut out: Vec<u64> = vec![0u64; 32]; // 256 字节，远大于两结构之和
    let cap_bytes = out.len() * 8;
    let mut bytes_returned: u32 = 0;
    let result = unsafe {
        DeviceIoControl(
            vol.handle,
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some(out.as_mut_ptr() as *mut core::ffi::c_void),
            cap_bytes as u32,
            Some(&mut bytes_returned as *mut u32),
            None,
        )
    };
    result.map_err(|e| map_win32_error(e, "DeviceIoControl/GET_NTFS_VOLUME_DATA"))?;

    if (bytes_returned as usize) > cap_bytes {
        return Err(VolumeError::InvalidData);
    }
    let bytes: &[u8] = unsafe {
        std::slice::from_raw_parts(out.as_ptr() as *const u8, bytes_returned as usize)
    };

    let mut data = parse_volume_data_buffer(bytes)?;

    // 扩展结构位于基础结构之后；按实际返回长度判断是否存在。
    // NTFS_VOLUME_DATA_BUFFER 大小 = 10 个 i64(8) + 4 个 u32(4) = 96 字节
    // （windows 0.62 绑定）。DeviceIoControl 在输出缓冲足够时紧随其后
    // 追加 NTFS_EXTENDED_VOLUME_DATA（按微软文档）。
    const BASE_VOLUME_DATA_SIZE: usize = 96;
    if (bytes_returned as usize) > BASE_VOLUME_DATA_SIZE {
        let ext_bytes = &bytes[BASE_VOLUME_DATA_SIZE..];
        let ext_returned = (bytes_returned as usize) - BASE_VOLUME_DATA_SIZE;
        // 扩展结构存在——尝试解析。失败不致命，回退到 0/0 版本。
        if let Ok((major, minor)) = parse_extended_volume_data(ext_bytes, ext_returned) {
            data.major_version = major;
            data.minor_version = minor;
        }
    }

    Ok(data)
}

/// 读取指定文件参考号对应的 MFT 记录。
///
/// - `file_reference`：请求的参考号（任意 64 位；驱动会向下取到最近的有效记录）。
/// - `record_capacity_bytes`：单条记录最大容量，通常等于
///   `bytes_per_file_record_segment`（由 `VolumeData` 提供）。
#[cfg(windows)]
pub fn read_mft_record(
    vol: &VolumeHandle,
    file_reference: u64,
    record_capacity_bytes: u32,
) -> Result<RawFileRecord, VolumeError> {
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FSCTL_GET_NTFS_FILE_RECORD, NTFS_FILE_RECORD_INPUT_BUFFER,
    };

    let header_offset = file_record_buffer_offset();
    let buf_size = header_offset
        .checked_add(record_capacity_bytes as usize)
        .ok_or(VolumeError::InvalidData)?;
    if buf_size == 0 {
        return Err(VolumeError::InvalidData);
    }
    // Vec<u64> 保证 8 字节对齐，匹配 NTFS_FILE_RECORD_OUTPUT_BUFFER 的 i64 字段。
    let words = buf_size.div_ceil(8);
    let mut out: Vec<u64> = vec![0u64; words];
    let cap_bytes = words * 8;

    let mut input = NTFS_FILE_RECORD_INPUT_BUFFER {
        FileReferenceNumber: file_reference as i64,
    };
    let mut bytes_returned: u32 = 0;
    let result = unsafe {
        DeviceIoControl(
            vol.handle,
            FSCTL_GET_NTFS_FILE_RECORD,
            Some(&mut input as *mut _ as *mut core::ffi::c_void),
            core::mem::size_of::<NTFS_FILE_RECORD_INPUT_BUFFER>() as u32,
            Some(out.as_mut_ptr() as *mut core::ffi::c_void),
            cap_bytes as u32,
            Some(&mut bytes_returned as *mut u32),
            None,
        )
    };
    result.map_err(|e| map_win32_error(e, "DeviceIoControl/GET_NTFS_FILE_RECORD"))?;

    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(out.as_ptr() as *const u8, cap_bytes) };
    let (file_ref_low48, record_bytes) =
        parse_file_record_output(bytes, bytes_returned as usize, cap_bytes)?;

    Ok(RawFileRecord {
        file_reference: file_ref_low48,
        bytes: record_bytes.to_vec(),
    })
}

// ===== 非 windows 平台 stub（保证 cargo build 通过） =====

#[cfg(not(windows))]
pub struct VolumeHandle;

#[cfg(not(windows))]
pub fn open_volume(_drive_letter: char) -> Result<VolumeHandle, VolumeError> {
    Err(VolumeError::Io {
        code: 0,
        operation: "open_volume/not_windows",
    })
}

#[cfg(not(windows))]
pub fn read_volume_data(_vol: &VolumeHandle) -> Result<VolumeData, VolumeError> {
    Err(VolumeError::Io {
        code: 0,
        operation: "read_volume_data/not_windows",
    })
}

#[cfg(not(windows))]
pub fn read_mft_record(
    _vol: &VolumeHandle,
    _file_reference: u64,
    _record_capacity_bytes: u32,
) -> Result<RawFileRecord, VolumeError> {
    Err(VolumeError::Io {
        code: 0,
        operation: "read_mft_record/not_windows",
    })
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
        assert_eq!(to_long_path("C:\\"), r"\\?\C:\");
    }

    #[test]
    fn disk_free_space_nonzero_on_temp() {
        let dir = TempDir::new().unwrap();
        let free = disk_free_bytes(dir.path()).unwrap();
        assert!(free > 0);
    }

    #[test]
    fn disk_free_space_accepts_nonexistent_child() {
        let dir = TempDir::new().unwrap();
        let child = dir.path().join("not-created-yet").join("nested");
        let free = disk_free_bytes(&child).unwrap();
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
    fn volume_info_on_drive_root_succeeds() {
        // 验证磁盘根经 to_long_path 转为 \\?\C:\（单尾随反斜杠）后，
        // GetVolumeInformationW 能成功返回——确认不再因双斜杠被 Win32 API 拒绝。
        let (serial, _is_ntfs) = volume_info(Path::new("C:\\")).unwrap();
        assert!(!serial.is_empty(), "volume_info on C:\\ returned empty serial");
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

    // ===== T0 ABI 单测（0.2） =====
    //
    // 这些测试只驱动纯函数（缓冲校验、字节解析、错误映射、offset 断言），
    // 不调用真实 DeviceIoControl（需要管理员权限和真实卷）。

    /// 0.2-1：断言生成绑定中的 FileRecordBuffer offset 与分配/切片逻辑一致。
    ///
    /// `NTFS_FILE_RECORD_OUTPUT_BUFFER` 布局（windows 0.62 绑定）：
    ///   FileReferenceNumber: i64 @ 0
    ///   FileRecordLength: u32 @ 8
    ///   FileRecordBuffer: [u8; 1] @ 12
    /// 即 FileRecordBuffer offset == 12。
    #[test]
    fn file_record_buffer_offset_matches_binding() {
        let offset = file_record_buffer_offset();
        // repr(C) 下 FileReferenceNumber(8) + FileRecordLength(4) = 12
        // [u8; 1] 对齐为 1，无额外填充
        assert_eq!(offset, 12, "FileRecordBuffer offset 必须为 12（FileReferenceNumber(8) + FileRecordLength(4)）");
    }

    /// 0.2-2a：给定短于 output header 的缓冲，返回错误且不 panic。
    #[test]
    fn parse_file_record_output_too_short_for_header() {
        let buf = vec![0u8; 8]; // 仅 8 字节，不足 header（需 >= offset 12）
        let result = parse_file_record_output(&buf, 8, 8);
        assert!(result.is_err(), "短于 header 的缓冲必须返回错误");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-2b：FileRecordLength 越界（超出缓冲容量）。
    #[test]
    fn parse_file_record_output_record_length_overflows_capacity() {
        let mut buf = vec![0u8; 32];
        // FileReferenceNumber = 5
        buf[0..8].copy_from_slice(&5u64.to_le_bytes());
        // FileRecordLength = 1024（远超剩余容量 32 - 12 = 20 字节）
        buf[8..12].copy_from_slice(&1024u32.to_le_bytes());
        let result = parse_file_record_output(&buf, 32, 32);
        assert!(result.is_err(), "FileRecordLength 越界必须返回错误");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-2c：bytes_returned 不足（小于 header + FileRecordLength）。
    #[test]
    fn parse_file_record_output_bytes_returned_insufficient() {
        let mut buf = vec![0u8; 64];
        // FileReferenceNumber = 5
        buf[0..8].copy_from_slice(&5u64.to_le_bytes());
        // FileRecordLength = 32
        buf[8..12].copy_from_slice(&32u32.to_le_bytes());
        // bytes_returned = 20（仅覆盖 header + 8 字节记录，不够 32 字节）
        let result = parse_file_record_output(&buf, 20, 64);
        assert!(result.is_err(), "bytes_returned 不足必须返回错误");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-2d：bytes_returned 超过容量。
    #[test]
    fn parse_file_record_output_bytes_returned_exceeds_capacity() {
        let buf = vec![0u8; 32];
        let result = parse_file_record_output(&buf, 64, 32);
        assert!(result.is_err(), "bytes_returned 超容量必须返回错误");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-2e：合法输出正确解析。
    #[test]
    fn parse_file_record_output_valid() {
        let offset = file_record_buffer_offset();
        let record_len = 1024usize; // 标准 1 KiB 记录
        let cap = offset + record_len;
        let mut buf = vec![0u8; cap];
        // FileReferenceNumber = 42（低 48 位）
        buf[0..8].copy_from_slice(&42u64.to_le_bytes());
        // FileRecordLength = 1024
        buf[8..12].copy_from_slice(&1024u32.to_le_bytes());
        // 填充记录体为 0xAA
        for b in buf.iter_mut().skip(offset).take(record_len) {
            *b = 0xAA;
        }
        let bytes_returned = cap;
        let result = parse_file_record_output(&buf, bytes_returned, cap);
        assert!(result.is_ok(), "合法缓冲应成功解析");
        let (file_ref, record_bytes) = result.unwrap();
        assert_eq!(file_ref, 42, "低 48 位记录号应为 42");
        assert_eq!(record_bytes.len(), 1024);
        assert!(record_bytes.iter().all(|&b| b == 0xAA), "记录体内容应全为 0xAA");
    }

    /// 0.2-2f：FileReferenceNumber 高位（序列号）被剥离，只保留低 48 位。
    #[test]
    fn parse_file_record_output_strips_sequence_number() {
        let mut buf = vec![0u8; 64];
        // 设置高 16 位序列号 = 0x0001
        let full_ref: u64 = (1u64 << 48) | 42u64;
        buf[0..8].copy_from_slice(&full_ref.to_le_bytes());
        buf[8..12].copy_from_slice(&16u32.to_le_bytes());
        let offset = file_record_buffer_offset();
        let result = parse_file_record_output(&buf, offset + 16, 64);
        assert!(result.is_ok());
        let (file_ref, _) = result.unwrap();
        assert_eq!(file_ref, 42, "高位序列号应被剥离，仅保留低 48 位");
    }

    /// 0.2-3a：给定缺失的扩展卷数据（bytes_returned < 8），返回 InvalidData。
    #[test]
    fn parse_extended_volume_data_too_short() {
        let bytes = vec![0u8; 4]; // 不足 8 字节
        let result = parse_extended_volume_data(&bytes, 4);
        assert!(result.is_err(), "截断扩展数据必须返回错误");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-3b：给定截断的扩展卷数据（ByteCount 声称有版本号但 bytes_returned 不够）。
    #[test]
    fn parse_extended_volume_data_byte_count_exceeds_returned() {
        let mut bytes = vec![0u8; 16];
        // ByteCount = 100（声称写入 100 字节，不含自身 4 字节）
        bytes[0..4].copy_from_slice(&100u32.to_le_bytes());
        // MajorVersion = 3, MinorVersion = 1
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
        // bytes_returned = 8（但 ByteCount + 4 = 104 > 8）
        let result = parse_extended_volume_data(&bytes, 8);
        assert!(result.is_err(), "ByteCount 超过实际返回必须返回错误");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-3c：零填充不得被解析成版本号（ByteCount=0 应返回 InvalidData）。
    #[test]
    fn parse_extended_volume_data_zero_byte_count_is_invalid() {
        let mut bytes = vec![0u8; 16];
        // ByteCount = 0（全部零填充）
        bytes[0..4].copy_from_slice(&0u32.to_le_bytes());
        // 后续全零——如果 ByteCount=0 被错误跳过，就会把零当成版本号。
        let result = parse_extended_volume_data(&bytes, 16);
        assert!(result.is_err(), "ByteCount=0 必须返回 InvalidData，不得把零填充当版本号");
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-3d：合法扩展卷数据正确解析。
    #[test]
    fn parse_extended_volume_data_valid() {
        let mut bytes = vec![0u8; 32];
        // ByteCount = 8（4 字节版本号 + 4 字节 BytesPerPhysicalSector）
        bytes[0..4].copy_from_slice(&8u32.to_le_bytes());
        // MajorVersion = 3, MinorVersion = 1
        bytes[4..6].copy_from_slice(&3u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
        let result = parse_extended_volume_data(&bytes, 32);
        assert!(result.is_ok());
        let (major, minor) = result.unwrap();
        assert_eq!(major, 3);
        assert_eq!(minor, 1);
    }

    /// 0.2-3e：bytes_returned 超过切片容量。
    #[test]
    fn parse_extended_volume_data_returned_exceeds_slice() {
        let bytes = vec![0u8; 4]; // 切片只有 4 字节
        let result = parse_extended_volume_data(&bytes, 16); // 声称返回了 16 字节
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 0.2-4a：ERROR_ACCESS_DENIED (5) 精确映射为 VolumeError::AccessDenied。
    #[cfg(windows)]
    #[test]
    fn map_win32_error_access_denied() {
        use windows::Win32::Foundation::ERROR_ACCESS_DENIED;
        // 构造 HRESULT_FROM_WIN32(5) = 0x80070005
        let hr = windows::core::HRESULT::from_win32(ERROR_ACCESS_DENIED.0);
        let err = windows::core::Error::from_hresult(hr);
        let mapped = map_win32_error(err, "test_op");
        assert_eq!(mapped, VolumeError::AccessDenied);
    }

    /// 0.2-4b：非 ACCESS_DENIED 的 Win32 错误保留 code。
    #[cfg(windows)]
    #[test]
    fn map_win32_error_generic_preserves_code() {
        // ERROR_INVALID_FUNCTION = 1
        let hr = windows::core::HRESULT::from_win32(1);
        let err = windows::core::Error::from_hresult(hr);
        let mapped = map_win32_error(err, "test_op");
        match mapped {
            VolumeError::Io { code, operation } => {
                assert_eq!(code, 1);
                assert_eq!(operation, "test_op");
            }
            other => panic!("期望 Io 变体，得到 {:?}", other),
        }
    }

    /// 补充：parse_volume_data_buffer 合法输入。
    #[test]
    fn parse_volume_data_buffer_valid() {
        let mut bytes = vec![0u8; 128];
        // BytesPerSector = 512
        bytes[40..44].copy_from_slice(&512u32.to_le_bytes());
        // BytesPerCluster = 4096
        bytes[44..48].copy_from_slice(&4096u32.to_le_bytes());
        // BytesPerFileRecordSegment = 1024
        bytes[48..52].copy_from_slice(&1024u32.to_le_bytes());
        // MftValidDataLength = 1048576 (1 MB)
        bytes[56..64].copy_from_slice(&1_048_576u64.to_le_bytes());

        let result = parse_volume_data_buffer(&bytes);
        assert!(result.is_ok());
        let data = result.unwrap();
        assert_eq!(data.bytes_per_sector, 512);
        assert_eq!(data.bytes_per_cluster, 4096);
        assert_eq!(data.bytes_per_file_record_segment, 1024);
        assert_eq!(data.mft_valid_data_length, 1_048_576);
        // slot_count = 1_048_576 / 1024 = 1024
        assert_eq!(data.slot_count, 1024);
    }

    /// 补充：parse_volume_data_buffer 截断缓冲返回 InvalidData。
    #[test]
    fn parse_volume_data_buffer_truncated() {
        let bytes = vec![0u8; 32]; // 不足 64 字节
        let result = parse_volume_data_buffer(&bytes);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), VolumeError::InvalidData);
    }

    /// 补充：BytesPerFileRecordSegment = 0 返回 InvalidData（防除零）。
    #[test]
    fn parse_volume_data_buffer_zero_record_size() {
        let mut bytes = vec![0u8; 128];
        bytes[40..44].copy_from_slice(&512u32.to_le_bytes());
        bytes[44..48].copy_from_slice(&4096u32.to_le_bytes());
        // BytesPerFileRecordSegment = 0
        bytes[48..52].copy_from_slice(&0u32.to_le_bytes());
        bytes[56..64].copy_from_slice(&1_048_576u64.to_le_bytes());

        let result = parse_volume_data_buffer(&bytes);
        assert!(result.is_err(), "零 BytesPerFileRecordSegment 必须返回 InvalidData");
    }

    /// 补充：slot_count 向上取整验证。
    #[test]
    fn parse_volume_data_buffer_slot_count_ceiling() {
        let mut bytes = vec![0u8; 128];
        bytes[40..44].copy_from_slice(&512u32.to_le_bytes());
        bytes[44..48].copy_from_slice(&4096u32.to_le_bytes());
        bytes[48..52].copy_from_slice(&1024u32.to_le_bytes());
        // MftValidDataLength = 1025（不能被 1024 整除）
        bytes[56..64].copy_from_slice(&1025u64.to_le_bytes());

        let data = parse_volume_data_buffer(&bytes).unwrap();
        // 1025 / 1024 向上取整 = 2
        assert_eq!(data.slot_count, 2);
    }

    /// 补充：VolumeError::UnsupportedFilesystem 格式化。
    #[test]
    fn volume_error_unsupported_filesystem_display() {
        let err = VolumeError::UnsupportedFilesystem {
            actual: "fat32".into(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("fat32"), "错误信息应包含实际文件系统名");
    }
}
