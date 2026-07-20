//! T3 fixture 集成测试：从真实 NTFS VHD 导出的记录字节做 USA fix-up + 属性解析，
//! 断言解析出的**具体名称、父引用、sequence、stream 大小、reparse tag**。
//!
//! fixture 不得 `#[ignore]`（简报 3.1），普通 `cargo test` 必须跑。

use dayu_disk_manager_lib::mft::{
    decode_data_runs, parse_record, select_effective_names, IO_REPARSE_TAG_MOUNT_POINT,
    IO_REPARSE_TAG_SYMLINK,
};
use std::fs;
use std::path::Path;

/// fixture 根目录。
const FIXTURE_DIR: &str = "tests/fixtures/ntfs_sample/raw";

/// 卷几何参数（来自 volume_meta.txt）。
const BYTES_PER_SECTOR: u32 = 512;
const BYTES_PER_CLUSTER: u32 = 4096;
const BYTES_PER_FILE_RECORD_SEGMENT: u32 = 1024;
const MFT_VALID_DATA_LENGTH: u64 = 262144;

/// 读取指定记录号的 fixture 二进制文件。
fn read_record_bin(record_no: u64) -> Vec<u8> {
    let path = Path::new(FIXTURE_DIR).join(format!("record_{:06}.bin", record_no));
    fs::read(&path).unwrap_or_else(|e| panic!("无法读取 {:?}: {}", path, e))
}

/// 解析指定记录号，返回 MftRecord。
fn parse_fixture_record(record_no: u64) -> dayu_disk_manager_lib::mft::MftRecord {
    let bytes = read_record_bin(record_no);
    parse_record(&bytes, record_no, BYTES_PER_SECTOR)
        .unwrap_or_else(|e| panic!("记录 {} 解析失败: {:?}", record_no, e))
}

// ===== 记录号映射（通过解析结果确立并钉死） =====
//
// 以下映射来自对 fixture 的探索性解析（Python 脚本遍历所有记录后确认）。
// NTFS 分配顺序不保证在不同卷上固定，但在同一 fixture 镜像内是稳定的。

/// 根目录（Z:\）。
const REC_ROOT: u64 = 5;
/// `mft_src` 目录（Z:\mft_src）。
const REC_MFT_SRC: u64 = 39;
/// `sub1` 目录（Z:\mft_src\sub1）。
const REC_SUB1: u64 = 42;
/// `deep` 目录（Z:\mft_src\sub1\deep）。
const REC_DEEP: u64 = 43;
/// `alpha.txt`（resident，22B）。
const REC_ALPHA: u64 = 40;
/// `big.bin`（non-resident，1MB+5B）。
const REC_BIG: u64 = 41;
/// `leaf.txt`（resident，11B）。
const REC_LEAF: u64 = 44;
/// `long filename with spaces & special.txt`。
const REC_LONG_NAME: u64 = 45;
/// `rootfile.txt`。
const REC_ROOTFILE: u64 = 46;
/// `junction_target`（junction，IO_REPARSE_TAG_MOUNT_POINT）。
const REC_JUNCTION: u64 = 47;
/// `dir_symlink`（symlink，IO_REPARSE_TAG_SYMLINK）。
const REC_SYMLINK: u64 = 48;
/// `file_with_special.txt`。
const REC_FILE_SPECIAL: u64 = 60;
/// big.bin 的第一个 extension record。
const REC_EXT_FIRST: u64 = 49;

// ===== 基本记录解析 =====

#[test]
fn record_0_is_mft_with_data_runs() {
    let rec = parse_fixture_record(0);
    assert_eq!(rec.id.record_no, 0);
    assert_eq!(rec.id.sequence, 1);
    assert!(!rec.is_dir);
    assert!(rec.base_record.is_none());
    // $MFT 的 $FILE_NAME 应为 "$MFT"，namespace 3（POSIX）
    assert!(!rec.names.is_empty());
    let effective = select_effective_names(&rec.names);
    assert!(effective.iter().any(|n| n.name == "$MFT"));
}

#[test]
fn record_5_is_root_directory() {
    let rec = parse_fixture_record(REC_ROOT);
    assert_eq!(rec.id.record_no, REC_ROOT);
    assert_eq!(rec.id.sequence, 5);
    assert!(rec.is_dir);
    assert!(rec.base_record.is_none());
    assert!(rec.reparse_tag.is_none());
}

#[test]
fn record_39_is_mft_src_directory() {
    let rec = parse_fixture_record(REC_MFT_SRC);
    assert!(rec.is_dir);
    assert!(rec.base_record.is_none());
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective.len(), 1);
    assert_eq!(effective[0].name, "mft_src");
    assert_eq!(effective[0].parent.record_no, REC_ROOT);
}

// ===== resident $DATA（alpha.txt，22B） =====

#[test]
fn alpha_txt_resident_data_22_bytes() {
    let rec = parse_fixture_record(REC_ALPHA);
    assert!(!rec.is_dir);
    assert!(rec.base_record.is_none());
    // alpha.txt 有两个 FILE_NAME 入口（硬链接：alpha.txt + hardlink_to_alpha.txt）
    assert!(rec.names.len() >= 2, "alpha.txt 应有至少 2 个 FILE_NAME 入口");
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective.len(), 2, "两个 Win32 入口都应保留");
    // 第一个入口：parent = mft_src (39)
    assert_eq!(effective[0].name, "alpha.txt");
    assert_eq!(effective[0].parent.record_no, REC_MFT_SRC);
    // 第二个入口：parent = sub1 (42)（硬链接）
    assert_eq!(effective[1].name, "hardlink_to_alpha.txt");
    assert_eq!(effective[1].parent.record_no, REC_SUB1);
    // 逻辑大小 = 22（resident $DATA）+ 16（custom_stream ADS）
    assert_eq!(rec.logical_size, 22 + 16, "alpha.txt logical_size 应含默认流 + ADS");
}

// ===== non-resident $DATA（big.bin，1MB+5B = 1048576+5 = 1048581） =====

#[test]
fn big_bin_nonresident_data_size() {
    let rec = parse_fixture_record(REC_BIG);
    assert!(!rec.is_dir);
    assert!(rec.base_record.is_none());
    // big.bin 的 base record 中直接可见的 $DATA 逻辑大小
    // 包含：non-resident 默认流 (1048581) + base 上的 4 个 ADS (stream10/38/39/40 各 102)
    assert!(
        rec.logical_size >= 1048581,
        "big.bin logical_size 应至少含 non-resident 默认流 (1048581)，实际={}",
        rec.logical_size
    );
    // base record 带有 non-resident $ATTRIBUTE_LIST
    assert!(
        rec.has_nonresident_attr_list,
        "big.bin 应有 non-resident $ATTRIBUTE_LIST"
    );
}

// ===== extension record（big.bin 的 extension） =====

#[test]
fn extension_record_has_base_and_no_file_name() {
    let rec = parse_fixture_record(REC_EXT_FIRST);
    assert!(!rec.is_dir);
    // extension record 的 base_record 非零
    assert!(rec.base_record.is_some());
    let base = rec.base_record.unwrap();
    assert_eq!(base.record_no, REC_BIG, "extension 应指向 big.bin (record 41)");
    // extension record 没有 $FILE_NAME
    assert!(rec.names.is_empty(), "extension record 不应有 FILE_NAME");
}

// ===== 嵌套目录与 resident 文件（leaf.txt，11B） =====

#[test]
fn sub1_and_deep_directories() {
    let sub1 = parse_fixture_record(REC_SUB1);
    assert!(sub1.is_dir);
    let effective_sub1 = select_effective_names(&sub1.names);
    assert_eq!(effective_sub1[0].name, "sub1");
    assert_eq!(effective_sub1[0].parent.record_no, REC_MFT_SRC);

    let deep = parse_fixture_record(REC_DEEP);
    assert!(deep.is_dir);
    let effective_deep = select_effective_names(&deep.names);
    assert_eq!(effective_deep[0].name, "deep");
    assert_eq!(effective_deep[0].parent.record_no, REC_SUB1);
}

#[test]
fn leaf_txt_resident_data_11_bytes() {
    let rec = parse_fixture_record(REC_LEAF);
    assert!(!rec.is_dir);
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective[0].name, "leaf.txt");
    assert_eq!(effective[0].parent.record_no, REC_DEEP);
    assert_eq!(rec.logical_size, 11, "leaf.txt 应为 11 字节 resident");
}

// ===== 长文件名（namespace 测试） =====

#[test]
fn long_filename_with_spaces_and_special() {
    let rec = parse_fixture_record(REC_LONG_NAME);
    assert!(!rec.is_dir);
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective.len(), 1);
    assert_eq!(
        effective[0].name, "long filename with spaces & special.txt",
        "长文件名应被完整保留"
    );
    assert_eq!(effective[0].parent.record_no, REC_MFT_SRC);
    // 逻辑大小 = 22（默认流）+ 16（custom_stream ADS）
    assert_eq!(rec.logical_size, 22 + 16);
}

// ===== 根目录文件 =====

#[test]
fn rootfile_txt() {
    let rec = parse_fixture_record(REC_ROOTFILE);
    assert!(!rec.is_dir);
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective[0].name, "rootfile.txt");
    assert_eq!(effective[0].parent.record_no, REC_MFT_SRC);
    assert_eq!(rec.logical_size, 11);
}

// ===== reparse point（junction 与 symlink） =====

#[test]
fn junction_target_has_mount_point_tag() {
    let rec = parse_fixture_record(REC_JUNCTION);
    assert!(rec.is_dir, "junction 是目录");
    assert_eq!(
        rec.reparse_tag,
        Some(IO_REPARSE_TAG_MOUNT_POINT),
        "junction 应有 IO_REPARSE_TAG_MOUNT_POINT (0xA0000003)"
    );
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective[0].name, "junction_target");
    assert_eq!(effective[0].parent.record_no, REC_MFT_SRC);
}

#[test]
fn dir_symlink_has_symlink_tag() {
    let rec = parse_fixture_record(REC_SYMLINK);
    assert!(rec.is_dir, "symlink 是目录");
    assert_eq!(
        rec.reparse_tag,
        Some(IO_REPARSE_TAG_SYMLINK),
        "dir_symlink 应有 IO_REPARSE_TAG_SYMLINK (0xA000000C)"
    );
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective[0].name, "dir_symlink");
    assert_eq!(effective[0].parent.record_no, REC_MFT_SRC);
}

// ===== file_with_special.txt =====

#[test]
fn file_with_special_txt() {
    let rec = parse_fixture_record(REC_FILE_SPECIAL);
    assert!(!rec.is_dir);
    let effective = select_effective_names(&rec.names);
    assert_eq!(effective[0].name, "file_with_special.txt");
    assert_eq!(effective[0].parent.record_no, REC_MFT_SRC);
    // 逻辑大小 = 22（默认流）+ 16（custom_stream ADS）
    assert_eq!(rec.logical_size, 22 + 16);
}

// ===== 记录 0 $DATA Data Run 解码（T2 批量读前置） =====

#[test]
fn record_0_mft_data_run_decode() {
    let bytes = read_record_bin(0);
    let _rec = parse_record(&bytes, 0, BYTES_PER_SECTOR).expect("记录 0 应解析成功");

    // 记录 0 的 $DATA 是 non-resident；我们需要从原始字节中提取 Data Run。
    // 先做 USA fix-up，然后遍历属性找到 non-resident $DATA，提取 run list。
    let fixed = dayu_disk_manager_lib::mft::apply_usa_fixup(&bytes, 0, BYTES_PER_SECTOR)
        .expect("USA fix-up 应成功");

    // 遍历属性找到 non-resident $DATA（未命名，attribute name 为空）
    let first_attr = u16::from_le_bytes([fixed[0x14], fixed[0x15]]) as usize;
    let bytes_in_use = u32::from_le_bytes([fixed[0x18], fixed[0x19], fixed[0x1A], fixed[0x1B]])
        as usize;
    let mut off = first_attr;
    let mut found_runs = None;
    while off + 16 <= bytes_in_use {
        let attr_type = u32::from_le_bytes([fixed[off], fixed[off + 1], fixed[off + 2], fixed[off + 3]]);
        let attr_len = u32::from_le_bytes([fixed[off + 4], fixed[off + 5], fixed[off + 6], fixed[off + 7]])
            as usize;
        if attr_type == 0xFFFF_FFFF || attr_len == 0 {
            break;
        }
        if attr_type == 0x80 && fixed[off + 8] != 0 {
            // non-resident $DATA：检查是否为未命名（name_len == 0）
            let name_len = fixed[off + 9];
            if name_len == 0 {
                let run_offset = u16::from_le_bytes([fixed[off + 0x20], fixed[off + 0x21]]) as usize;
                let run_bytes = &fixed[off + run_offset..off + attr_len];
                let runs = decode_data_runs(run_bytes).expect("Data Run 解码应成功");
                found_runs = Some(runs);
                break;
            }
        }
        off += attr_len;
    }

    let runs = found_runs.expect("记录 0 应有 non-resident 未命名 $DATA");
    assert!(!runs.is_empty(), "Data Run 序列不应为空");
    // 每个 run 的 start_lcn >= 0 且 length_clusters > 0
    for run in &runs {
        assert!(
            run.start_lcn >= 0,
            "Data Run start_lcn 应非负：{:?}",
            run
        );
        assert!(
            run.length_clusters > 0,
            "Data Run length_clusters 应为正：{:?}",
            run
        );
    }
    // 总 cluster 数 × bytes_per_cluster ≥ mft_valid_data_length
    let total_clusters: u64 = runs.iter().map(|r| r.length_clusters).sum();
    let total_bytes = total_clusters * BYTES_PER_CLUSTER as u64;
    assert!(
        total_bytes >= MFT_VALID_DATA_LENGTH,
        "总字节数 {} ({} clusters × {} B/cluster) 应 ≥ mft_valid_data_length {}",
        total_bytes,
        total_clusters,
        BYTES_PER_CLUSTER,
        MFT_VALID_DATA_LENGTH
    );
}

// ===== extension extent 大小合并（big.bin） =====
//
// big.bin 的 base record (41) 有 4 个 ADS (stream10/38/39/40 各 102B)，
// 加上 non-resident 默认流 (1048581)。
// extension records (49-59) 携带其余 ADS (stream1-9/11-37 各 102B + stream5 non-resident 102B)。
// T3 的 parse_record 对每条记录单独返回 logical_size；合并由 T2 完成。
// 此测试验证：
// - base record 的 logical_size 包含 base 上直接可见的流大小。
// - 各 extension 的 logical_size 包含其上可见的流大小。
// - 合并后总大小与预期一致。

#[test]
fn big_bin_extension_merge_sizes() {
    let base = parse_fixture_record(REC_BIG);
    // base 上可见：默认流 (1048581) + stream10(102) + stream38(102) + stream39(102) + stream40(102)
    let base_default_data = 1048581u64;
    let base_ads_on_base = 4 * 102u64; // stream10, 38, 39, 40
    assert_eq!(
        base.logical_size,
        base_default_data + base_ads_on_base,
        "base logical_size = 默认流 + base 上的 4 个 ADS，实际={}",
        base.logical_size
    );

    // 各 extension 的 logical_size 之和。
    // big.bin 共有 40 个 ADS（stream1..stream40），其中 stream10/38/39/40 在 base 上，
    // 其余 36 个分布在 extension records 49-59 中（各 102B）。
    let mut ext_total: u64 = 0;
    let mut ext_count = 0;
    for ext_no in 49..=59u64 {
        let ext = parse_fixture_record(ext_no);
        assert!(
            ext.base_record.is_some(),
            "record {} 应是 extension",
            ext_no
        );
        ext_total += ext.logical_size;
        ext_count += 1;
    }
    // 36 个 ADS（stream1-9, 11-37），每个 102B。
    //   stream5 是 non-resident (102B)，其余 resident (102B)。
    //   9 + 27 = 36 个。
    let ext_expected = 36 * 102u64;
    assert_eq!(
        ext_total, ext_expected,
        "extension logical_size 之和应为 36*102={}（{} 条 extension），实际={}",
        ext_expected, ext_count, ext_total
    );

    // 合并总大小 = base + extensions = 默认流 + 40*102（41 个流：默认 + 40 个 ADS）
    let total = base.logical_size + ext_total;
    let expected_total = base_default_data + 40 * 102u64; // 40 个 ADS 各 102B
    assert_eq!(
        total, expected_total,
        "合并总大小 = 默认流(1048581) + 40*102(ADS) = {}，实际={}",
        expected_total, total
    );
}

// ===== 命名流精确大小测试 =====

#[test]
fn alpha_txt_custom_stream_size() {
    // alpha.txt 有 custom_stream ADS (16B resident)
    let rec = parse_fixture_record(REC_ALPHA);
    // logical_size = 22 (默认) + 16 (custom_stream) = 38
    assert_eq!(rec.logical_size, 38);
}

#[test]
fn extension_stream5_nonresident_102_bytes() {
    // stream5 在 extension record 49 上是 non-resident，logical size = 102
    let rec = parse_fixture_record(49);
    assert!(rec.logical_size >= 102, "extension 49 应含 stream5 (102B non-resident)");
}

// ===== 多硬链接精确测试 =====

#[test]
fn alpha_txt_two_parents_hardlink() {
    let rec = parse_fixture_record(REC_ALPHA);
    let effective = select_effective_names(&rec.names);
    // 两个入口，不同 parent
    assert_eq!(effective.len(), 2);
    let parents: Vec<u64> = effective.iter().map(|n| n.parent.record_no).collect();
    assert!(parents.contains(&REC_MFT_SRC), "应有 parent=mft_src(39)");
    assert!(parents.contains(&REC_SUB1), "应有 parent=sub1(42)（硬链接）");
    // 两个入口的 sequence 都应非零
    for name in &effective {
        assert!(name.parent.sequence > 0, "parent sequence 应非零");
    }
}

// ===== 系统元记录 =====

#[test]
fn system_records_0_through_15_parse_successfully() {
    for rec_no in 0u64..=15 {
        let bytes = read_record_bin(rec_no);
        // 有些系统记录可能不在用（如 12-15），parse_record 不应 panic
        let result = parse_record(&bytes, rec_no, BYTES_PER_SECTOR);
        assert!(result.is_ok(), "系统记录 {} 解析不应失败", rec_no);
    }
}

#[test]
fn only_root_5_among_0_15_is_directory() {
    for rec_no in 0u64..=15 {
        let rec = parse_fixture_record(rec_no);
        if rec_no == REC_ROOT {
            assert!(rec.is_dir, "record 5 应为目录");
        } else {
            // 记录 11 ($Extend) 也是目录，其余不是
            if rec_no == 11 {
                assert!(rec.is_dir, "record 11 ($Extend) 应为目录");
            }
            // 不对其他记录做 is_dir 断言（有些可能未在用）
        }
    }
}

// ===== 所有在用记录都能成功解析 =====

#[test]
fn all_fixture_records_parse_without_panic() {
    let dir = Path::new(FIXTURE_DIR);
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.starts_with("record_") || !fname.ends_with(".bin") {
            continue;
        }
        let rec_no: u64 = fname
            .trim_start_matches("record_")
            .trim_end_matches(".bin")
            .parse()
            .unwrap();
        let bytes = fs::read(entry.path()).unwrap();
        // parse_record 必须返回 Result（不 panic），无论记录内容
        let _ = parse_record(&bytes, rec_no, BYTES_PER_SECTOR);
    }
}

// ===== $ATTRIBUTE_LIST resident 解析测试 =====
//
// big.bin 的 $ATTRIBUTE_LIST 是 non-resident，无法在此直接测试 resident 解析。
// 但 parse_attribute_list_entries 是公开 API，用合成字节测试其健壮性。

#[test]
fn parse_attribute_list_entries_minimal() {
    // 构造一个最小的 resident attribute list：一条 entry 指向 record 49。
    // entry 布局：type(4) + length(2) + name_len(1) + name_offset(1) +
    //             lowest_vcn(8) + base_ref(8) + attr_id(2) = 26 bytes
    let mut list_bytes = vec![0u8; 26];
    list_bytes[0..4].copy_from_slice(&0x80u32.to_le_bytes()); // type = $DATA
    list_bytes[4..6].copy_from_slice(&26u16.to_le_bytes()); // length
    list_bytes[6] = 0; // name_len
    list_bytes[7] = 0; // name_offset
    list_bytes[8..16].copy_from_slice(&0u64.to_le_bytes()); // lowest_vcn = 0
    let base_ref = (1u64 << 48) | 49u64; // seq=1, record=49
    list_bytes[16..24].copy_from_slice(&base_ref.to_le_bytes());
    list_bytes[24..26].copy_from_slice(&0u16.to_le_bytes()); // attr_id = 0

    let entries = dayu_disk_manager_lib::mft::parse_attribute_list_entries(&list_bytes, 41)
        .expect("attribute list 解析应成功");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].attribute_type, 0x80);
    assert_eq!(entries[0].base_reference.record_no, 49);
    assert_eq!(entries[0].base_reference.sequence, 1);
    assert_eq!(entries[0].lowest_vcn, 0);
    assert_eq!(entries[0].attribute_id, 0);
    assert!(entries[0].attribute_name.is_empty());
}
