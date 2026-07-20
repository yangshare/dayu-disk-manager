//! MFT 记录导出工具（T3 fixture 准备，控制者真机使用）。
//!
//! 枚举指定卷的所有在用 MFT 记录，把每条记录的原始字节（USA 未修复）
//! dump 到输出目录，并写入卷几何参数。导出的字节作为 T3 解析器的真实
//! fixture：解析器对其做 USA fixup 与属性解析，验证对真实 NTFS 字节的
//! 正确性。
//!
//! 运行：`cargo run --example mft_export --release -- <盘符> <输出目录>`
//! 例如：`mft_export F fixtures_out`
//! 需要管理员权限；否则在 `open_volume` 处返回 `AccessDenied`。
//!
//! 仅 windows 平台可执行。

use dayu_disk_manager_lib::win32::{open_volume, read_mft_record, read_volume_data, VolumeError};
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("mft_export: 仅支持 Windows");
    ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("用法: mft_export <盘符> <输出目录>  例如: mft_export F fixtures_out");
        return ExitCode::from(2);
    }
    let drive_char = match args[1].chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c,
        _ => {
            eprintln!("无效盘符: {:?}（应为单个 ASCII 字母）", args[1]);
            return ExitCode::from(2);
        }
    };
    let out_dir = PathBuf::from(&args[2]);
    if let Err(e) = fs::create_dir_all(&out_dir) {
        eprintln!("无法创建输出目录 {out_dir:?}: {e}");
        return ExitCode::from(2);
    }

    match run(drive_char, &out_dir) {
        Ok(count) => {
            println!("导出完成：{count} 条记录 -> {}", out_dir.display());
            ExitCode::from(0)
        }
        Err(e) => {
            eprintln!("错误: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(windows)]
fn run(drive: char, out_dir: &PathBuf) -> Result<usize, Box<dyn std::error::Error>> {
    let vol = open_volume(drive)?;
    let vdata = read_volume_data(&vol)?;
    let record_size = vdata.bytes_per_file_record_segment;
    let slot_count = vdata.slot_count;
    if record_size == 0 || slot_count == 0 {
        return Err(Box::new(VolumeError::InvalidVolumeData));
    }

    // 卷几何参数：T3 解析 USA fixup 需要 bytes_per_sector
    {
        let mut meta = fs::File::create(out_dir.join("volume_meta.txt"))?;
        writeln!(meta, "drive={drive}:")?;
        writeln!(meta, "bytes_per_sector={}", vdata.bytes_per_sector)?;
        writeln!(meta, "bytes_per_cluster={}", vdata.bytes_per_cluster)?;
        writeln!(meta, "bytes_per_file_record_segment={record_size}")?;
        writeln!(meta, "mft_valid_data_length={}", vdata.mft_valid_data_length)?;
        writeln!(
            meta,
            "ntfs_version={}.{}",
            vdata.major_version, vdata.minor_version
        )?;
        writeln!(meta, "slot_count={slot_count}")?;
    }

    let mut next_request: u64 = slot_count - 1;
    let mut seen: HashSet<u64> = HashSet::new();
    let mut count = 0usize;

    loop {
        let raw = read_mft_record(&vol, next_request, record_size)?;
        let returned = raw.file_reference; // 低 48 位
        if returned > next_request {
            eprintln!("警告：返回号 {returned} > 请求 {next_request}，停止");
            break;
        }
        if !seen.insert(returned) {
            eprintln!("警告：记录 {returned} 重复，停止");
            break;
        }

        // dump 原始字节（USA 未修复），文件名按记录号命名
        let path = out_dir.join(format!("record_{returned:06}.bin"));
        fs::write(&path, &raw.bytes)?;
        count += 1;

        if returned == 0 {
            break;
        }
        next_request = returned - 1;
    }

    Ok(count)
}
