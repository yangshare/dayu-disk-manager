//! MFT 枚举 spike（任务 0.3）。
//!
//! 用 `FSCTL_GET_NTFS_FILE_RECORD` 逐条枚举 MFT 记录，验证"按返回号低 48 位
//! 递减"能完整覆盖在用记录，作为 go/no-go 门槛。
//!
//! 运行：`cargo run --example mft_spike --release -- C`
//! （需要管理员权限；否则在 `open_volume` 处返回 `AccessDenied`。）
//!
//! 此 example 仅在 windows 平台可执行——非 windows 平台 main 直接报错退出。

use dayu_disk_manager_lib::win32::{open_volume, read_mft_record, read_volume_data, VolumeError};
use std::collections::HashSet;
use std::env;
use std::process::ExitCode;
use std::time::Instant;

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("mft_spike: 仅支持 Windows");
    ExitCode::from(2)
}

#[cfg(windows)]
fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("用法: mft_spike <盘符>  例如: mft_spike C");
        return ExitCode::from(2);
    }
    let drive_arg = &args[1];
    let drive_char = match drive_arg.chars().next() {
        Some(c) if c.is_ascii_alphabetic() => c,
        _ => {
            eprintln!("无效盘符: {drive_arg:?}（应为单个 ASCII 字母，如 C）");
            return ExitCode::from(2);
        }
    };

    let started = Instant::now();
    let outcome = run_spike(drive_char);
    let elapsed = started.elapsed();

    println!();
    println!("===== SPIKE 汇总 =====");
    println!("drive           : {drive_char}:");
    println!("elapsed_ms      : {}", elapsed.as_millis());
    match outcome {
        Ok(summary) => {
            print_summary(&summary);
            // 输出明确 go/no-go 决策
            let decision = if summary.no_go_reason.is_some() {
                "no-go"
            } else {
                "go"
            };
            println!("decision        : {decision}");
            if summary.no_go_reason.is_some() {
                ExitCode::from(1)
            } else {
                ExitCode::from(0)
            }
        }
        Err(err) => {
            println!("error           : {err}");
            println!("decision        : no-go");
            ExitCode::from(1)
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Default)]
struct SpikeSummary {
    /// 总槽位数（slot_count，由 VolumeData 推导）。
    slot_count: u64,
    /// 实际枚举到的不同记录号数量。
    seen: u64,
    /// 单条记录字节长度（来自 VolumeData.bytes_per_file_record_segment）。
    record_segment_size: u32,
    /// 扫描期间检测到的 IO 错误次数（仅可恢复的记录变化）。
    transient_errors: u64,
    /// NTFS 主/次版本号。
    ntfs_major: u16,
    ntfs_minor: u16,
    /// 记录 5（卷根目录 $Root）是否存在。
    root_5_seen: bool,
    /// 记录 5 是否在用（FILE 头 flags & 0x01）。
    root_5_in_use: bool,
    /// 记录 5 是否以 "FILE" 签名开头。
    root_5_has_file_signature: bool,
    /// 若中途决定 no-go，记录原因。
    no_go_reason: Option<String>,
}

#[cfg(windows)]
fn print_summary(s: &SpikeSummary) {
    println!("slot_count      : {}", s.slot_count);
    println!("records_seen    : {}", s.seen);
    println!("record_size     : {} bytes", s.record_segment_size);
    println!("ntfs_version    : {}.{}", s.ntfs_major, s.ntfs_minor);
    println!("transient_errors: {}", s.transient_errors);
    println!(
        "root_5          : seen={} in_use={} file_sig={}",
        s.root_5_seen, s.root_5_in_use, s.root_5_has_file_signature
    );
    if let Some(reason) = &s.no_go_reason {
        println!("no_go_reason    : {reason}");
    }
}

#[cfg(windows)]
fn run_spike(drive: char) -> Result<SpikeSummary, VolumeError> {
    let vol = open_volume(drive)?;
    let vdata = read_volume_data(&vol)?;

    let record_segment_size = vdata.bytes_per_file_record_segment;
    if record_segment_size == 0 {
        return Err(VolumeError::InvalidVolumeData);
    }
    let slot_count = vdata.slot_count;
    if slot_count == 0 {
        return Err(VolumeError::InvalidVolumeData);
    }

    let mut summary = SpikeSummary {
        slot_count,
        record_segment_size,
        ntfs_major: vdata.major_version,
        ntfs_minor: vdata.minor_version,
        ..Default::default()
    };

    let mut seen: HashSet<u64> = HashSet::new();

    // 简报 0.3：从 slot_count - 1 请求，始终使用 API 实际返回的低 48 位记录号推进。
    let mut next_request: u64 = slot_count - 1;

    loop {
        let raw = match read_mft_record(&vol, next_request, record_segment_size) {
            Ok(raw) => raw,
            Err(VolumeError::Io { code, operation }) => {
                // 简报 0.3：普通 I/O/句柄错误立即 no-go，不掩盖全局错误。
                // 仅记录级别的瞬时变化可继续——但单次 IOCTL 失败无法区分"记录变化"
                // 与"全局错误"，保守起见一律 no-go。
                summary.no_go_reason =
                    Some(format!("IO 错误（code={code} op={operation}）——保守 no-go"));
                return Ok(summary);
            }
            Err(other) => return Err(other),
        };

        let returned = raw.file_reference; // 已是低 48 位（read_mft_record 内剥离）

        // 简报 0.3：返回号必须 <= request，否则立即 no-go。
        if returned > next_request {
            summary.no_go_reason = Some(format!(
                "返回号 {returned} > 请求 {next_request}，枚举顺序异常"
            ));
            return Ok(summary);
        }

        // 简报 0.3：重复记录立即 no-go，不能只打印警告。
        if !seen.insert(returned) {
            summary.no_go_reason = Some(format!("记录 {returned} 重复出现（API 不再前进）"));
            return Ok(summary);
        }
        summary.seen += 1;

        // 简报 0.3：记录根 5 是否存在，并解析最小 FILE 签名/in-use 字段。
        if returned == 5 {
            summary.root_5_seen = true;
            // FILE 记录头：偏移 0 处的 "FILE" 签名（4 字节 ASCII）
            summary.root_5_has_file_signature = raw.bytes.len() >= 4 && &raw.bytes[0..4] == b"FILE";
            // flags 字段位于偏移 0x16（22），bit 0 = FILE_RECORD_SEGMENT_IN_USE
            if raw.bytes.len() >= 23 {
                summary.root_5_in_use = (raw.bytes[22] & 0x01) != 0;
            }
        }

        // 简报 0.3：返回记录 0 时先计入结果，再终止；禁止在处理前跳过 0。
        if returned == 0 {
            // $MFT 自身（记录 0）——枚举完成。
            break;
        }

        // 下次请求"返回号 - 1"，驱动会向下取到下一条有效记录。
        // 不会下溢：returned >= 1 这里（returned == 0 已 break）。
        next_request = returned - 1;
    }

    Ok(summary)
}
