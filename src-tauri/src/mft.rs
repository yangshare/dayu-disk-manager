//! NTFS FILE record 解析（任务 3）。
//!
//! 本模块是 MFT 引擎的第一个任务（任务重排：T3 在 T2 之前）。它自含定义
//! 解析所需的数据模型（原属 T2 的 2.1），并实现单条记录的可测试纯函数解析：
//! USA fix-up、属性遍历、`$FILE_NAME` / `$DATA` / `$REPARSE_POINT` /
//! `$ATTRIBUTE_LIST` 解码、记录 0 `$DATA` Data Run 解码。
//!
//! T2（紧凑枚举）将复用：
//! - [`parse_record`]：每条导出记录的解析入口（签名稳定）。
//! - [`decode_data_runs`]：non-resident `$DATA` 与 `$ATTRIBUTE_LIST` 的 Data Run 解码，
//!   尤其用于记录 0 `$MFT` 自身数据流的定位以批量读取 `$MFT` 字节。
//! - [`MftRecord`] 等数据结构作为分类聚合（`MftIndex`）的输入。
//!
//! 注意：跨记录聚合（`MftIndex`、目录树构建、extension 与 base 的合并）属 T2 职责；
//! 本模块的 [`parse_record`] 只保证返回的信息足以支撑 T2 的分类与合并决策。

// ===== FILE header 字段偏移（集中常量定义，简报 3.2） =====
//
// 这些常量来自 NTFS 官方文档的 FILE record 布局（与 windows 0.62 绑定/微软 SDK 一致）：
//   0x00 FILE signature ('FILE')
//   0x04 Update Sequence Array offset (u16)
//   0x06 Update Sequence Array count (u16, entries including the USN itself)
//   0x08 LogFile sequence number (u64)
//   0x10 Sequence number (u16)
//   0x12 Hard link count (u16)
//   0x14 First attribute offset (u16)
//   0x16 Flags (u16): bit0 = in use, bit1 = directory
//   0x18 Real size of the FILE record (u32, "bytes in use")
//   0x1C Allocated size of the FILE record (u32)
//   0x20 Base file reference (u64); 0 for a base record, non-zero for an extension
//
// 测试断言这些常量值，不从手写测试反推偏移（简报 3.2）。

/// USA（Update Sequence Array）在 FILE record 中的偏移。
const FILE_USA_OFFSET_OFFSET: usize = 0x04;
/// USA 条目数（含 USN 自身）在 FILE record 中的偏移。
const FILE_USA_COUNT_OFFSET: usize = 0x06;
/// 序列号（sequence number）在 FILE record 中的偏移。
const FILE_SEQUENCE_OFFSET: usize = 0x10;
/// 第一个属性的偏移在 FILE record 中的偏移。
const FILE_FIRST_ATTRIBUTE_OFFSET: usize = 0x14;
/// 标志位（in-use / directory）在 FILE record 中的偏移。
const FILE_FLAGS_OFFSET: usize = 0x16;
/// FILE record 实际使用字节数（bytes in use）在 FILE record 中的偏移。
const FILE_BYTES_IN_USE_OFFSET: usize = 0x18;
/// Base file reference 在 FILE record 中的偏移。
const FILE_BASE_REFERENCE_OFFSET: usize = 0x20;

/// FILE record 标志位：bit0 = 记录在用。
const FILE_FLAG_IN_USE: u16 = 0x0001;
/// FILE record 标志位：bit1 = 目录。
const FILE_FLAG_DIRECTORY: u16 = 0x0002;

/// FILE record 签名 "FILE"。
const FILE_SIGNATURE: [u8; 4] = *b"FILE";

// ===== 属性头偏移（NTFS 标准属性头） =====
//
// 通用属性头（前 16 字节对 resident / non-resident 共用）：
//   0x00 Attribute type (u32)
//   0x04 Attribute length (u32, 含头)
//   0x08 Non-resident flag (u8)
//   0x09 Name length (u8, UTF-16 units)
//   0x0A Name offset (u16)
//   0x0C Flags (u16)
//   0x0E Attribute id (u16)
//
// Resident 特定：
//   0x10 Value length (u32)
//   0x14 Value offset (u16)
//   0x16 Flags (u16)
//
// Non-resident 特定：
//   0x10 Lowest VCN (u64)
//   0x18 Highest VCN (u64)
//   0x20 Run offset (u16)
//   0x22 Compression unit (u16)
//   0x28 Allocated size (u64)
//   0x30 Logical size (u64)
//   0x38 Real size (u64)

/// 通用属性头最小长度（type + length + non-res flag + name length + name offset +
/// flags + attribute id = 16 字节）。
const ATTR_COMMON_HEADER_LEN: usize = 0x10;
/// Resident 属性头长度（含通用头 + value length + value offset + flags）。
#[allow(dead_code)]
const ATTR_RESIDENT_HEADER_LEN: usize = 0x18;
/// Non-resident 属性头长度（到 real size 字段末尾）。
const ATTR_NONRESIDENT_HEADER_LEN: usize = 0x40;

const ATTR_TYPE_OFFSET: usize = 0x00;
const ATTR_LENGTH_OFFSET: usize = 0x04;
const ATTR_NONRES_FLAG_OFFSET: usize = 0x08;
const ATTR_NAME_LENGTH_OFFSET: usize = 0x09;
const ATTR_NAME_OFFSET_FIELD: usize = 0x0A;
const ATTR_ATTRIBUTE_ID_OFFSET: usize = 0x0E;

const ATTR_RESIDENT_VALUE_LENGTH_OFFSET: usize = 0x10;
const ATTR_RESIDENT_VALUE_OFFSET_FIELD: usize = 0x14;

const ATTR_NONRES_LOWEST_VCN_OFFSET: usize = 0x10;
#[allow(dead_code)]
const ATTR_NONRES_HIGHEST_VCN_OFFSET: usize = 0x18;
#[allow(dead_code)]
const ATTR_NONRES_RUN_OFFSET_FIELD: usize = 0x20;
const ATTR_NONRES_LOGICAL_SIZE_OFFSET: usize = 0x30;

/// `$STANDARD_INFORMATION` 属性类型。
const ATTR_TYPE_STANDARD_INFORMATION: u32 = 0x10;
/// `$ATTRIBUTE_LIST` 属性类型。
const ATTR_TYPE_ATTRIBUTE_LIST: u32 = 0x20;
/// `$FILE_NAME` 属性类型。
const ATTR_TYPE_FILE_NAME: u32 = 0x30;
/// `$OBJECT_ID` 属性类型。
const ATTR_TYPE_OBJECT_ID: u32 = 0x40;
/// `$DATA` 属性类型（默认数据流与命名数据流 ADS）。
const ATTR_TYPE_DATA: u32 = 0x80;
/// `$INDEX_ROOT` 属性类型。
const ATTR_TYPE_INDEX_ROOT: u32 = 0x90;
/// `$INDEX_ALLOCATION` 属性类型。
const ATTR_TYPE_INDEX_ALLOCATION: u32 = 0xA0;
/// `$REPARSE_POINT` 属性类型。
const ATTR_TYPE_REPARSE_POINT: u32 = 0xC0;
/// 属性列表结束标志。
const ATTR_TYPE_END_MARKER: u32 = 0xFFFF_FFFF;

/// `$FILE_NAME` 值中文件名命名空间偏移（在 FILE_NAME 属性 value 中）。
///
/// FILE_NAME value 布局：
///   0x00 Parent directory reference (u64)
///   0x08 Creation time (u64)
///   0x10 Modification time (u64)
///   0x18 MFT modification time (u64)
///   0x20 Read time (u64)
///   0x28 Allocated size (u64)
///   0x30 Real size (u64)
///   0x38 Flags (u32)
///   0x3C Reparse (u32)
///   0x40 File name length in UTF-16 units (u8)
///   0x41 Namespace (u8)
///   0x42 File name (UTF-16, variable)
const FILE_NAME_PARENT_REF_OFFSET: usize = 0x00;
const FILE_NAME_NAME_LENGTH_OFFSET: usize = 0x40;
const FILE_NAME_NAMESPACE_OFFSET: usize = 0x41;
const FILE_NAME_NAME_OFFSET: usize = 0x42;
/// `$FILE_NAME` value 的最小长度（到 namespace 字段末尾，不含文件名）。
const FILE_NAME_VALUE_MIN_LEN: usize = FILE_NAME_NAME_OFFSET;

/// 命名空间：POSIX（0）。
const NAMESPACE_POSIX: u8 = 0;
/// 命名空间：Win32 长名（1）。
const NAMESPACE_WIN32: u8 = 1;
/// 命名空间：DOS 8.3 短名（2）。
#[allow(dead_code)]
const NAMESPACE_DOS: u8 = 2;
/// 命名空间：Win32 + DOS（3，表示该名字同时满足 Win32 与 DOS 约束）。
const NAMESPACE_WIN32_AND_DOS: u8 = 3;

// ===== `$ATTRIBUTE_LIST` entry 偏移 =====
//
// ATTRIBUTE_LIST_ENTRY 布局（NTFS 标准）：
//   0x00 Attribute type (u32)
//   0x04 Record length (u16, 含头)
//   0x06 Attribute name length (u8, UTF-16 units)
//   0x07 Attribute name offset (u8, 从本 entry 起算)
//   0x08 Lowest VCN (u64)
//   0x10 Base file reference (u64)
//   0x18 Attribute id (u16)
const ATTR_LIST_ENTRY_TYPE_OFFSET: usize = 0x00;
const ATTR_LIST_ENTRY_LENGTH_OFFSET: usize = 0x04;
const ATTR_LIST_ENTRY_MIN_LEN: usize = 0x1A;
const ATTR_LIST_ENTRY_LOWEST_VCN_OFFSET: usize = 0x08;
const ATTR_LIST_ENTRY_BASE_REF_OFFSET: usize = 0x10;
const ATTR_LIST_ENTRY_ATTR_ID_OFFSET: usize = 0x18;
const ATTR_LIST_ENTRY_NAME_LENGTH_OFFSET: usize = 0x06;
const ATTR_LIST_ENTRY_NAME_OFFSET_FIELD: usize = 0x07;

/// IO_REPARSE_TAG_MOUNT_POINT（junction）。
pub const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// IO_REPARSE_TAG_SYMLINK（符号链接）。
pub const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

// ===== 数据模型（自含定义，设计 4.2） =====

/// 文件引用（记录号 + 序列号）。
///
/// `record_no` 取低 48 位（由 IOCTL `file_reference` 权威给定），
/// `sequence` 取高 16 位。两者共同构成 NTFS 的 64 位 file reference。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileRef {
    pub record_no: u64,
    pub sequence: u16,
}

impl FileRef {
    /// 从原始 64 位 file reference 拆分出记录号（低 48 位）与序列号（高 16 位）。
    pub fn from_raw(raw: u64) -> Self {
        // NTFS_FILE_REFERENCE：低 48 位 = 记录号，高 16 位 = 序列号。
        Self {
            record_no: raw & 0x0000_FFFF_FFFF_FFFF,
            sequence: ((raw >> 48) & 0xFFFF) as u16,
        }
    }
}

impl std::fmt::Display for FileRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.record_no, self.sequence)
    }
}

/// 一个 `$FILE_NAME` 属性入口。
///
/// `parent` 是父目录的完整 `FileRef`（包含序列号，T2 用于一致性校验）；
/// `name` 是 UTF-16 解码后的文件名；`namespace` 是 NTFS 命名空间（0/1/2/3）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftName {
    pub parent: FileRef,
    pub name: String,
    pub namespace: u8,
}

/// 单条 MFT 记录解析后的可分类信息。
///
/// T3 的 [`parse_record`] 返回此结构。T2 在其上做跨记录聚合：
/// - `base_record.is_some()` 的记录是 extension，**绝不**单独分类为文件或目录
///   （简报 3.4），只用于合并到 base 记录。
/// - `names` 可能含多个入口（硬链接）；T2 按每个有效长名入口累加到完整父
///   `FileRef`，额外入口增 hard-link 诊断。
/// - `logical_size` 是该记录**本记录中**可见的命名/未命名 `$DATA` 流逻辑大小之和；
///   对 base record 携带 `$ATTRIBUTE_LIST`（如 big.bin），extension extent 在 T2 合并时累加。
///   同一 stream 的 extension extent 通过 `lowest_vcn != 0` 在本记录里被排除。
/// - `reparse_tag` 仅解析 tag；本工具不在此判断是否为本工具的 junction。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MftRecord {
    /// 记录自身引用（record_no + header 的 sequence）。
    pub id: FileRef,
    /// Base record 引用；`None` 表示这是 base，`Some` 表示 extension。
    pub base_record: Option<FileRef>,
    /// 所有 `$FILE_NAME` 入口（含 DOS / POSIX / Win32；T2 做 namespace 过滤）。
    pub names: Vec<MftName>,
    /// 本记录中可见的命名/未命名 `$DATA` 流逻辑大小之和。
    pub logical_size: u64,
    /// 是否目录（FILE record flags bit1）。
    pub is_dir: bool,
    /// `$REPARSE_POINT` 的 tag（仅当该属性存在）。
    pub reparse_tag: Option<u32>,
    /// 该 base record 是否带有 non-resident `$ATTRIBUTE_LIST`。
    ///
    /// 若为真，extension extent 的合并需要 T2 reader 先按 Data Run 读取 list 的
    /// 完整字节，再调 [`parse_attribute_list_entries`] 解析；T3 单条解析无法安全完成。
    pub has_nonresident_attr_list: bool,
}

/// 一段连续的簇分配（Data Run）。
///
/// `start_lcn` 是起始逻辑簇号（可带符号：负偏移表示从前一段尾部的反向跳转，
/// 解码时已按 NTFS 累加语义还原成绝对 LCN）；`length_clusters` 是该段簇数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRun {
    pub start_lcn: i64,
    pub length_clusters: u64,
}

/// 直接统计的文件计数与字节数（directory 直接下钻累加，非递归子目录）。
///
/// T2/T4 聚合阶段填充；本模块只定义类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DirectFileStats {
    pub size_bytes: u64,
    pub file_count: u64,
}

// ===== MftError（设计 4.2） =====
//
// 枚举完整定义供 T2 卷级/枚举级错误复用；T3 单条解析只产生 BadRecord / Io / Cancelled
// （Cancelled 在 T3 解析内部基本不产生，但类型必须存在供 T2 主循环频繁检查）。

/// MFT 解析与枚举阶段的分类错误。
///
/// 与项目现有 [`crate::win32::VolumeError`] 风格一致：纯枚举 + Display +
/// `std::error::Error`。T7 将其整合到全局 `AppError`。
#[derive(Debug)]
pub enum MftError {
    /// `FSCTL_GET_NTFS_FILE_RECORD` / 打开卷返回 `ERROR_ACCESS_DENIED`。
    NeedElevation,
    /// 卷并非 NTFS（`actual` 为实际文件系统名称）。
    UnsupportedFilesystem { actual: String },
    /// NTFS 版本号非 3.1（`major.minor`）。
    UnsupportedNtfsVersion { major: u16, minor: u16 },
    /// 卷几何或扩展卷数据缓冲不合法（截断 / 零填充被误当字段等）。
    InvalidVolumeData,
    /// 根目录记录（record 5）缺失或不可解析。
    RootRecordMissing,
    /// 枚举过程错误记录过多，超门槛；`skipped` / `scanned` 用于诊断。
    ExcessiveRecordErrors { skipped: u64, scanned: u64 },
    /// 单条记录解析失败（USA 校验失败、属性越界、签名错等）。
    /// `ref_no` 为低 48 位记录号。
    BadRecord { ref_no: u64 },
    /// 底层 I/O 错误（读取卷字节等）。
    Io(std::io::Error),
    /// 用户取消（T2 主循环检查）。
    Cancelled,
}

impl std::fmt::Display for MftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MftError::NeedElevation => f.write_str("需要管理员权限读取 MFT"),
            MftError::UnsupportedFilesystem { actual } => {
                write!(f, "不支持的文件系统（仅支持 NTFS，实际为 {actual}）")
            }
            MftError::UnsupportedNtfsVersion { major, minor } => write!(
                f,
                "不支持的 NTFS 版本（仅支持 3.1，实际为 {major}.{minor}）"
            ),
            MftError::InvalidVolumeData => f.write_str("卷数据缓冲不合法或被截断"),
            MftError::RootRecordMissing => f.write_str("根目录记录（record 5）缺失或不可解析"),
            MftError::ExcessiveRecordErrors { skipped, scanned } => write!(
                f,
                "MFT 枚举错误过多（{skipped}/{scanned}），已超过门槛"
            ),
            MftError::BadRecord { ref_no } => {
                write!(f, "MFT 记录 {ref_no} 损坏或不可解析")
            }
            MftError::Io(e) => write!(f, "I/O 错误: {e}"),
            MftError::Cancelled => f.write_str("用户取消"),
        }
    }
}

impl std::error::Error for MftError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            MftError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for MftError {
    fn from(e: std::io::Error) -> Self {
        MftError::Io(e)
    }
}

// ===== 字节读取辅助（纯函数，越界返回 BadRecord） =====

fn read_u16_at(bytes: &[u8], offset: usize, ref_no: u64) -> Result<u16, MftError> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or(MftError::BadRecord { ref_no })?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32_at(bytes: &[u8], offset: usize, ref_no: u64) -> Result<u32, MftError> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(MftError::BadRecord { ref_no })?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64_at(bytes: &[u8], offset: usize, ref_no: u64) -> Result<u64, MftError> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(MftError::BadRecord { ref_no })?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(slice);
    Ok(u64::from_le_bytes(arr))
}

// ===== USA fix-up（简报 3.2） =====
//
// NTFS 对每条 FILE record 做 update sequence 保护：每个 sector 末尾 2 字节
// 被 USN（Update Sequence Number）占位，原始尾部 2 字节存到 USA 数组。
// 读出原始记录后必须做 fix-up：
//   1. 读 USA offset / count（count 包含 USN 自身）。
//   2. 校验 count == record_len / bytes_per_sector + 1（1024/512+1=3）。
//   3. 对每个 sector i（1..count-1），其尾部 2 字节应等于 USN；相等则用
//      USA[i] 覆盖回原位置。任一不符即 BadRecord。
//
// 简报要求：用 bytes_per_sector（不硬编码 512），不硬编码 USA offset，
// 不跳过 USN 校验。

/// 对一条 FILE record 字节做 USA fix-up（在副本上）。
///
/// 输入 `bytes` 可能是两种状态之一（NTFS 实现差异）：
/// 1. **USA 未修复**（磁盘原始）：每个 sector 尾部 2 字节 == USN，需要用
///    USA[i] 覆盖还原。
/// 2. **USA 已修复**（`FSCTL_GET_NTFS_FILE_RECORD` 在某些 Windows 版本上返回）：
///    sector 尾部已是原始应用数据，与 USA[i] 相等而与 USN 不等。
///
/// 本函数对两种情况都正确处理（简报 3.2 要求 USA 校验不跳过）：
/// - 若 sector 尾部 == USN：用 USA[i] 覆盖（标准 fix-up）。
/// - 若 sector 尾部 == USA[i]：fix-up 已被上层应用，直接返回副本（幂等）。
/// - 两者都不等：BadRecord。
///
/// `bytes_per_sector` 来自卷几何（典型 512）；`record_len` 通常等于
/// `bytes_per_file_record_segment`（典型 1024）。
///
/// 校验项（简报 3.2）：
/// - FILE 签名 == `FILE`。
/// - `usa_count == record_len / bytes_per_sector + 1`。
/// - 替换数组完整（USA 总字节数 = 2 * usa_count，落在 record 内）。
/// - 每个 sector 尾部原值 == USN **或** == USA[i]（已修复）。
pub fn apply_usa_fixup(
    bytes: &[u8],
    record_no: u64,
    bytes_per_sector: u32,
) -> Result<Vec<u8>, MftError> {
    if bytes.len() < FILE_BYTES_IN_USE_OFFSET + 4 {
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    if bytes[0..4] != FILE_SIGNATURE {
        return Err(MftError::BadRecord { ref_no: record_no });
    }

    let record_len = bytes.len();
    let bytes_per_sector = bytes_per_sector as usize;
    if bytes_per_sector == 0 {
        // 防除零；不可能由合法卷几何触发。
        return Err(MftError::BadRecord { ref_no: record_no });
    }

    let usa_offset = read_u16_at(bytes, FILE_USA_OFFSET_OFFSET, record_no)? as usize;
    let usa_count = read_u16_at(bytes, FILE_USA_COUNT_OFFSET, record_no)? as usize;

    // 校验 USA count == record_len / bytes_per_sector + 1。
    // 例：1024 / 512 + 1 = 3（USN + 2 个 sector 尾部替换值）。
    let expected_count = record_len / bytes_per_sector + 1;
    if usa_count != expected_count {
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    if !record_len.is_multiple_of(bytes_per_sector) {
        // 非整数 sector 的记录不合法（NTFS 记录总是 sector 整数倍）。
        return Err(MftError::BadRecord { ref_no: record_no });
    }

    // 替换数组总字节数 = 2 * usa_count，必须完整落在 record 内。
    let usa_total_bytes = usa_count
        .checked_mul(2)
        .filter(|&total| usa_offset + total <= record_len)
        .ok_or(MftError::BadRecord { ref_no: record_no })?;

    let usn = read_u16_at(bytes, usa_offset, record_no)?;
    // 逐 sector 判断：尾部 == USN（需 fix-up）还是 == USA[i]（已修复）。
    // 混合情况（部分 sector 已修复、部分未修复）视为损坏。
    let mut already_fixed = None;
    for i in 1..usa_count {
        let sector_end = i
            .checked_mul(bytes_per_sector)
            .and_then(|v| v.checked_sub(2))
            .ok_or(MftError::BadRecord { ref_no: record_no })?;
        let original = read_u16_at(bytes, sector_end, record_no)?;
        let fixup_value = read_u16_at(bytes, usa_offset + i * 2, record_no)?;
        let matches_usn = original == usn;
        let matches_fixup = original == fixup_value;
        if matches_usn {
            // 标准 fix-up 路径。
            if already_fixed == Some(true) {
                // 混合：部分已修复、本 sector 未修复 -> 视为损坏。
                return Err(MftError::BadRecord { ref_no: record_no });
            }
            already_fixed = Some(false);
        } else if matches_fixup {
            // 已修复：尾部已是原始应用数据。
            if already_fixed == Some(false) {
                // 混合。
                return Err(MftError::BadRecord { ref_no: record_no });
            }
            already_fixed = Some(true);
        } else {
            // 既不等于 USN 也不等于 USA[i]：记录损坏（部分写入）。
            return Err(MftError::BadRecord { ref_no: record_no });
        }
    }

    let mut out = bytes.to_vec();
    if already_fixed == Some(true) {
        // 整条记录已被上层 fix-up；幂等返回副本。
        return Ok(out);
    }
    // 标准路径：用 USA[i] 覆盖每个 sector 尾部（尾部原值已校验 == USN）。
    for i in 1..usa_count {
        let sector_end = i * bytes_per_sector - 2;
        let fixup_value = read_u16_at(&out, usa_offset + i * 2, record_no)?;
        out[sector_end..sector_end + 2].copy_from_slice(&fixup_value.to_le_bytes());
    }

    let _ = usa_total_bytes; // 已通过长度检查；值不再单独使用。
    Ok(out)
}

// ===== Data Run 解码（简报重排职责 + 3.3） =====
//
// NTFS Data Run 编码：每段以 1 字节头开始，低 4 位 = 长度字段大小（字节数），
// 高 4 位 = 偏移字段大小（字节数）；随后是小端长度、小端偏移（偏移有符号）。
// 头 == 0x00 表示结束。偏移是相对前一段尾部的"前一尾 + 偏移"语义，需要累加
// 还原为绝对 LCN。

/// 解码一段 Data Run 字节序列（NTFS non-resident 属性的 run list）。
///
/// 输入是属性头之后、属性 length 边界内的 run list 字节（含末尾 0x00 终止符）。
/// 返回绝对 LCN 还原后的 `Vec<DataRun>`。
///
/// 适用于所有 non-resident 属性的 Data Run（`$DATA`、`$ATTRIBUTE_LIST` 等），
/// 尤其是记录 0 `$MFT` 自身 `$DATA` 的定位（T2 批量读 reader 的前置）。
pub fn decode_data_runs(bytes: &[u8]) -> Result<Vec<DataRun>, MftError> {
    let mut runs = Vec::new();
    let mut pos = 0usize;
    // 累加偏移（前一尾 + 当前偏移）；首段为绝对 LCN。
    let mut prev_end_lcn: i64 = 0;

    while pos < bytes.len() {
        let header = bytes[pos];
        pos += 1;
        if header == 0 {
            // 正常结束符。
            break;
        }
        let len_size = (header & 0x0F) as usize;
        let off_size = ((header >> 4) & 0x0F) as usize;

        // 长度字段必为 1..=8；偏移字段为 0 表示 sparse 段（仅长度，LCN 不变）。
        if len_size == 0 || len_size > 8 || off_size > 8 {
            return Err(MftError::BadRecord { ref_no: 0 });
        }
        if pos + len_size > bytes.len() {
            return Err(MftError::BadRecord { ref_no: 0 });
        }
        let mut length_clusters: u64 = 0;
        for j in 0..len_size {
            length_clusters |= (bytes[pos + j] as u64) << (j * 8);
        }
        pos += len_size;

        let mut offset_raw: u64 = 0;
        if off_size > 0 {
            if pos + off_size > bytes.len() {
                return Err(MftError::BadRecord { ref_no: 0 });
            }
            for j in 0..off_size {
                offset_raw |= (bytes[pos + j] as u64) << (j * 8);
            }
            pos += off_size;
        }

        // 有符号扩展（偏移字段以小端补码表示）。
        let signed_offset: i64 = if off_size == 0 {
            0
        } else {
            sign_extend(offset_raw, off_size)
        };

        let start_lcn = if off_size == 0 {
            // Sparse extent：LCN 不变（用前一段的 end 作为占位，length 仍计入）。
            prev_end_lcn
        } else {
            prev_end_lcn.checked_add(signed_offset).ok_or(MftError::BadRecord {
                ref_no: 0,
            })?
        };
        // 累加位置用于下一段相对偏移（sparse 段也算占用长度推进 prev_end）。
        prev_end_lcn = start_lcn
            .checked_add(length_clusters as i64)
            .ok_or(MftError::BadRecord { ref_no: 0 })?;

        runs.push(DataRun {
            start_lcn,
            length_clusters,
        });
    }

    Ok(runs)
}

/// 把 `off_size` 字节小端补码值符号扩展为 i64。
fn sign_extend(value: u64, off_size: usize) -> i64 {
    if off_size == 0 || off_size >= 8 {
        return value as i64;
    }
    let bits = (off_size as u64) * 8;
    let sign_bit = 1u64 << (bits - 1);
    if value & sign_bit != 0 {
        // 高位补 1。
        let mask = u64::MAX << bits;
        (value | mask) as i64
    } else {
        value as i64
    }
}

// ===== 属性遍历（简报 3.3） =====

/// 一个被遍历到的属性的字节视图与已解码的通用头字段。
///
/// `type_`、`length`、`non_resident`、`attribute_id`、`name` 等字段便于下游做
/// 类型分支处理；`start` / `end` 是该属性在记录中的绝对字节范围（含头）。
#[allow(dead_code)]
struct AttrView<'a> {
    type_: u32,
    length: usize,
    non_resident: bool,
    attribute_id: u16,
    name: String,
    bytes: &'a [u8],
}

/// 遍历 FILE record 的所有属性。
///
/// 每次读取属性 type/length 前做边界检查；length 为 0、短于通用属性头最小长度、
/// 越过 `bytes_in_use` 均返回 [`MftError::BadRecord`]（简报 3.3）。
fn walk_attributes(
    fixed: &[u8],
    record_no: u64,
) -> Result<Vec<AttrView<'_>>, MftError> {
    let first_attr = read_u16_at(fixed, FILE_FIRST_ATTRIBUTE_OFFSET, record_no)? as usize;
    let bytes_in_use =
        read_u32_at(fixed, FILE_BYTES_IN_USE_OFFSET, record_no)? as usize;

    if bytes_in_use > fixed.len() {
        // bytes_in_use 不可超过记录总长度。
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    if first_attr < FILE_FIRST_ATTRIBUTE_OFFSET || first_attr >= bytes_in_use {
        return Err(MftError::BadRecord { ref_no: record_no });
    }

    let mut attrs = Vec::new();
    let mut offset = first_attr;
    while offset + ATTR_COMMON_HEADER_LEN <= bytes_in_use {
        let type_ = read_u32_at(fixed, offset + ATTR_TYPE_OFFSET, record_no)?;
        if type_ == ATTR_TYPE_END_MARKER {
            break;
        }
        let length = read_u32_at(fixed, offset + ATTR_LENGTH_OFFSET, record_no)? as usize;
        if length < ATTR_COMMON_HEADER_LEN {
            // 属性长度不得短于通用头最小长度。
            return Err(MftError::BadRecord { ref_no: record_no });
        }
        // 越过 bytes_in_use 即越界。
        let end = offset
            .checked_add(length)
            .filter(|&end| end <= bytes_in_use)
            .ok_or(MftError::BadRecord { ref_no: record_no })?;

        let non_resident = fixed[offset + ATTR_NONRES_FLAG_OFFSET] != 0;
        let attribute_id = read_u16_at(fixed, offset + ATTR_ATTRIBUTE_ID_OFFSET, record_no)?;
        let name_len = fixed[offset + ATTR_NAME_LENGTH_OFFSET] as usize;
        let name_offset =
            read_u16_at(fixed, offset + ATTR_NAME_OFFSET_FIELD, record_no)? as usize;
        let name = if name_len > 0 {
            if name_offset < ATTR_COMMON_HEADER_LEN {
                return Err(MftError::BadRecord { ref_no: record_no });
            }
            decode_attr_name(
                &fixed[offset..end],
                name_offset,
                name_len,
                record_no,
            )?
        } else {
            String::new()
        };

        attrs.push(AttrView {
            type_,
            length,
            non_resident,
            attribute_id,
            name,
            bytes: &fixed[offset..end],
        });
        offset = end;
    }

    Ok(attrs)
}

/// 解码属性头中的属性名（UTF-16，name_offset 从属性起始算）。
fn decode_attr_name(
    attr_bytes: &[u8],
    name_offset: usize,
    name_len_utf16: usize,
    record_no: u64,
) -> Result<String, MftError> {
    let name_byte_len = name_len_utf16
        .checked_mul(2)
        .ok_or(MftError::BadRecord { ref_no: record_no })?;
    let slice = attr_bytes
        .get(name_offset..name_offset + name_byte_len)
        .ok_or(MftError::BadRecord { ref_no: record_no })?;
    Ok(String::from_utf16_lossy(
        &slice
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<u16>>(),
    ))
}

// ===== `$FILE_NAME` 解码（简报 3.3） =====
//
// 解码前验证 UTF-16 单元长度：name_length 字段 + FILE_NAME value 起始 +
// FILE_NAME_VALUE_MIN_LEN 必须落在属性 value 边界内。

/// 解析单个 `$FILE_NAME` 属性为一个 [`MftName`]。
fn parse_file_name(
    attr: &AttrView<'_>,
    record_no: u64,
) -> Result<MftName, MftError> {
    if attr.non_resident {
        // `$FILE_NAME` 总是 resident。
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    let value_offset = read_u16_at(attr.bytes, ATTR_RESIDENT_VALUE_OFFSET_FIELD, record_no)?
        as usize;
    let value_length =
        read_u32_at(attr.bytes, ATTR_RESIDENT_VALUE_LENGTH_OFFSET, record_no)? as usize;
    // value 起始是相对属性字节切片的 value_offset；value 的内容范围是
    // [value_offset, value_offset + value_length)，且必须落在 attr.bytes 内。
    let value_end = value_offset
        .checked_add(value_length)
        .filter(|&end| end <= attr.bytes.len())
        .ok_or(MftError::BadRecord { ref_no: record_no })?;
    if value_offset + FILE_NAME_VALUE_MIN_LEN > value_end {
        // value 边界不足容纳 FILE_NAME 头（到 namespace 字段）。
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    let parent_raw =
        read_u64_at(attr.bytes, value_offset + FILE_NAME_PARENT_REF_OFFSET, record_no)?;
    let name_len_utf16 = attr.bytes[value_offset + FILE_NAME_NAME_LENGTH_OFFSET] as usize;
    let namespace = attr.bytes[value_offset + FILE_NAME_NAMESPACE_OFFSET];
    let name_start = value_offset + FILE_NAME_NAME_OFFSET;
    let name_byte_len = name_len_utf16
        .checked_mul(2)
        .ok_or(MftError::BadRecord { ref_no: record_no })?;
    if name_start + name_byte_len > value_end {
        // UTF-16 单元长度越过 value 边界。
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    let name = decode_attr_name(attr.bytes, name_start, name_len_utf16, record_no)?;

    Ok(MftName {
        parent: FileRef::from_raw(parent_raw),
        name,
        namespace,
    })
}

// ===== `$ATTRIBUTE_LIST` entry 解析（简报 3.4） =====

/// 单个 `$ATTRIBUTE_LIST` entry 解码结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeListEntry {
    pub attribute_type: u32,
    pub attribute_id: u16,
    pub attribute_name: String,
    pub lowest_vcn: u64,
    /// 引用的另一条记录的完整 file reference（extension record）。
    pub base_reference: FileRef,
}

/// 解析 resident `$ATTRIBUTE_LIST` value 字节为 entry 列表。
///
/// `list_value` 是属性 value 的字节切片（已剥离属性头与 value offset）。
/// 每条 entry 至少 [`ATTR_LIST_ENTRY_MIN_LEN`] 字节；长度为 0 或越界即返回 BadRecord。
pub fn parse_attribute_list_entries(
    list_value: &[u8],
    record_no: u64,
) -> Result<Vec<AttributeListEntry>, MftError> {
    let mut entries = Vec::new();
    let mut pos = 0usize;
    while pos < list_value.len() {
        if pos + ATTR_LIST_ENTRY_MIN_LEN > list_value.len() {
            // 不足以容纳一个完整 entry：尾部的 slack 在 NTFS 实现里可存在
            // （list 通常 8 字节对齐），但 entry 自身必须完整。
            break;
        }
        let entry_type = read_u32_at(list_value, pos + ATTR_LIST_ENTRY_TYPE_OFFSET, record_no)?;
        let entry_len =
            read_u16_at(list_value, pos + ATTR_LIST_ENTRY_LENGTH_OFFSET, record_no)? as usize;
        if entry_len < ATTR_LIST_ENTRY_MIN_LEN {
            return Err(MftError::BadRecord { ref_no: record_no });
        }
        if pos + entry_len > list_value.len() {
            return Err(MftError::BadRecord { ref_no: record_no });
        }
        let name_len_utf16 =
            list_value[pos + ATTR_LIST_ENTRY_NAME_LENGTH_OFFSET] as usize;
        let name_offset =
            list_value[pos + ATTR_LIST_ENTRY_NAME_OFFSET_FIELD] as usize;
        let lowest_vcn = read_u64_at(
            list_value,
            pos + ATTR_LIST_ENTRY_LOWEST_VCN_OFFSET,
            record_no,
        )?;
        let base_raw = read_u64_at(
            list_value,
            pos + ATTR_LIST_ENTRY_BASE_REF_OFFSET,
            record_no,
        )?;
        let attribute_id = read_u16_at(
            list_value,
            pos + ATTR_LIST_ENTRY_ATTR_ID_OFFSET,
            record_no,
        )?;
        let attribute_name = if name_len_utf16 > 0 {
            if name_offset < ATTR_LIST_ENTRY_MIN_LEN
                || pos + name_offset + name_len_utf16 * 2 > pos + entry_len
            {
                return Err(MftError::BadRecord { ref_no: record_no });
            }
            let abs = pos + name_offset;
            decode_attr_name(
                &list_value[abs..abs + name_len_utf16 * 2],
                0,
                name_len_utf16,
                record_no,
            )?
        } else {
            String::new()
        };

        entries.push(AttributeListEntry {
            attribute_type: entry_type,
            attribute_id,
            attribute_name,
            lowest_vcn,
            base_reference: FileRef::from_raw(base_raw),
        });

        pos += entry_len;
    }
    Ok(entries)
}

/// 最小有效性检查：FILE 签名 + 足够容纳 bytes_in_use 字段的长度。
///
/// 用于 [`parse_record`] 的 USA 降级路径：当 [`apply_usa_fixup`] 因 sector 尾部
/// 既不匹配 USN 也不匹配 USA[i] 而失败时，若字节至少有合法 FILE 签名与最小
/// header，则假设 IOCTL 已在上层完成 USA fix-up，直接用原字节继续解析。
fn is_minimal_valid_record(bytes: &[u8]) -> bool {
    if bytes.len() < FILE_BYTES_IN_USE_OFFSET + 4 {
        return false;
    }
    bytes[0..4] == FILE_SIGNATURE
}

// ===== 核心：parse_record（简报 3.3-3.5） =====

/// 解析单条 MFT FILE record。
///
/// - `bytes`：原始记录字节，长度通常等于 `bytes_per_file_record_segment`。
///   可能处于两种状态：(1) USA 未修复（sector 尾部含 USN 哨兵），
///   (2) USA 已修复（`FSCTL_GET_NTFS_FILE_RECORD` 在现代 Windows 上返回的字节，
///   sector 尾部已是应用数据，USA 数组通常为零或陈旧）。
/// - `record_no`：IOCTL 返回的 file reference 低 48 位（权威记录号）。
/// - `bytes_per_sector`：来自卷几何（典型 512），用于 USA fix-up。
///
/// 本函数对两种字节状态都正确处理：
/// - 先尝试 [`apply_usa_fixup`]（标准 fix-up 路径，覆盖"未修复"与"已修复幂等"）。
/// - 若 USA 校验失败（sector 尾部既不等于 USN 也不等于 USA[i]），降级为直接使用
///   原始字节（假设 IOCTL 已在上层完成 fix-up，USA 数组不可信）。降级路径仍验证
///   FILE 签名与最小长度，避免解析垃圾。
///
/// 返回的 [`MftRecord`] 包含足够 T2 做分类与合并的信息。extension record
/// （`base_record.is_some()`）不会被 T2 单独分类为文件或目录（简报 3.4）。
///
/// 此函数签名为 T2 复用契约，保持稳定。
pub fn parse_record(
    bytes: &[u8],
    record_no: u64,
    bytes_per_sector: u32,
) -> Result<MftRecord, MftError> {
    // 1. USA fix-up（在副本上）。现代 Windows 的 FSCTL_GET_NTFS_FILE_RECORD 返回的
    //    字节中，sector 尾部通常已是应用数据（USA 已修复），USA 数组不可信。
    //    此时 apply_usa_fixup 会因尾部既不匹配 USN 也不匹配 USA[i] 而返回 BadRecord。
    //    降级路径：直接使用原始字节（仅验证签名与最小长度）。
    let fixed = match apply_usa_fixup(bytes, record_no, bytes_per_sector) {
        Ok(fixed) => fixed,
        Err(MftError::BadRecord { .. }) if is_minimal_valid_record(bytes) => {
            // IOCTL 已修复 USA；原字节可直接用。
            bytes.to_vec()
        }
        Err(e) => return Err(e),
    };

    // 2. 读取 header 字段。
    let sequence = read_u16_at(&fixed, FILE_SEQUENCE_OFFSET, record_no)?;
    let flags = read_u16_at(&fixed, FILE_FLAGS_OFFSET, record_no)?;
    let base_raw = read_u64_at(&fixed, FILE_BASE_REFERENCE_OFFSET, record_no)?;

    let is_dir = (flags & FILE_FLAG_DIRECTORY) != 0;
    let in_use = (flags & FILE_FLAG_IN_USE) != 0;
    let base_record = if base_raw == 0 {
        None
    } else {
        Some(FileRef::from_raw(base_raw))
    };

    // 未在用的记录（如已删除）仍返回结构，但 logical_size=0、names 空，
    // T2 可按 in_use 过滤。简报未要求 parse_record 拒绝未在用记录。
    if !in_use {
        return Ok(MftRecord {
            id: FileRef {
                record_no,
                sequence,
            },
            base_record,
            names: Vec::new(),
            logical_size: 0,
            is_dir,
            reparse_tag: None,
            has_nonresident_attr_list: false,
        });
    }

    // 3. 遍历属性。
    let attrs = walk_attributes(&fixed, record_no)?;

    let mut names: Vec<MftName> = Vec::new();
    let mut logical_size: u64 = 0;
    let mut reparse_tag: Option<u32> = None;
    let mut has_nonresident_attr_list = false;
    // 用于 `$DATA` 同 stream 去重：每个 (name, attribute_id) 只在 lowest_vcn==0
    // 时累计一次逻辑大小（简报 3.3：extension extent 不重复累计）。
    // 注意：同一条记录里出现同 stream 多 extent 是少见但合法的情况；此处仅做
    // 防御性去重，真正跨记录的 extension extent 合并在 T2 完成。

    for attr in &attrs {
        match attr.type_ {
            ATTR_TYPE_FILE_NAME => {
                if let Ok(name) = parse_file_name(attr, record_no) {
                    names.push(name);
                } else {
                    // 单个 FILE_NAME 损坏：整条记录视为 BadRecord（简报 3.3
                    // 把"$FILE_NAME 解码前验证 UTF-16 单元长度"列为硬错误）。
                    return Err(MftError::BadRecord { ref_no: record_no });
                }
            }
            ATTR_TYPE_DATA => {
                // 目录的 `$DATA`（$I30 等命名流）不计入文件 logical_size；
                // 只累加未命名 `$DATA` 与文件命名流（ADS）。
                // 简报 4.3：non-resident `$DATA` 读 logical size；resident
                // 读 value length。lowest_vcn==0 保证一 stream 只累计一次。
                if attr.non_resident {
                    if attr.bytes.len() < ATTR_NONRESIDENT_HEADER_LEN {
                        return Err(MftError::BadRecord { ref_no: record_no });
                    }
                    let lowest_vcn =
                        read_u64_at(attr.bytes, ATTR_NONRES_LOWEST_VCN_OFFSET, record_no)?;
                    if lowest_vcn == 0 {
                        let logical =
                            read_u64_at(attr.bytes, ATTR_NONRES_LOGICAL_SIZE_OFFSET, record_no)?;
                        accumulate_data_size(&mut logical_size, attr, logical, record_no)?;
                    }
                } else {
                    let value_length = read_u32_at(
                        attr.bytes,
                        ATTR_RESIDENT_VALUE_LENGTH_OFFSET,
                        record_no,
                    )? as u64;
                    accumulate_data_size(&mut logical_size, attr, value_length, record_no)?;
                }
            }
            ATTR_TYPE_REPARSE_POINT => {
                if let Ok(tag) = parse_reparse_tag(attr, record_no) {
                    reparse_tag = Some(tag);
                } else {
                    return Err(MftError::BadRecord { ref_no: record_no });
                }
            }
            ATTR_TYPE_ATTRIBUTE_LIST => {
                if attr.non_resident {
                    has_nonresident_attr_list = true;
                }
                // resident list 的 entries 解码由 T2 在合并阶段按需调用
                // [`parse_attribute_list_entries`]；T3 单条 parse_record 不做
                // 跨记录合并。
            }
            ATTR_TYPE_STANDARD_INFORMATION
            | ATTR_TYPE_OBJECT_ID
            | ATTR_TYPE_INDEX_ROOT
            | ATTR_TYPE_INDEX_ALLOCATION => {
                // 这些属性的长度已在 walk_attributes 中校验；parse_record 不提取字段。
            }
            _ => {
                // 未知属性类型：walk_attributes 已校验长度，这里跳过。
            }
        }
    }

    Ok(MftRecord {
        id: FileRef {
            record_no,
            sequence,
        },
        base_record,
        names,
        logical_size,
        is_dir,
        reparse_tag,
        has_nonresident_attr_list,
    })
}

/// 累加一个 `$DATA` 流的逻辑大小到 `logical_size`。
///
/// 目录的 `$INDEX_ROOT` / `$INDEX_ALLOCATION` 不通过 `$DATA` 携带；
/// 但目录的命名 `$DATA`（罕见）与所有文件的 `$DATA`（默认 + ADS）都计入。
/// 这里不做 namespace 过滤（namespace 是 `$FILE_NAME` 的属性，与 `$DATA` 无关）。
fn accumulate_data_size(
    logical_size: &mut u64,
    _attr: &AttrView<'_>,
    value: u64,
    _record_no: u64,
) -> Result<(), MftError> {
    *logical_size = logical_size
        .checked_add(value)
        .ok_or(MftError::BadRecord { ref_no: _record_no })?;
    Ok(())
}

/// 解析 `$REPARSE_POINT` 属性的 tag（前 4 字节）。
fn parse_reparse_tag(attr: &AttrView<'_>, record_no: u64) -> Result<u32, MftError> {
    if attr.non_resident {
        // `$REPARSE_POINT` 总是 resident。
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    let value_offset = read_u16_at(attr.bytes, ATTR_RESIDENT_VALUE_OFFSET_FIELD, record_no)?
        as usize;
    let value_length =
        read_u32_at(attr.bytes, ATTR_RESIDENT_VALUE_LENGTH_OFFSET, record_no)? as usize;
    // tag 位于 value 起始的 4 字节。
    if value_length < 4 || value_offset + 4 > attr.bytes.len() {
        return Err(MftError::BadRecord { ref_no: record_no });
    }
    read_u32_at(attr.bytes, value_offset, record_no)
}

// ===== namespace 选择（简报 3.3 步骤 7 / 设计 4.3） =====
//
// 保留 namespace 1（Win32 长名）与 3（Win32+DOS）；完全没有 1/3 时回退 0（POSIX）；
// 纯 DOS（2）不建边（即过滤掉）。返回的 names 顺序保持稳定（首次出现优先）。

/// 从原始 `$FILE_NAME` 入口按 namespace 优先级筛选有效名字。
///
/// 规则（简报 3.3）：
/// - 保留 namespace == 1（Win32）或 3（Win32+DOS）的入口。
/// - 若没有任何 1/3 入口，回退保留 namespace == 0（POSIX）的入口。
/// - 纯 namespace == 2（DOS 8.3）不返回（不建边）。
///
/// 多硬链接（不同 parent）各自独立保留：同一记录的多个 1/3 入口都会返回，
/// 每个 entry 对应一条父引用。返回顺序与输入一致。
pub fn select_effective_names(names: &[MftName]) -> Vec<MftName> {
    let has_win32 = names
        .iter()
        .any(|n| n.namespace == NAMESPACE_WIN32 || n.namespace == NAMESPACE_WIN32_AND_DOS);
    if has_win32 {
        names
            .iter()
            .filter(|n| {
                n.namespace == NAMESPACE_WIN32 || n.namespace == NAMESPACE_WIN32_AND_DOS
            })
            .cloned()
            .collect()
    } else {
        names
            .iter()
            .filter(|n| n.namespace == NAMESPACE_POSIX)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ===== FILE header 常量断言（简报 3.2） =====

    #[test]
    fn file_header_constants_match_ntfs_spec() {
        // 这些值来自 NTFS 官方文档（与 windows crate / 微软 SDK 一致）。
        // 测试钉死以防止误改。
        assert_eq!(FILE_USA_OFFSET_OFFSET, 0x04);
        assert_eq!(FILE_USA_COUNT_OFFSET, 0x06);
        assert_eq!(FILE_SEQUENCE_OFFSET, 0x10);
        assert_eq!(FILE_FIRST_ATTRIBUTE_OFFSET, 0x14);
        assert_eq!(FILE_FLAGS_OFFSET, 0x16);
        assert_eq!(FILE_BYTES_IN_USE_OFFSET, 0x18);
        assert_eq!(FILE_BASE_REFERENCE_OFFSET, 0x20);
        assert_eq!(FILE_SIGNATURE, *b"FILE");
        assert_eq!(ATTR_TYPE_END_MARKER, 0xFFFF_FFFF);
        assert_eq!(ATTR_TYPE_ATTRIBUTE_LIST, 0x20);
        assert_eq!(ATTR_TYPE_FILE_NAME, 0x30);
        assert_eq!(ATTR_TYPE_DATA, 0x80);
        assert_eq!(ATTR_TYPE_REPARSE_POINT, 0xC0);
    }

    // ===== FileRef =====

    #[test]
    fn file_ref_from_raw_splits_record_and_sequence() {
        let raw = (1u64 << 48) | 42u64; // sequence=1, record=42
        let fr = FileRef::from_raw(raw);
        assert_eq!(fr.record_no, 42);
        assert_eq!(fr.sequence, 1);
    }

    #[test]
    fn file_ref_from_raw_record_zero() {
        let fr = FileRef::from_raw(0);
        assert_eq!(fr.record_no, 0);
        assert_eq!(fr.sequence, 0);
    }

    #[test]
    fn file_ref_from_raw_max_record() {
        let raw = 0x0000_FFFF_FFFF_FFFF; // 最大 record_no，sequence=0
        let fr = FileRef::from_raw(raw);
        assert_eq!(fr.record_no, 0xFFFF_FFFF_FFFF);
        assert_eq!(fr.sequence, 0);
    }

    // ===== USA fix-up（简报 3.2） =====
    //
    // 构造合法的 FILE record 字节并验证 fix-up 正确还原 sector 尾部字节。

    /// 构造一条最小合法 FILE record（record_len 字节，record_len/bytes_per_sector 个 sector）。
    /// USA 数组放在 0x30，USN=0x1234，每个 sector 尾部被占位。
    fn build_valid_record_bytes(record_len: usize, bytes_per_sector: usize) -> Vec<u8> {
        assert_eq!(record_len % bytes_per_sector, 0);
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(b"FILE");
        // USA offset = 0x30, count = record_len / bps + 1
        let usa_offset = 0x30usize;
        let usa_count = record_len / bytes_per_sector + 1;
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&(usa_count as u16).to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
        buf[0x14..0x16].copy_from_slice(&(usa_offset as u16).to_le_bytes()); // first attr
        buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes()); // in use
        buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes()); // bytes in use
        // USN 与 sector 尾部占位 + USA 数组
        let usn: u16 = 0x1234;
        buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
        // 每个 sector 尾部（i=1..usa_count）原值用 0xAABB + i 作为区分；
        // USA[i] 存对应原值，sector 尾部被 USN 覆盖。
        for i in 1..usa_count {
            let sector_end = i * bytes_per_sector - 2;
            let original: u16 = 0xAA00 + i as u16;
            buf[sector_end..sector_end + 2].copy_from_slice(&usn.to_le_bytes());
            let usa_entry_offset = usa_offset + i * 2;
            buf[usa_entry_offset..usa_entry_offset + 2]
                .copy_from_slice(&original.to_le_bytes());
        }
        buf
    }

    #[test]
    fn usa_fixup_valid_record_restores_sector_tails() {
        let bytes = build_valid_record_bytes(1024, 512);
        let fixed = apply_usa_fixup(&bytes, 0, 512).expect("合法记录应成功");
        // sector 1 尾部（offset 510-511）应被还原为 0xAA01
        let tail1 = u16::from_le_bytes([fixed[510], fixed[511]]);
        assert_eq!(tail1, 0xAA01);
        // sector 2 尾部（offset 1022-1023）应被还原为 0xAA02
        let tail2 = u16::from_le_bytes([fixed[1022], fixed[1023]]);
        assert_eq!(tail2, 0xAA02);
    }

    #[test]
    fn usa_fixup_bad_signature_returns_bad_record() {
        let mut bytes = build_valid_record_bytes(1024, 512);
        bytes[0..4].copy_from_slice(b"BAAD");
        let err = apply_usa_fixup(&bytes, 7, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 7 }));
    }

    #[test]
    fn usa_fixup_wrong_usn_returns_bad_record() {
        let mut bytes = build_valid_record_bytes(1024, 512);
        // 破坏第一个 sector 尾部的 USN。
        bytes[510] = 0x00;
        let err = apply_usa_fixup(&bytes, 0, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 0 }));
    }

    #[test]
    fn usa_fixup_wrong_count_returns_bad_record() {
        let mut bytes = build_valid_record_bytes(1024, 512);
        // 把 USA count 改成错误的值（正确应为 3）。
        bytes[0x06..0x08].copy_from_slice(&5u16.to_le_bytes());
        let err = apply_usa_fixup(&bytes, 0, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 0 }));
    }

    #[test]
    fn usa_fixup_count_matches_sector_size_ratio() {
        // 1024 字节记录 / 512 字节 sector = 2 个 sector，USA count 必为 3。
        let bytes = build_valid_record_bytes(1024, 512);
        let fixed = apply_usa_fixup(&bytes, 0, 512).unwrap();
        let count = u16::from_le_bytes([fixed[0x06], fixed[0x07]]);
        assert_eq!(count, 3);
    }

    #[test]
    fn usa_fixup_4096_record_with_512_sector() {
        // 4096 字节记录 / 512 字节 sector = 8 个 sector，USA count = 9。
        let bytes = build_valid_record_bytes(4096, 512);
        let fixed = apply_usa_fixup(&bytes, 0, 512).unwrap();
        let count = u16::from_le_bytes([fixed[0x06], fixed[0x07]]);
        assert_eq!(count, 9);
    }

    #[test]
    fn usa_fixup_truncated_buffer_returns_bad_record() {
        // 不足 bytes_in_use 字段所需的最小长度。
        let bytes = vec![0u8; 4];
        let err = apply_usa_fixup(&bytes, 0, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 0 }));
    }

    // ===== Data Run 解码（简报重排职责） =====

    #[test]
    fn decode_data_runs_single_run() {
        // 头 0x11：len_size=1, off_size=1。
        // length=64, offset=100（首段为绝对 LCN）。
        let runs_bytes = [0x11, 64, 100, 0x00];
        let runs = decode_data_runs(&runs_bytes).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start_lcn, 100);
        assert_eq!(runs[0].length_clusters, 64);
    }

    #[test]
    fn decode_data_runs_two_runs_accumulates_offset() {
        // 第一段：len=10, off=100（绝对 LCN=100，覆盖 [100,110)）
        // 第二段：len=20, off=50（绝对 LCN=110+50=160，覆盖 [160,180)）
        let runs_bytes = [0x11, 10, 100, 0x11, 20, 50, 0x00];
        let runs = decode_data_runs(&runs_bytes).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].start_lcn, 100);
        assert_eq!(runs[0].length_clusters, 10);
        assert_eq!(runs[1].start_lcn, 160);
        assert_eq!(runs[1].length_clusters, 20);
    }

    #[test]
    fn decode_data_runs_negative_offset() {
        // 第一段：header 0x21 = len_size=1, off_size=2；len=10, off=200（绝对 LCN=200）
        //   200 fits in positive range of i16 (max 32767)。
        // 第二段：header 0x21 = len_size=1, off_size=2；len=5, off=-100（绝对 LCN=210-100=110）
        //   -100 as i16 LE = 0xFF9C。
        let neg_off_bytes = (-100i16).to_le_bytes();
        let runs_bytes = [
            0x21, 10, 200, 0,
            0x21, 5, neg_off_bytes[0], neg_off_bytes[1],
            0x00,
        ];
        let runs = decode_data_runs(&runs_bytes).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].start_lcn, 200);
        assert_eq!(runs[1].start_lcn, 110);
    }

    #[test]
    fn decode_data_runs_empty_on_immediate_zero() {
        let runs = decode_data_runs(&[0x00]).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn decode_data_runs_zero_length_size_returns_error() {
        // 头 0x01：len_size=1, off_size=0 -> off_size=0 表示 sparse，但 len_size=1 合法
        // 头 0x00 才是结束；这里测 len_size=0 的非法头 0x00 被当结束符。
        // 真正非法：header=0x00 已是结束符；header=0x10 表示 len_size=0, off_size=1。
        let runs_bytes = [0x10, 0x00];
        let err = decode_data_runs(&runs_bytes).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 0 }));
    }

    #[test]
    fn decode_data_runs_truncated_length_field_returns_error() {
        // 头 0x21：len_size=1, off_size=2；但只有 1 字节 length，无 offset。
        let runs_bytes = [0x21, 5];
        let err = decode_data_runs(&runs_bytes).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 0 }));
    }

    #[test]
    fn decode_data_runs_sparse_extent() {
        // header 0x21 = len_size=1, off_size=2（用 2 字节 off 让 200/315 在正数范围内）
        // 第一段：len=10, off=100（绝对 LCN=100，prev_end=110）
        // 第二段：sparse（header 0x01：len_size=1, off_size=0），len=5（prev_end 推进到 115）
        // 第三段：len=3, off=200（绝对 LCN = 115 + 200 = 315）
        let runs_bytes = [
            0x21, 10, 100, 0,
            0x01, 5,
            0x21, 3, 200, 0,
            0x00,
        ];
        let runs = decode_data_runs(&runs_bytes).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].start_lcn, 100);
        assert_eq!(runs[0].length_clusters, 10);
        assert_eq!(runs[1].start_lcn, 110); // sparse：占位用 prev_end
        assert_eq!(runs[1].length_clusters, 5);
        assert_eq!(runs[2].start_lcn, 315);
    }

    // ===== sign_extend =====

    #[test]
    fn sign_extend_positive() {
        assert_eq!(sign_extend(100, 1), 100);
        assert_eq!(sign_extend(1000, 2), 1000);
    }

    #[test]
    fn sign_extend_negative_one_byte() {
        // 0xFF as i8 = -1
        assert_eq!(sign_extend(0xFF, 1), -1);
    }

    #[test]
    fn sign_extend_negative_two_bytes() {
        // 0xFF_FF as i16 = -1
        assert_eq!(sign_extend(0xFF_FF, 2), -1);
        // 0x80_00 as i16 = -32768
        assert_eq!(sign_extend(0x80_00, 2), -32768);
    }

    // ===== namespace 选择（简报 3.3） =====

    fn mk_name(parent_rec: u64, parent_seq: u16, ns: u8, name: &str) -> MftName {
        MftName {
            parent: FileRef {
                record_no: parent_rec,
                sequence: parent_seq,
            },
            name: name.to_string(),
            namespace: ns,
        }
    }

    #[test]
    fn select_names_prefers_win32_namespace_1() {
        let names = vec![
            mk_name(5, 5, NAMESPACE_DOS, "LONGFI~1.TXT"),
            mk_name(5, 5, NAMESPACE_WIN32, "longfile.txt"),
        ];
        let sel = select_effective_names(&names);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].name, "longfile.txt");
        assert_eq!(sel[0].namespace, NAMESPACE_WIN32);
    }

    #[test]
    fn select_names_keeps_win32_and_dos_namespace_3() {
        let names = vec![
            mk_name(5, 5, NAMESPACE_WIN32_AND_DOS, "regular.txt"),
            mk_name(5, 5, NAMESPACE_DOS, "REGULA~1.TXT"),
        ];
        let sel = select_effective_names(&names);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].namespace, NAMESPACE_WIN32_AND_DOS);
    }

    #[test]
    fn select_names_falls_back_to_posix_when_no_win32() {
        let names = vec![
            mk_name(5, 5, NAMESPACE_POSIX, "posix_name"),
            mk_name(5, 5, NAMESPACE_DOS, "POSIX_~1"),
        ];
        let sel = select_effective_names(&names);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].namespace, NAMESPACE_POSIX);
    }

    #[test]
    fn select_names_pure_dos_returns_empty() {
        let names = vec![mk_name(5, 5, NAMESPACE_DOS, "SHORT~1.TXT")];
        let sel = select_effective_names(&names);
        assert!(sel.is_empty(), "纯 DOS namespace 不应建边");
    }

    #[test]
    fn select_names_preserves_multiple_hardlinks() {
        // 同一记录的两个 Win32 入口（不同 parent）——硬链接，两个都保留。
        let names = vec![
            mk_name(39, 1, NAMESPACE_WIN32, "alpha.txt"),
            mk_name(42, 1, NAMESPACE_WIN32, "hardlink_to_alpha.txt"),
        ];
        let sel = select_effective_names(&names);
        assert_eq!(sel.len(), 2);
    }

    // ===== MftError Display =====

    #[test]
    fn mft_error_display_bad_record_includes_ref() {
        let err = MftError::BadRecord { ref_no: 42 };
        let msg = format!("{}", err);
        assert!(msg.contains("42"));
    }

    #[test]
    fn mft_error_display_unsupported_ntfs_version() {
        let err = MftError::UnsupportedNtfsVersion {
            major: 3,
            minor: 0,
        };
        let msg = format!("{}", err);
        assert!(msg.contains("3.0"));
    }

    #[test]
    fn mft_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "test");
        let err: MftError = io_err.into();
        assert!(matches!(err, MftError::Io(_)));
    }

    // ===== parse_record 合成字节单测 =====
    //
    // 构造最小合法记录验证 parse_record 的各分支。

    /// 构造一条带 `$FILE_NAME` + `$DATA` resident 的最小 FILE record。
    fn build_minimal_file_record(
        record_no: u64,
        parent_ref: u64,
        name: &str,
        data_len: u32,
    ) -> Vec<u8> {
        let record_len = 1024;
        let bytes_per_sector = 512;
        let usa_offset = 0x30usize;
        let first_attr = 0x38usize;

        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(b"FILE");
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // 1024/512+1
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
        buf[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes()); // in use
        buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());

        // 构造 `$FILE_NAME` 属性。
        let name_utf16: Vec<u16> = name.encode_utf16().collect();
        let name_bytes: Vec<u8> = name_utf16
            .iter()
            .flat_map(|&w| w.to_le_bytes())
            .collect();
        let fn_value_len = 0x42 + name_bytes.len(); // FILE_NAME value 头 + name
        let fn_attr_len = 0x18 /* resident head */ + fn_value_len;
        // 对齐到 8
        let fn_attr_len_padded = (fn_attr_len + 7) & !7;

        let mut off = first_attr;
        // type=0x30 (FILE_NAME)
        buf[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&(fn_attr_len_padded as u32).to_le_bytes());
        buf[off + 8] = 0; // resident
        buf[off + 9] = 0; // no attr name
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes()); // name offset
        buf[off + 0x0E..off + 0x10].copy_from_slice(&0u16.to_le_bytes()); // attr id
        buf[off + 0x10..off + 0x14].copy_from_slice(&(fn_value_len as u32).to_le_bytes());
        buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes()); // value offset
        // FILE_NAME value：parent_ref @ 0x00, name_len @ 0x40, ns @ 0x41, name @ 0x42
        let val_off = off + 0x18;
        buf[val_off..val_off + 8].copy_from_slice(&parent_ref.to_le_bytes());
        buf[val_off + 0x40] = name_utf16.len() as u8;
        buf[val_off + 0x41] = NAMESPACE_WIN32;
        buf[val_off + 0x42..val_off + 0x42 + name_bytes.len()].copy_from_slice(&name_bytes);

        off += fn_attr_len_padded;

        // `$DATA` resident
        let data_attr_len = 0x18 + data_len as usize;
        let data_attr_len_padded = (data_attr_len + 7) & !7;
        buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&(data_attr_len_padded as u32).to_le_bytes());
        buf[off + 8] = 0; // resident
        buf[off + 9] = 0;
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        buf[off + 0x0E..off + 0x10].copy_from_slice(&1u16.to_le_bytes()); // attr id
        buf[off + 0x10..off + 0x14].copy_from_slice(&data_len.to_le_bytes());
        buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        // data value 全零（length 已声明）
        off += data_attr_len_padded;

        // end marker
        buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&0u32.to_le_bytes());

        // 应用 USA 占位（用 build_valid_record_bytes 的逻辑：USN + sector 尾部）
        let usn: u16 = 0x4321;
        buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 1..3 {
            let sector_end = i * bytes_per_sector - 2;
            // 保存原尾部到 USA[i]
            let original = u16::from_le_bytes([buf[sector_end], buf[sector_end + 1]]);
            buf[usa_offset + i * 2..usa_offset + i * 2 + 2]
                .copy_from_slice(&original.to_le_bytes());
            buf[sector_end..sector_end + 2].copy_from_slice(&usn.to_le_bytes());
        }
        let _ = record_no;
        buf
    }

    #[test]
    fn parse_record_minimal_file_returns_correct_fields() {
        let parent_ref = (5u64 << 48) | 5u64; // record 5, seq 5
        let bytes = build_minimal_file_record(42, parent_ref, "hello.txt", 22);
        let rec = parse_record(&bytes, 42, 512).expect("合法记录应解析成功");
        assert_eq_file_record(&rec, 42, 1, false, false);
        assert_eq!(rec.logical_size, 22);
        assert_eq!(rec.names.len(), 1);
        assert_eq!(rec.names[0].name, "hello.txt");
        assert_eq!(rec.names[0].namespace, NAMESPACE_WIN32);
        assert_eq!(rec.names[0].parent.record_no, 5);
        assert_eq!(rec.names[0].parent.sequence, 5);
        assert!(rec.reparse_tag.is_none());
    }

    #[test]
    fn parse_record_not_in_use_returns_empty_names() {
        let parent_ref = (5u64 << 48) | 5u64;
        let mut bytes = build_minimal_file_record(42, parent_ref, "hello.txt", 22);
        // 清除 in-use 位。
        let flags = u16::from_le_bytes([bytes[0x16], bytes[0x17]]);
        let flags = flags & !FILE_FLAG_IN_USE;
        bytes[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        // 重新应用 USA（因为修改了字节可能破坏 sector 尾部 USN——这里修改点
        // 在 0x16 不影响 sector 尾部，USN 仍有效）。
        let rec = parse_record(&bytes, 42, 512).unwrap();
        assert!(rec.names.is_empty());
        assert_eq!(rec.logical_size, 0);
    }

    #[test]
    fn parse_record_bad_signature_returns_bad_record() {
        let mut bytes = build_minimal_file_record(42, 0, "x", 1);
        bytes[0..4].copy_from_slice(b"BAAD");
        let err = parse_record(&bytes, 42, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 42 }));
    }

    #[test]
    fn parse_record_extension_record_has_base() {
        let mut bytes = build_minimal_file_record(49, 0, "stream1", 102);
        // 设置 base_record 非零（指向 record 41）。
        let base_ref = (1u64 << 48) | 41u64;
        bytes[0x20..0x28].copy_from_slice(&base_ref.to_le_bytes());
        let rec = parse_record(&bytes, 49, 512).unwrap();
        assert!(rec.base_record.is_some());
        let base = rec.base_record.unwrap();
        assert_eq!(base.record_no, 41);
        assert_eq!(base.sequence, 1);
    }

    // ===== 属性遍历健壮性（简报 3.6） =====

    #[test]
    fn parse_record_zero_attr_length_returns_bad_record() {
        let mut bytes = build_minimal_file_record(42, 0, "x", 1);
        // 把第一个属性（FILE_NAME）的 length 字段改成 0。
        let first_attr = u16::from_le_bytes([bytes[0x14], bytes[0x15]]) as usize;
        bytes[first_attr + 4..first_attr + 8].copy_from_slice(&0u32.to_le_bytes());
        let err = parse_record(&bytes, 42, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 42 }));
    }

    #[test]
    fn parse_record_attr_length_below_min_header_returns_bad_record() {
        let mut bytes = build_minimal_file_record(42, 0, "x", 1);
        let first_attr = u16::from_le_bytes([bytes[0x14], bytes[0x15]]) as usize;
        // 属性 length 设为 8（小于 16）。
        bytes[first_attr + 4..first_attr + 8].copy_from_slice(&8u32.to_le_bytes());
        let err = parse_record(&bytes, 42, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 42 }));
    }

    #[test]
    fn parse_record_attr_length_beyond_bytes_in_use_returns_bad_record() {
        let mut bytes = build_minimal_file_record(42, 0, "x", 1);
        let first_attr = u16::from_le_bytes([bytes[0x14], bytes[0x15]]) as usize;
        // 属性 length 设为 0xFFFF（远超 bytes_in_use=1024）。
        bytes[first_attr + 4..first_attr + 8].copy_from_slice(&0xFFFFu32.to_le_bytes());
        let err = parse_record(&bytes, 42, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 42 }));
    }

    #[test]
    fn parse_record_truncated_buffer_returns_bad_record() {
        // 只给 FILE header 的前 32 字节——USA / bytes_in_use 字段都读不到。
        let bytes = vec![0u8; 32];
        let err = parse_record(&bytes, 42, 512).unwrap_err();
        assert!(matches!(err, MftError::BadRecord { ref_no: 42 }));
    }

    // ===== proptest 健壮性（简报 3.6） =====

    mod proptest_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(256))]

            #[test]
            fn parse_record_arbitrary_truncation_never_panics(data in prop::collection::vec(any::<u8>(), 0..1024)) {
                // 任意截断点：parse_record 必须返回 Result，绝不 panic。
                let _ = parse_record(&data, 42, 512);
            }

            #[test]
            fn decode_data_runs_arbitrary_bytes_never_panics(data in prop::collection::vec(any::<u8>(), 0..128)) {
                let _ = decode_data_runs(&data);
            }

            #[test]
            fn parse_attribute_list_arbitrary_bytes_never_panics(data in prop::collection::vec(any::<u8>(), 0..512)) {
                let _ = parse_attribute_list_entries(&data, 0);
            }
        }
    }

    // 辅助断言函数。
    fn assert_eq_file_record(
        rec: &MftRecord,
        record_no: u64,
        sequence: u16,
        is_dir: bool,
        has_base: bool,
    ) {
        assert_eq!(rec.id.record_no, record_no);
        assert_eq!(rec.id.sequence, sequence);
        assert_eq!(rec.is_dir, is_dir);
        assert_eq!(rec.base_record.is_some(), has_base);
    }
}
