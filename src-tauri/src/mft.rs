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

use std::collections::{HashMap, HashSet};

use crate::win32::{read_mft_record, read_volume_bytes_at, RawFileRecord, VolumeData, VolumeError, VolumeHandle};
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
/// - `in_use` 来自 FILE header flags bit0（`FILE_RECORD_SEGMENT_IN_USE`），
///   T2 可据此区分"未在用"与"在用但无名且大小 0"的记录。
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
    /// 是否在用（FILE record flags bit0）。
    pub in_use: bool,
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
/// 解码时已按 NTFS 累加语义还原成绝对 LCN）。**sparse extent**（无物理分配）
/// 的 `start_lcn == 0`（NTFS 保留簇，`$MFT` 数据不可能在 LCN 0），T2 读取时
/// 应跳过/补零；`length_clusters` 仍如实计入。
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
            // Sparse extent：无物理分配。start_lcn 设为 0（NTFS 保留簇，`$MFT`
            // 数据不可能在 LCN 0），T2 读取时应跳过/补零。
            0
        } else {
            prev_end_lcn.checked_add(signed_offset).ok_or(MftError::BadRecord {
                ref_no: 0,
            })?
        };
        // 累加位置用于下一段相对偏移。**sparse 段不推进 prev_end**（无物理位置），
        // 下一段 allocated 段仍以最近一个 allocated 段尾部为基准；allocated 段
        // 用本段 length 推进 prev_end。
        if off_size != 0 {
            prev_end_lcn = start_lcn
                .checked_add(length_clusters as i64)
                .ok_or(MftError::BadRecord { ref_no: 0 })?;
        }

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
/// 仅检查签名与最小 header 长度；不验证 USA 几何或内容。**不可**单独用于
/// USA 降级路径（会把真正损坏但签名尚存的记录当有效）——使用
/// [`is_ioctl_fixed_record`] 区分"IOCTL 已修复"与"记录真损坏"。
fn is_minimal_valid_record(bytes: &[u8]) -> bool {
    if bytes.len() < FILE_BYTES_IN_USE_OFFSET + 4 {
        return false;
    }
    bytes[0..4] == FILE_SIGNATURE
}

/// 判断一条记录字节是否为"`FSCTL_GET_NTFS_FILE_RECORD` 在现代 Windows 上
/// 返回的已 USA 修复字节"。
///
/// 现代版本的 IOCTL 返回的记录中，sector 尾部已是应用数据（USA 已应用），
/// USA 数组区域被归零或从未写入——典型特征是 USA 替换数组（`USA[1..usa_count]`）
/// **全为零**。此时 [`apply_usa_fixup`] 会因 sector 尾部既不匹配 USN 也不匹配
/// USA[i] 而失败，但字节本身是正确的（已 fix-up），应降级用原字节。
///
/// 区分"IOCTL 已修复"与"记录真损坏"的判定（全部满足才降级）：
/// 1. FILE 签名正确；
/// 2. 长度至少能容纳 bytes_in_use 字段；
/// 3. USA 几何一致（`usa_count == record_len / bytes_per_sector + 1`，
///    且 `record_len` 是 `bytes_per_sector` 的整数倍）；
/// 4. USA 替换数组（从 `usa_offset + 2` 起的 `(usa_count - 1) * 2` 字节，
///    即 `USA[1..usa_count]`）**全部为零**。
///
/// 若 USA 替换数组非全零，说明 USA 被真实写入、记录本应做 fix-up 却 fix-up
/// 失败（sector 尾部与 USN/USA 都不匹配）——这是真损坏，**不**降级。
pub(crate) fn is_ioctl_fixed_record(bytes: &[u8], bytes_per_sector: u32) -> bool {
    if !is_minimal_valid_record(bytes) {
        return false;
    }
    let record_len = bytes.len();
    let bps = bytes_per_sector as usize;
    if bps == 0 || !record_len.is_multiple_of(bps) {
        return false;
    }
    let usa_offset = match read_u16_at(bytes, FILE_USA_OFFSET_OFFSET, 0) {
        Ok(v) => v as usize,
        Err(_) => return false,
    };
    let usa_count = match read_u16_at(bytes, FILE_USA_COUNT_OFFSET, 0) {
        Ok(v) => v as usize,
        Err(_) => return false,
    };
    let expected_count = record_len / bps + 1;
    if usa_count != expected_count {
        return false;
    }
    // USA 数组（含 USN 自身）必须完整落在 record 内。
    let usa_total_bytes = match usa_count.checked_mul(2) {
        Some(total) if usa_offset + total <= record_len => total,
        _ => return false,
    };
    // 替换数组从 USA[1] 开始（USN 在 USA[0]，占 2 字节），
    // 长度 (usa_count - 1) * 2 字节。
    let repl_start = usa_offset + 2;
    let repl_len = usa_total_bytes.saturating_sub(2);
    if repl_start + repl_len > record_len {
        return false;
    }
    // 全零检查（usa_count>=2 时 repl_len>=2，有实际替换项可检查）。
    bytes[repl_start..repl_start + repl_len]
        .iter()
        .all(|&b| b == 0)
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
    //    字节中，sector 尾部通常已是应用数据（USA 已修复），USA 数组区域被归零。
    //    此时 apply_usa_fixup 会因尾部既不匹配 USN 也不匹配 USA[i] 而返回 BadRecord。
    //    降级路径：当字节满足 is_ioctl_fixed_record（USA 替换数组全零，表明 IOCTL
    //    已 fix-up 而 USA 数组从未写入或已过时归零）时，直接使用原字节。
    //    若 USA 替换数组非全零（说明 USA 被真实写入、本应 fix-up 却失败 = 真损坏），
    //    不降级、返回 BadRecord。
    let fixed = match apply_usa_fixup(bytes, record_no, bytes_per_sector) {
        Ok(fixed) => fixed,
        Err(MftError::BadRecord { .. }) if is_ioctl_fixed_record(bytes, bytes_per_sector) => {
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
            in_use: false,
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
    // 时累计一次逻辑大小（简报 3.3：extension extent 不重复累计，stream identity
    // 含属性名称与 attribute id）。resident `$DATA` 同理（每对 (name, attr_id)
    // 只累加一次）。unnamed `$DATA` 的 name 为空字符串，同样纳入去重键。
    // 真正跨记录的 extension extent 合并在 T2 完成。
    let mut seen_data_streams: HashSet<(String, u16)> = HashSet::new();

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
                // 简报 3.3：stream identity = (name, attribute_id)；同一对
                // 多次出现只累计一次（防御性去重，跨记录合并在 T2）。
                let key = (attr.name.clone(), attr.attribute_id);
                if attr.non_resident {
                    if attr.bytes.len() < ATTR_NONRESIDENT_HEADER_LEN {
                        return Err(MftError::BadRecord { ref_no: record_no });
                    }
                    let lowest_vcn =
                        read_u64_at(attr.bytes, ATTR_NONRES_LOWEST_VCN_OFFSET, record_no)?;
                    if lowest_vcn == 0 && seen_data_streams.insert(key) {
                        let logical =
                            read_u64_at(attr.bytes, ATTR_NONRES_LOGICAL_SIZE_OFFSET, record_no)?;
                        accumulate_data_size(&mut logical_size, attr, logical, record_no)?;
                    }
                } else if seen_data_streams.insert(key) {
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
        in_use: true,
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

// ===== T2：批量读、枚举与索引聚合 =====

impl From<VolumeError> for MftError {
    fn from(e: VolumeError) -> Self {
        match e {
            VolumeError::AccessDenied => MftError::NeedElevation,
            VolumeError::UnsupportedFilesystem { actual } => {
                MftError::UnsupportedFilesystem { actual }
            }
            VolumeError::InvalidVolumeData => MftError::InvalidVolumeData,
            VolumeError::Io { code, operation } => MftError::Io(std::io::Error::other(format!(
                "Win32 I/O error: operation={operation} code={code}"
            ))),
        }
    }
}

/// MFT 枚举聚合产物。
#[derive(Debug, Clone)]
pub struct MftIndex {
    /// 按 `record_no` 索引的 base 与独立记录。
    pub records: HashMap<u64, MftRecord>,
    /// 交给 `parse_record` 并检查过的记录总数（含最终被判坏的记录）。
    pub scanned_records: u64,
    /// 因内容损坏/竞态无法使用的记录数。
    pub skipped_records: u64,
    /// 非目录、在用的 base/独立记录数（按 record 计数）。
    pub scanned_files: u64,
    /// 额外长名入口数（硬链接诊断）。
    pub hard_link_entries: u64,
}

/// 记录读取器抽象。
///
/// T2 批量读路线下，`read(n)` 从 reader 持有的 `$MFT` 字节缓冲按记录号切片，
/// 返回的 `file_reference` 恒等于 `n`。
pub trait RecordReader {
    fn read(&self, requested_record: u64) -> Result<RawFileRecord, MftError>;
}

/// 从记录 0（`$MFT`）的原始字节提取其 non-resident `$DATA`（未命名默认流）
/// 的 Data Run，用于批量读 `$MFT` 文件本身。
///
/// USA 处理与 [`parse_record`] 保持一致：先尝试 fix-up，失败时若判定为
/// IOCTL 已修复记录则降级使用原字节。
pub fn extract_mft_data_runs(
    record0_bytes: &[u8],
    bytes_per_sector: u32,
) -> Result<Vec<DataRun>, MftError> {
    // 与 parse_record USA 路径一致。
    let fixed = match apply_usa_fixup(record0_bytes, 0, bytes_per_sector) {
        Ok(fixed) => fixed,
        Err(MftError::BadRecord { .. }) if is_ioctl_fixed_record(record0_bytes, bytes_per_sector) => {
            record0_bytes.to_vec()
        }
        Err(e) => return Err(e),
    };

    let attrs = walk_attributes(&fixed, 0)?;
    for attr in attrs {
        if attr.type_ == ATTR_TYPE_DATA && attr.non_resident && attr.name.is_empty() {
            if attr.bytes.len() < ATTR_NONRESIDENT_HEADER_LEN {
                return Err(MftError::BadRecord { ref_no: 0 });
            }
            let lowest_vcn = read_u64_at(attr.bytes, ATTR_NONRES_LOWEST_VCN_OFFSET, 0)?;
            if lowest_vcn == 0 {
                let run_offset = read_u16_at(attr.bytes, ATTR_NONRES_RUN_OFFSET_FIELD, 0)? as usize;
                if run_offset > attr.length {
                    return Err(MftError::BadRecord { ref_no: 0 });
                }
                let run_bytes = &attr.bytes[run_offset..attr.length];
                return decode_data_runs(run_bytes);
            }
        }
    }
    Err(MftError::BadRecord { ref_no: 0 })
}

fn validate_volume_data(volume_data: &VolumeData) -> Result<(), MftError> {
    if volume_data.slot_count == 0 || volume_data.bytes_per_file_record_segment == 0 {
        return Err(MftError::InvalidVolumeData);
    }
    if volume_data.major_version != 3 || volume_data.minor_version != 1 {
        return Err(MftError::UnsupportedNtfsVersion {
            major: volume_data.major_version,
            minor: volume_data.minor_version,
        });
    }
    Ok(())
}

/// 批量读 `$MFT` 文件的生产 reader。
///
/// 先读记录 0 定位 `$MFT` Data Run，逐段卷级读取拼成完整缓冲，之后按记录号
/// 随机切片。
pub struct MftFileReader {
    bytes: Vec<u8>,
    record_size: u32,
}

impl MftFileReader {
    /// 读记录 0 定位 `$MFT` Data Run，逐段卷级读取拼成完整缓冲。
    #[cfg(windows)]
    pub fn open(vol: &VolumeHandle, volume_data: VolumeData) -> Result<Self, MftError> {
        validate_volume_data(&volume_data)?;
        let record_size = volume_data.bytes_per_file_record_segment;
        let record0 = read_mft_record(vol, 0, record_size).map_err(MftError::from)?;
        let runs = extract_mft_data_runs(&record0.bytes, volume_data.bytes_per_sector)?;
        if runs.is_empty() {
            return Err(MftError::BadRecord { ref_no: 0 });
        }

        let mut bytes = Vec::with_capacity(volume_data.mft_valid_data_length as usize);
        let bpc = volume_data.bytes_per_cluster as u64;
        let valid_len = volume_data.mft_valid_data_length;

        for run in runs {
            if run.start_lcn < 0 {
                return Err(MftError::InvalidVolumeData);
            }
            let run_bytes = run
                .length_clusters
                .checked_mul(bpc)
                .ok_or(MftError::InvalidVolumeData)?;
            let current_len = bytes.len() as u64;
            if current_len + run_bytes > valid_len {
                return Err(MftError::InvalidVolumeData);
            }
            if run.start_lcn == 0 {
                bytes.resize((current_len + run_bytes) as usize, 0);
            } else {
                let offset = (run.start_lcn as u64)
                    .checked_mul(bpc)
                    .ok_or(MftError::InvalidVolumeData)?;
                let chunk = read_volume_bytes_at(vol, offset, run_bytes).map_err(MftError::from)?;
                bytes.extend_from_slice(&chunk);
            }
        }

        let record_size_u64 = record_size as u64;
        let aligned = (valid_len / record_size_u64) * record_size_u64;
        if (bytes.len() as u64) < aligned {
            return Err(MftError::InvalidVolumeData);
        }
        bytes.truncate(aligned as usize);

        Ok(Self { bytes, record_size })
    }

    /// 测试构造器：直接注入 `$MFT` 字节缓冲。
    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: Vec<u8>, record_size: u32) -> Self {
        Self { bytes, record_size }
    }

    /// `$MFT` 有效数据覆盖的记录数。
    pub fn record_count(&self) -> u64 {
        self.bytes.len() as u64 / self.record_size as u64
    }
}

impl RecordReader for MftFileReader {
    fn read(&self, requested_record: u64) -> Result<RawFileRecord, MftError> {
        let offset = requested_record
            .checked_mul(self.record_size as u64)
            .ok_or_else(|| {
                MftError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "record offset overflow",
                ))
            })?;
        let end = offset + self.record_size as u64;
        if end > self.bytes.len() as u64 {
            return Err(MftError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("record {requested_record} out of range"),
            )));
        }
        Ok(RawFileRecord {
            file_reference: requested_record,
            bytes: self.bytes[offset as usize..end as usize].to_vec(),
        })
    }
}

/// 枚举 `$MFT` 记录并聚合为 [`MftIndex`]。
///
/// - 遍历升序 `0..record_count`，`record_count = mft_valid_data_length / record_size`。
/// - 周期性检查 `cancel` 回调。
/// - 每 4096 条调用 `progress(scanned_records)`。
pub fn enumerate_mft(
    reader: &dyn RecordReader,
    volume_data: VolumeData,
    cancel: &mut dyn FnMut() -> bool,
    progress: &mut dyn FnMut(u64),
) -> Result<MftIndex, MftError> {
    validate_volume_data(&volume_data)?;
    let record_size = volume_data.bytes_per_file_record_segment as u64;
    let record_count = volume_data.mft_valid_data_length / record_size;
    let bytes_per_sector = volume_data.bytes_per_sector;

    let mut records: HashMap<u64, MftRecord> = HashMap::new();
    let mut pending_extensions: HashMap<u64, Vec<MftRecord>> = HashMap::new();
    let mut scanned_records = 0u64;
    let mut skipped_records = 0u64;

    for n in 0..record_count {
        if cancel() {
            return Err(MftError::Cancelled);
        }

        let raw = match reader.read(n) {
            Ok(r) => r,
            Err(_) => {
                // read 失败说明记录号超出 reader 缓冲；这是 reader 层面的损坏/空洞，
                // 不交给 parse_record，只计 skipped（简报 2.4 步骤 2）。
                skipped_records += 1;
                if skipped_records > 100 && skipped_records.saturating_mul(100) > scanned_records {
                    return Err(MftError::ExcessiveRecordErrors {
                        skipped: skipped_records,
                        scanned: scanned_records,
                    });
                }
                continue;
            }
        };

        let record = match parse_record(&raw.bytes, n, bytes_per_sector) {
            Ok(r) => r,
            Err(_) => {
                scanned_records += 1;
                skipped_records += 1;
                if skipped_records > 100 && skipped_records.saturating_mul(100) > scanned_records {
                    return Err(MftError::ExcessiveRecordErrors {
                        skipped: skipped_records,
                        scanned: scanned_records,
                    });
                }
                continue;
            }
        };

        scanned_records += 1;

        if (n + 1) % 4096 == 0 {
            if cancel() {
                return Err(MftError::Cancelled);
            }
            progress(scanned_records);
        }

        if !record.in_use {
            continue;
        }

        if let Some(base) = record.base_record {
            let base_no = base.record_no;
            if let Some(base_rec) = records.get_mut(&base_no) {
                base_rec.logical_size = base_rec
                    .logical_size
                    .checked_add(record.logical_size)
                    .ok_or(MftError::BadRecord { ref_no: base_no })?;
            } else {
                pending_extensions.entry(base_no).or_default().push(record);
            }
        } else {
            let rec_no = record.id.record_no;
            let mut merged = record;
            if let Some(exts) = pending_extensions.remove(&rec_no) {
                for ext in exts {
                    merged.logical_size = merged
                        .logical_size
                        .checked_add(ext.logical_size)
                        .ok_or(MftError::BadRecord { ref_no: rec_no })?;
                }
            }
            records.insert(rec_no, merged);
        }

        if skipped_records > 100 && skipped_records.saturating_mul(100) > scanned_records {
            return Err(MftError::ExcessiveRecordErrors {
                skipped: skipped_records,
                scanned: scanned_records,
            });
        }
    }

    if !records.contains_key(&5) {
        return Err(MftError::RootRecordMissing);
    }

    let mut scanned_files = 0u64;
    let mut hard_link_entries = 0u64;
    for rec in records.values() {
        if rec.is_dir || !rec.in_use {
            continue;
        }
        let effective = select_effective_names(&rec.names);
        let distinct_parents: HashSet<_> = effective.iter().map(|n| n.parent.record_no).collect();
        if effective.len() > 1 && distinct_parents.len() > 1 {
            hard_link_entries += (effective.len() - 1) as u64;
        }
        scanned_files += 1;
    }

    Ok(MftIndex {
        records,
        scanned_records,
        skipped_records,
        scanned_files,
        hard_link_entries,
    })
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
        // header 0x21 = len_size=1, off_size=2（用 2 字节 off 让 200/310 在正数范围内）
        // 第一段：len=10, off=100（绝对 LCN=100，prev_end=110）
        // 第二段：sparse（header 0x01：len_size=1, off_size=0），len=5（start_lcn=0；
        //   sparse 不推进 prev_end，仍为 110）
        // 第三段：len=3, off=200（绝对 LCN = 110 + 200 = 310）
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
        assert_eq!(runs[1].start_lcn, 0); // sparse：无物理分配
        assert_eq!(runs[1].length_clusters, 5);
        assert_eq!(runs[2].start_lcn, 310); // prev_end 仍为 110（sparse 不推进）
    }

    #[test]
    fn decode_data_runs_sparse_has_zero_lcn() {
        // 单一 sparse extent：start_lcn 必须为 0。
        let runs_bytes = [0x01, 7, 0x00];
        let runs = decode_data_runs(&runs_bytes).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start_lcn, 0);
        assert_eq!(runs[0].length_clusters, 7);
    }

    #[test]
    fn decode_data_runs_mixed_allocated_and_sparse() {
        // [allocated, sparse, allocated] 序列：
        //   第一段 allocated：len=10, off=100 -> start_lcn=100, prev_end=110
        //   第二段 sparse：len=5 -> start_lcn=0，prev_end 保持 110（不推进）
        //   第三段 allocated：len=3, off=50 -> start_lcn=110+50=160, prev_end=163
        let runs_bytes = [
            0x11, 10, 100,
            0x01, 5,
            0x11, 3, 50,
            0x00,
        ];
        let runs = decode_data_runs(&runs_bytes).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].start_lcn, 100);
        assert_eq!(runs[0].length_clusters, 10);
        assert_eq!(runs[1].start_lcn, 0);
        assert_eq!(runs[1].length_clusters, 5);
        assert_eq!(runs[2].start_lcn, 160);
        assert_eq!(runs[2].length_clusters, 3);
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
        assert_eq_file_record(&rec, 42, 1, false, false, true);
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
        assert!(!rec.in_use, "in_use 应为 false");
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

    // ===== USA 降级精确化（K1） =====

    /// 构造一条"IOCTL 已修复"的记录：FILE 签名正确、USA 替换数组全零、
    /// sector 尾部为应用数据（非 USN）。
    fn build_ioctl_fixed_record(record_len: usize, bytes_per_sector: usize) -> Vec<u8> {
        assert_eq!(record_len % bytes_per_sector, 0);
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(b"FILE");
        let usa_offset = 0x30usize;
        let first_attr = 0x38usize;
        let usa_count = record_len / bytes_per_sector + 1;
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&(usa_count as u16).to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
        buf[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes()); // in use
        buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());
        // USN = 0（IOCTL 已修复的典型特征：USA 全零）
        // USA 替换数组 [usa_offset+2 .. usa_offset+2*usa_count] 全零（buf 已初始化为零）
        // sector 尾部放应用数据（非 USN），模拟 IOCTL 已 fix-up 的状态
        for i in 1..usa_count {
            let sector_end = i * bytes_per_sector - 2;
            let app_data: u16 = 0xBB00 + i as u16;
            buf[sector_end..sector_end + 2].copy_from_slice(&app_data.to_le_bytes());
        }
        // 写入 end marker（0xFFFFFFFF）以便 walk_attributes 停止
        buf[first_attr..first_attr + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf
    }

    #[test]
    fn parse_record_ioctl_fixed_zero_usa_array_degrades_safely() {
        // 构造 USA 数组全零、sector 尾部为应用数据（非 USN）的记录。
        // apply_usa_fixup 会因尾部既不匹配 USN(0) 也不匹配 USA[i](0) 而失败
        // （因为尾部 0xBB01 != 0），但 is_ioctl_fixed_record 应识别 USA 替换
        // 数组全零，降级用原字节。
        let bytes = build_ioctl_fixed_record(1024, 512);
        let rec = parse_record(&bytes, 99, 512).expect("IOCTL 已修复记录应降级成功");
        assert_eq!(rec.id.record_no, 99);
        assert_eq!(rec.id.sequence, 1);
        assert!(rec.in_use);
        assert!(!rec.is_dir);
    }

    #[test]
    fn parse_record_real_corruption_with_nonzero_usa_rejected() {
        // 构造 USA 数组非零、sector 尾部与 USN 不匹配的记录（模拟真损坏）。
        // is_ioctl_fixed_record 应因 USA 替换数组非零而返回 false，
        // parse_record 不降级、返回 BadRecord。
        let mut bytes = build_ioctl_fixed_record(1024, 512);
        // 在 USA 替换数组中写入非零值（模拟 USA 被真实写入）
        let usa_offset = 0x30usize;
        // USA[1] = 0xDEAD（非零替换值）
        bytes[usa_offset + 2..usa_offset + 4].copy_from_slice(&0xDEADu16.to_le_bytes());
        // sector 1 尾部仍为 0xBB01（既不匹配 USN=0 也不匹配 USA[1]=0xDEAD）
        let err = parse_record(&bytes, 99, 512).unwrap_err();
        assert!(
            matches!(err, MftError::BadRecord { ref_no: 99 }),
            "USA 非零且 sector 尾部不匹配时应返回 BadRecord，不降级"
        );
    }

    #[test]
    fn is_ioctl_fixed_record_allows_zero_usa_array() {
        let bytes = build_ioctl_fixed_record(1024, 512);
        assert!(
            is_ioctl_fixed_record(&bytes, 512),
            "USA 替换数组全零应被识别为 IOCTL 已修复"
        );
    }

    #[test]
    fn is_ioctl_fixed_record_rejects_nonzero_usa_array() {
        let mut bytes = build_ioctl_fixed_record(1024, 512);
        let usa_offset = 0x30usize;
        bytes[usa_offset + 2..usa_offset + 4].copy_from_slice(&0x0001u16.to_le_bytes());
        assert!(
            !is_ioctl_fixed_record(&bytes, 512),
            "USA 替换数组非零不应被识别为 IOCTL 已修复"
        );
    }

    #[test]
    fn is_ioctl_fixed_record_rejects_bad_signature() {
        let mut bytes = build_ioctl_fixed_record(1024, 512);
        bytes[0..4].copy_from_slice(b"BAAD");
        assert!(!is_ioctl_fixed_record(&bytes, 512));
    }

    #[test]
    fn is_ioctl_fixed_record_rejects_wrong_usa_count() {
        let mut bytes = build_ioctl_fixed_record(1024, 512);
        // 把 USA count 改成错误的值
        bytes[0x06..0x08].copy_from_slice(&5u16.to_le_bytes());
        assert!(!is_ioctl_fixed_record(&bytes, 512));
    }

    // ===== $DATA stream 去重（I3） =====

    /// 构造一条含两个同 (name, attribute_id) 的 lowest_vcn==0 non-resident `$DATA`
    /// 的记录（模拟损坏/异常），验证 logical_size 只累加一次。
    fn build_duplicate_data_stream_record() -> Vec<u8> {
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

        // 第一个 non-resident $DATA：unnamed, attr_id=0, lowest_vcn=0, logical_size=1000
        let mut off = first_attr;
        let data1_len = ATTR_NONRESIDENT_HEADER_LEN; // 0x40
        let data1_padded = (data1_len + 7) & !7; // 0x40 已对齐
        buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes()); // $DATA
        buf[off + 4..off + 8].copy_from_slice(&(data1_padded as u32).to_le_bytes());
        buf[off + 8] = 1; // non-resident
        buf[off + 9] = 0; // no attr name
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x40u16.to_le_bytes()); // name offset
        buf[off + 0x0E..off + 0x10].copy_from_slice(&0u16.to_le_bytes()); // attr_id = 0
        // non-resident specific fields
        buf[off + 0x10..off + 0x18].copy_from_slice(&0u64.to_le_bytes()); // lowest_vcn = 0
        buf[off + 0x18..off + 0x20].copy_from_slice(&0u64.to_le_bytes()); // highest_vcn
        buf[off + 0x20..off + 0x22].copy_from_slice(&0x40u16.to_le_bytes()); // run offset
        buf[off + 0x30..off + 0x38].copy_from_slice(&1000u64.to_le_bytes()); // logical_size
        off += data1_padded;

        // 第二个 non-resident $DATA：unnamed, attr_id=0, lowest_vcn=0, logical_size=2000
        // （同 stream 重复，应被去重跳过）
        buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes()); // $DATA
        buf[off + 4..off + 8].copy_from_slice(&(data1_padded as u32).to_le_bytes());
        buf[off + 8] = 1; // non-resident
        buf[off + 9] = 0; // no attr name
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x40u16.to_le_bytes());
        buf[off + 0x0E..off + 0x10].copy_from_slice(&0u16.to_le_bytes()); // attr_id = 0 (same!)
        buf[off + 0x10..off + 0x18].copy_from_slice(&0u64.to_le_bytes()); // lowest_vcn = 0
        buf[off + 0x18..off + 0x20].copy_from_slice(&0u64.to_le_bytes());
        buf[off + 0x20..off + 0x22].copy_from_slice(&0x40u16.to_le_bytes());
        buf[off + 0x30..off + 0x38].copy_from_slice(&2000u64.to_le_bytes()); // logical_size
        off += data1_padded;

        // end marker
        buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&0u32.to_le_bytes());

        // 应用 USA 占位
        let usn: u16 = 0x4321;
        buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 1..3 {
            let sector_end = i * bytes_per_sector - 2;
            let original = u16::from_le_bytes([buf[sector_end], buf[sector_end + 1]]);
            buf[usa_offset + i * 2..usa_offset + i * 2 + 2]
                .copy_from_slice(&original.to_le_bytes());
            buf[sector_end..sector_end + 2].copy_from_slice(&usn.to_le_bytes());
        }
        buf
    }

    #[test]
    fn parse_record_duplicate_data_stream_no_double_count() {
        let bytes = build_duplicate_data_stream_record();
        let rec = parse_record(&bytes, 77, 512).expect("含重复 $DATA 的记录应解析成功");
        // 两个同 (name="", attr_id=0) 的 $DATA，logical_size 只累加第一个 (1000)
        assert_eq!(
            rec.logical_size, 1000,
            "重复 stream 的 logical_size 不应双倍累加"
        );
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

            #[test]
            fn parse_record_arbitrary_usa_offset_count_never_panics(
                usa_offset in 0u16..1024,
                usa_count in 0u16..6,
            ) {
                // 任意 USA offset/count：构造合成记录字节（含 FILE 签名 + header 字段），
                // 喂给 parse_record，断言不 panic（可返回 BadRecord 或 Ok）。
                // 覆盖 usa_count=0、usa_offset 越界等极端值。
                let record_len = 1024usize;
                let bytes_per_sector = 512u32;
                let mut buf = vec![0u8; record_len];
                buf[0..4].copy_from_slice(b"FILE");
                buf[0x04..0x06].copy_from_slice(&usa_offset.to_le_bytes());
                buf[0x06..0x08].copy_from_slice(&usa_count.to_le_bytes());
                buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes()); // sequence
                buf[0x14..0x16].copy_from_slice(&0x38u16.to_le_bytes()); // first attr
                buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes()); // in use
                buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());
                // end marker at first_attr
                buf[0x38..0x3C].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
                // 不 panic 即通过
                let _ = parse_record(&buf, 7, bytes_per_sector);
            }

            #[test]
            fn parse_record_arbitrary_utf16_name_length_never_panics(
                name_length_declared in 0u8..64,
            ) {
                // 生成 $FILE_NAME 属性，name_length 字段声明任意值（含超过 value 边界的值），
                // 喂给 parse_record/parse_file_name，断言不 panic、不越界
                // （声明的 name_length 越界时返回 BadRecord）。
                let record_len = 1024usize;
                let bytes_per_sector = 512u32;
                let usa_offset = 0x30usize;
                let first_attr = 0x38usize;

                let mut buf = vec![0u8; record_len];
                buf[0..4].copy_from_slice(b"FILE");
                buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
                buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes()); // 1024/512+1
                buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
                buf[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
                buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes());
                buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());

                // 构造 $FILE_NAME 属性，value 长度固定 0x42 + 8 字节（只放 4 个 UTF-16 字符）
                let actual_name_chars: usize = 4;
                let fn_value_len = 0x42 + actual_name_chars * 2;
                let fn_attr_len_padded = ((0x18 + fn_value_len + 7) & !7).max(0x20);

                let mut off = first_attr;
                buf[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
                buf[off + 4..off + 8].copy_from_slice(&(fn_attr_len_padded as u32).to_le_bytes());
                buf[off + 8] = 0; // resident
                buf[off + 9] = 0; // no attr name
                buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
                buf[off + 0x0E..off + 0x10].copy_from_slice(&0u16.to_le_bytes());
                buf[off + 0x10..off + 0x14].copy_from_slice(&(fn_value_len as u32).to_le_bytes());
                buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
                let val_off = off + 0x18;
                buf[val_off..val_off + 8].copy_from_slice(&((5u64 << 48) | 5u64).to_le_bytes());
                // 关键：name_length 字段写入 proptest 任意值（可能远超 value 边界）
                buf[val_off + 0x40] = name_length_declared;
                buf[val_off + 0x41] = NAMESPACE_WIN32;
                // 实际只放 actual_name_chars 个 UTF-16 字符
                for i in 0..actual_name_chars {
                    buf[val_off + 0x42 + i * 2] = b'a';
                }

                off += fn_attr_len_padded;
                buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

                // 应用 USA 占位
                let usn: u16 = 0x4321;
                buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
                for i in 1..3usize {
                    let sector_end = i * 512 - 2;
                    let original = u16::from_le_bytes([buf[sector_end], buf[sector_end + 1]]);
                    buf[usa_offset + i * 2..usa_offset + i * 2 + 2]
                        .copy_from_slice(&original.to_le_bytes());
                    buf[sector_end..sector_end + 2].copy_from_slice(&usn.to_le_bytes());
                }
                // 不 panic 即通过（返回 Ok 或 BadRecord 都可）
                let _ = parse_record(&buf, 7, bytes_per_sector);
            }
        }
    }

    // ===== T2：批量读、枚举与索引聚合测试 =====

    const TEST_RECORD_SIZE: u32 = 1024;
    const TEST_BYTES_PER_SECTOR: u32 = 512;

    fn test_volume_data(record_count: u64) -> VolumeData {
        VolumeData {
            bytes_per_sector: TEST_BYTES_PER_SECTOR,
            bytes_per_cluster: 4096,
            bytes_per_file_record_segment: TEST_RECORD_SIZE,
            mft_valid_data_length: record_count * TEST_RECORD_SIZE as u64,
            major_version: 3,
            minor_version: 1,
            slot_count: record_count,
        }
    }

    fn apply_usa_placeholder(buf: &mut [u8], bytes_per_sector: usize, usa_offset: usize) {
        let record_len = buf.len();
        let usa_count = record_len / bytes_per_sector + 1;
        let usn: u16 = 0x4321;
        buf[usa_offset..usa_offset + 2].copy_from_slice(&usn.to_le_bytes());
        for i in 1..usa_count {
            let sector_end = i * bytes_per_sector - 2;
            let original = u16::from_le_bytes([buf[sector_end], buf[sector_end + 1]]);
            buf[usa_offset + i * 2..usa_offset + i * 2 + 2]
                .copy_from_slice(&original.to_le_bytes());
            buf[sector_end..sector_end + 2].copy_from_slice(&usn.to_le_bytes());
        }
    }

    fn build_directory_record(_record_no: u64, parent_ref: u64, name: &str) -> Vec<u8> {
        let mut bytes = build_minimal_file_record(0, parent_ref, name, 0);
        let flags = u16::from_le_bytes([bytes[0x16], bytes[0x17]]) | FILE_FLAG_DIRECTORY;
        bytes[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        bytes
    }

    fn build_not_in_use_record(_record_no: u64) -> Vec<u8> {
        let mut bytes = build_minimal_file_record(0, (5u64 << 48) | 5u64, "x", 0);
        let flags = u16::from_le_bytes([bytes[0x16], bytes[0x17]]) & !FILE_FLAG_IN_USE;
        bytes[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        bytes
    }

    fn build_hardlink_record(
        _record_no: u64,
        parent1: u64,
        name1: &str,
        parent2: u64,
        name2: &str,
    ) -> Vec<u8> {
        let record_len = 1024usize;
        let bytes_per_sector = 512usize;
        let usa_offset = 0x30usize;
        let first_attr = 0x38usize;
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(b"FILE");
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        buf[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());
        buf[0x20..0x28].copy_from_slice(&0u64.to_le_bytes());

        let mut off = first_attr;
        for (idx, (parent_ref, name)) in [(parent1, name1), (parent2, name2)].iter().enumerate() {
            let name_utf16: Vec<u16> = name.encode_utf16().collect();
            let name_bytes: Vec<u8> = name_utf16
                .iter()
                .flat_map(|&w| w.to_le_bytes())
                .collect();
            let fn_value_len = 0x42 + name_bytes.len();
            let fn_attr_len = 0x18 + fn_value_len;
            let fn_attr_len_padded = (fn_attr_len + 7) & !7;

            buf[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
            buf[off + 4..off + 8].copy_from_slice(&(fn_attr_len_padded as u32).to_le_bytes());
            buf[off + 8] = 0;
            buf[off + 9] = 0;
            buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
            buf[off + 0x0E..off + 0x10].copy_from_slice(&(idx as u16).to_le_bytes());
            buf[off + 0x10..off + 0x14].copy_from_slice(&(fn_value_len as u32).to_le_bytes());
            buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
            let val_off = off + 0x18;
            buf[val_off..val_off + 8].copy_from_slice(&parent_ref.to_le_bytes());
            buf[val_off + 0x40] = name_utf16.len() as u8;
            buf[val_off + 0x41] = NAMESPACE_WIN32;
            buf[val_off + 0x42..val_off + 0x42 + name_bytes.len()].copy_from_slice(&name_bytes);
            off += fn_attr_len_padded;
        }

        // resident $DATA attr_id = 2, value length 0
        let data_attr_len = 0x18;
        let data_attr_len_padded = (data_attr_len + 7) & !7;
        buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&(data_attr_len_padded as u32).to_le_bytes());
        buf[off + 8] = 0;
        buf[off + 9] = 0;
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        buf[off + 0x0E..off + 0x10].copy_from_slice(&2u16.to_le_bytes());
        buf[off + 0x10..off + 0x14].copy_from_slice(&0u32.to_le_bytes());
        buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        off += data_attr_len_padded;

        buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&0u32.to_le_bytes());
        apply_usa_placeholder(&mut buf, bytes_per_sector, usa_offset);
        buf
    }

    fn build_same_parent_win32_dos_record(
        _record_no: u64,
        parent_ref: u64,
        long_name: &str,
        short_name: &str,
    ) -> Vec<u8> {
        let record_len = 1024usize;
        let bytes_per_sector = 512usize;
        let usa_offset = 0x30usize;
        let first_attr = 0x38usize;
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(b"FILE");
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        buf[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());
        buf[0x20..0x28].copy_from_slice(&0u64.to_le_bytes());

        let mut off = first_attr;
        for (idx, (parent_ref_val, name, ns)) in [
            (parent_ref, long_name, NAMESPACE_WIN32),
            (parent_ref, short_name, NAMESPACE_DOS),
        ]
        .iter()
        .enumerate()
        {
            let name_utf16: Vec<u16> = name.encode_utf16().collect();
            let name_bytes: Vec<u8> = name_utf16
                .iter()
                .flat_map(|&w| w.to_le_bytes())
                .collect();
            let fn_value_len = 0x42 + name_bytes.len();
            let fn_attr_len = 0x18 + fn_value_len;
            let fn_attr_len_padded = (fn_attr_len + 7) & !7;

            buf[off..off + 4].copy_from_slice(&0x30u32.to_le_bytes());
            buf[off + 4..off + 8].copy_from_slice(&(fn_attr_len_padded as u32).to_le_bytes());
            buf[off + 8] = 0;
            buf[off + 9] = 0;
            buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
            buf[off + 0x0E..off + 0x10].copy_from_slice(&(idx as u16).to_le_bytes());
            buf[off + 0x10..off + 0x14].copy_from_slice(&(fn_value_len as u32).to_le_bytes());
            buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
            let val_off = off + 0x18;
            buf[val_off..val_off + 8].copy_from_slice(&parent_ref_val.to_le_bytes());
            buf[val_off + 0x40] = name_utf16.len() as u8;
            buf[val_off + 0x41] = *ns;
            buf[val_off + 0x42..val_off + 0x42 + name_bytes.len()].copy_from_slice(&name_bytes);
            off += fn_attr_len_padded;
        }

        let data_attr_len = 0x18;
        let data_attr_len_padded = (data_attr_len + 7) & !7;
        buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&(data_attr_len_padded as u32).to_le_bytes());
        buf[off + 8] = 0;
        buf[off + 9] = 0;
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x18u16.to_le_bytes());
        buf[off + 0x0E..off + 0x10].copy_from_slice(&2u16.to_le_bytes());
        buf[off + 0x10..off + 0x14].copy_from_slice(&0u32.to_le_bytes());
        buf[off + 0x14..off + 0x16].copy_from_slice(&0x18u16.to_le_bytes());
        off += data_attr_len_padded;

        buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&0u32.to_le_bytes());
        apply_usa_placeholder(&mut buf, bytes_per_sector, usa_offset);
        buf
    }

    fn build_extension_record(_record_no: u64, base_no: u64, logical_size: u64) -> Vec<u8> {
        let record_len = 1024usize;
        let bytes_per_sector = 512usize;
        let usa_offset = 0x30usize;
        let first_attr = 0x38usize;
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(b"FILE");
        buf[0x04..0x06].copy_from_slice(&(usa_offset as u16).to_le_bytes());
        buf[0x06..0x08].copy_from_slice(&3u16.to_le_bytes());
        buf[0x10..0x12].copy_from_slice(&1u16.to_le_bytes());
        buf[0x14..0x16].copy_from_slice(&(first_attr as u16).to_le_bytes());
        buf[0x16..0x18].copy_from_slice(&0x01u16.to_le_bytes());
        buf[0x18..0x1C].copy_from_slice(&(record_len as u32).to_le_bytes());
        let base_ref = (1u64 << 48) | base_no;
        buf[0x20..0x28].copy_from_slice(&base_ref.to_le_bytes());

        let mut off = first_attr;
        let data_len = ATTR_NONRESIDENT_HEADER_LEN;
        let data_padded = (data_len + 7) & !7;
        buf[off..off + 4].copy_from_slice(&0x80u32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&(data_padded as u32).to_le_bytes());
        buf[off + 8] = 1;
        buf[off + 9] = 0;
        buf[off + 0x0A..off + 0x0C].copy_from_slice(&0x40u16.to_le_bytes());
        buf[off + 0x0E..off + 0x10].copy_from_slice(&0u16.to_le_bytes());
        buf[off + 0x10..off + 0x18].copy_from_slice(&0u64.to_le_bytes());
        buf[off + 0x18..off + 0x20].copy_from_slice(&0u64.to_le_bytes());
        buf[off + 0x20..off + 0x22].copy_from_slice(&0x40u16.to_le_bytes());
        buf[off + 0x30..off + 0x38].copy_from_slice(&logical_size.to_le_bytes());
        off += data_padded;

        buf[off..off + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&0u32.to_le_bytes());
        apply_usa_placeholder(&mut buf, bytes_per_sector, usa_offset);
        buf
    }

    fn build_bad_record() -> Vec<u8> {
        vec![0xFFu8; TEST_RECORD_SIZE as usize]
    }

    struct MockReader {
        records: HashMap<u64, Vec<u8>>,
    }

    impl MockReader {
        fn from_records(records: HashMap<u64, Vec<u8>>, _record_size: u32) -> Self {
            Self { records }
        }
    }

    impl RecordReader for MockReader {
        fn read(&self, requested_record: u64) -> Result<RawFileRecord, MftError> {
            match self.records.get(&requested_record) {
                Some(bytes) => Ok(RawFileRecord {
                    file_reference: requested_record,
                    bytes: bytes.clone(),
                }),
                None => Err(MftError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("mock record {requested_record} missing"),
                ))),
            }
        }
    }

    #[test]
    fn enumeration_visits_all_records_ascending() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(1, build_minimal_file_record(0, (5u64 << 48) | 5u64, "one", 10));
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let mut cancel = || false;
        let mut progress = |_n| {};
        let index = enumerate_mft(&reader, vd, &mut cancel, &mut progress).unwrap();
        assert_eq!(index.scanned_records, 3);
        assert_eq!(index.skipped_records, 3);
        assert!(index.records.contains_key(&0));
        assert!(index.records.contains_key(&1));
        assert!(index.records.contains_key(&5));
    }

    #[test]
    fn enumeration_skips_not_in_use_records() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(1, build_not_in_use_record(1));
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        assert!(!index.records.contains_key(&1));
        assert_eq!(index.scanned_records, 3);
        assert_eq!(index.skipped_records, 3);
        assert_eq!(index.scanned_files, 1); // record 0 only
    }

    #[test]
    fn enumeration_counts_bad_records_as_skipped() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(1, build_bad_record());
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        assert_eq!(index.skipped_records, 4); // 1 bad + 3 missing (2..4 not in mock)
        assert!(!index.records.contains_key(&1));
    }

    #[test]
    fn enumeration_root_missing_returns_error() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(1, build_minimal_file_record(0, (5u64 << 48) | 5u64, "one", 10));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let err = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap_err();
        assert!(matches!(err, MftError::RootRecordMissing));
    }

    #[test]
    fn enumeration_precancel_returns_cancelled() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let err = enumerate_mft(&reader, vd, &mut || true, &mut |_| {}).unwrap_err();
        assert!(matches!(err, MftError::Cancelled));
    }

    #[test]
    fn enumeration_cancel_mid_scan() {
        let mut records = HashMap::new();
        for n in 0..6 {
            records.insert(
                n,
                build_minimal_file_record(0, (5u64 << 48) | 5u64, &format!("f{n}"), 1),
            );
        }
        // record 5 as directory to satisfy root check once we get there
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let mut calls = 0u64;
        let err = enumerate_mft(
            &reader,
            vd,
            &mut || {
                calls += 1;
                calls == 2
            },
            &mut |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, MftError::Cancelled));
        assert_eq!(calls, 2);
    }

    #[test]
    fn enumeration_excessive_errors_threshold() {
        let mut records = HashMap::new();
        // 256 records: first 150 are bad/missing -> >50% and >100 skipped
        for n in 0..256u64 {
            if n < 150 {
                records.insert(n, build_bad_record());
            } else if n == 5 {
                records.insert(n, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
            } else {
                records.insert(
                    n,
                    build_minimal_file_record(0, (5u64 << 48) | 5u64, "f", 1),
                );
            }
        }
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(256);
        let err = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap_err();
        match err {
            MftError::ExcessiveRecordErrors { skipped, scanned } => {
                assert!(skipped > 100);
                assert!(skipped.saturating_mul(100) > scanned);
            }
            other => panic!("期望 ExcessiveRecordErrors，得到 {other:?}"),
        }
    }

    #[test]
    fn enumeration_record_0_passed_to_parser_once() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(1, build_minimal_file_record(0, (5u64 << 48) | 5u64, "one", 10));
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(6);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        assert_eq!(index.scanned_records, 3);
        assert!(index.records.contains_key(&0));
    }

    #[test]
    fn enumeration_extension_merge_sizes() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        records.insert(41, build_minimal_file_record(0, (5u64 << 48) | 5u64, "base", 10));
        records.insert(49, build_extension_record(49, 41, 100));
        records.insert(50, build_extension_record(50, 41, 200));
        for n in 1..51u64 {
            if !records.contains_key(&n) {
                records.insert(
                    n,
                    build_minimal_file_record(0, (5u64 << 48) | 5u64, "f", 1),
                );
            }
        }
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(51);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        let base = index.records.get(&41).expect("base record 41 应存在");
        assert_eq!(base.logical_size, 10 + 100 + 200);
        assert!(!index.records.contains_key(&49));
        assert!(!index.records.contains_key(&50));
    }

    #[test]
    fn enumeration_extension_bounded_before_base() {
        let mut records = HashMap::new();
        records.insert(0, build_minimal_file_record(0, (5u64 << 48) | 5u64, "mft", 0));
        records.insert(5, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
        records.insert(30, build_extension_record(30, 41, 55));
        records.insert(41, build_minimal_file_record(0, (5u64 << 48) | 5u64, "base", 10));
        for n in 1..42u64 {
            if !records.contains_key(&n) {
                records.insert(
                    n,
                    build_minimal_file_record(0, (5u64 << 48) | 5u64, "f", 1),
                );
            }
        }
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(42);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        let base = index.records.get(&41).expect("base record 41 应存在");
        assert_eq!(base.logical_size, 10 + 55);
    }

    #[test]
    fn enumeration_hard_link_entries_counted() {
        let mut records = HashMap::new();
        for n in 0..42u64 {
            if n == 5 {
                records.insert(n, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
            } else if n == 40 {
                records.insert(
                    n,
                    build_hardlink_record(
                        n,
                        (5u64 << 48) | 5u64,
                        "alpha.txt",
                        (1u64 << 48) | 42u64,
                        "hardlink_to_alpha.txt",
                    ),
                );
            } else {
                records.insert(
                    n,
                    build_minimal_file_record(0, (5u64 << 48) | 5u64, "f", 1),
                );
            }
        }
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(42);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        assert_eq!(index.scanned_files, 41); // 42 records - 1 directory
        assert_eq!(index.hard_link_entries, 1);
    }

    #[test]
    fn enumeration_same_parent_win32_dos_not_hard_link() {
        let mut records = HashMap::new();
        for n in 0..46u64 {
            if n == 5 {
                records.insert(n, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
            } else if n == 45 {
                records.insert(
                    n,
                    build_same_parent_win32_dos_record(
                        n,
                        (5u64 << 48) | 5u64,
                        "long filename.txt",
                        "LONGFI~1.TXT",
                    ),
                );
            } else {
                records.insert(
                    n,
                    build_minimal_file_record(0, (5u64 << 48) | 5u64, "f", 1),
                );
            }
        }
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(46);
        let index = enumerate_mft(&reader, vd, &mut || false, &mut |_| {}).unwrap();
        assert_eq!(index.hard_link_entries, 0);
        assert_eq!(index.scanned_files, 45);
    }

    #[test]
    fn enumeration_progress_callback_monotonic() {
        let mut records = HashMap::new();
        for n in 0..4098u64 {
            if n == 5 {
                records.insert(n, build_directory_record(0, (5u64 << 48) | 5u64, "root"));
            } else {
                records.insert(
                    n,
                    build_minimal_file_record(0, (5u64 << 48) | 5u64, "f", 1),
                );
            }
        }
        let reader = MockReader::from_records(records, TEST_RECORD_SIZE);
        let vd = test_volume_data(4098);
        let mut progress_calls = Vec::new();
        let index = enumerate_mft(
            &reader,
            vd,
            &mut || false,
            &mut |n| progress_calls.push(n),
        )
        .unwrap();
        assert!(!progress_calls.is_empty());
        for w in progress_calls.windows(2) {
            assert!(w[0] <= w[1], "progress 应单调不减");
        }
        assert_eq!(index.scanned_records, 4098);
    }

    #[test]
    fn extract_mft_data_runs_from_fixture_record_0() {
        let bytes = std::fs::read("tests/fixtures/ntfs_sample/raw/record_000000.bin")
            .expect("应能读取 fixture record 0");
        let runs = extract_mft_data_runs(&bytes, TEST_BYTES_PER_SECTOR)
            .expect("记录 0 Data Run 提取应成功");
        assert!(!runs.is_empty());
        for run in &runs {
            assert!(run.start_lcn >= 0, "start_lcn 不应为负: {:?}", run);
            assert!(run.length_clusters > 0, "length_clusters 应正: {:?}", run);
        }
        let total_clusters: u64 = runs.iter().map(|r| r.length_clusters).sum();
        let total_bytes = total_clusters * 4096u64;
        assert!(
            total_bytes >= 262_144u64,
            "总字节数 {} 应 >= mft_valid_data_length 262144",
            total_bytes
        );
    }

    #[test]
    fn reader_read_slices_record_n() {
        let r0 = build_minimal_file_record(0, (5u64 << 48) | 5u64, "a", 1);
        let r1 = build_minimal_file_record(0, (5u64 << 48) | 5u64, "b", 2);
        let r2 = build_minimal_file_record(0, (5u64 << 48) | 5u64, "c", 3);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&r0);
        bytes.extend_from_slice(&r1);
        bytes.extend_from_slice(&r2);
        let reader = MftFileReader::from_bytes(bytes, TEST_RECORD_SIZE);
        assert_eq!(reader.record_count(), 3);
        for (n, expected_first_name) in [(0u64, "a"), (1, "b"), (2, "c")] {
            let rec = reader.read(n).unwrap();
            assert_eq!(rec.file_reference, n);
            assert_eq!(rec.bytes.len(), TEST_RECORD_SIZE as usize);
            let parsed = parse_record(&rec.bytes, n, TEST_BYTES_PER_SECTOR).unwrap();
            assert_eq!(parsed.names[0].name, expected_first_name);
        }
    }

    #[test]
    fn reader_read_out_of_range_returns_io_error() {
        let r0 = build_minimal_file_record(0, (5u64 << 48) | 5u64, "a", 1);
        let reader = MftFileReader::from_bytes(r0, TEST_RECORD_SIZE);
        let err = reader.read(1).unwrap_err();
        assert!(matches!(err, MftError::Io(_)));
    }

    // 辅助断言函数。
    fn assert_eq_file_record(
        rec: &MftRecord,
        record_no: u64,
        sequence: u16,
        is_dir: bool,
        has_base: bool,
        in_use: bool,
    ) {
        assert_eq!(rec.id.record_no, record_no);
        assert_eq!(rec.id.sequence, sequence);
        assert_eq!(rec.is_dir, is_dir);
        assert_eq!(rec.base_record.is_some(), has_base);
        assert_eq!(rec.in_use, in_use);
    }
}
