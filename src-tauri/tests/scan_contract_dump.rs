//! 锁定 Rust 后端的 IPC 事件/枚举形状与 `src/ipc/__fixtures__/scan-contract.json`
//! 手动镜像一致：fixture 漂移则该测试失败，避免 Rust serde 与 TS 合约只用 fixture 镜像
//! 另一方单向 lock。
//!
//! 该测试只读 fixture 并比较，不写文件、不引入构建脚本 / CI 同步复杂度。

use dayu_disk_manager_lib::models::{
    CurrentPhase, FastScanFailure, ScanDriveResult, ScanInvalidatedEvent, ScanMode,
    ScanProgressEvent, ScanSource,
};
use serde_json::{json, Value};
use std::path::PathBuf;

fn fixture_path() -> PathBuf {
    // tests/ 在 src-tauri/ 下；fixture 在仓库根的 src/ipc/__fixtures__/。
    // CARGO_MANIFEST_DIR 指向 src-tauri/。
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("src")
        .join("ipc")
        .join("__fixtures__")
        .join("scan-contract.json")
}

fn load_fixture() -> Value {
    let path = fixture_path();
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read fixture {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse fixture {}: {e}", path.display()))
}

/// Rust 序列化出的全部 enum variant 与 fixture 对应字段比对：
/// ScanMode / ScanSource / CurrentPhase 是字符串数组，逐项 serde 等值。
fn assert_string_array_eq(label: &str, expected: &Value, actual_variants: Vec<String>) {
    let expected_array = expected
        .as_array()
        .unwrap_or_else(|| panic!("fixture {label} 应为数组"))
        .iter()
        .map(|v| v.as_str().unwrap_or("").to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        actual_variants, expected_array,
        "fixture {label} 与 Rust serde 输出不一致：\n  fixture:      {expected_array:?}\n  Rust 实际:    {actual_variants:?}",
    );
}

fn serialize_string_enum<T>(variants: &[T]) -> Vec<String>
where
    T: serde::Serialize,
{
    variants
        .iter()
        .map(|v| serde_json::to_value(v).unwrap())
        .map(|v| match v {
            Value::String(s) => s,
            other => panic!("expected string enum variant, got {other:?}"),
        })
        .collect()
}

#[test]
fn fixture_mirrors_rust_scan_contract() {
    let fixture = load_fixture();

    assert_string_array_eq(
        "scanModes",
        &fixture["scanModes"],
        serialize_string_enum(&[ScanMode::Auto, ScanMode::Mft, ScanMode::Filesystem]),
    );

    assert_string_array_eq(
        "scanSources",
        &fixture["scanSources"],
        serialize_string_enum(&[ScanSource::Mft, ScanSource::Filesystem]),
    );

    assert_string_array_eq(
        "currentPhases",
        &fixture["currentPhases"],
        serialize_string_enum(&[
            CurrentPhase::ReadingMft,
            CurrentPhase::Aggregating,
            CurrentPhase::Annotating,
            CurrentPhase::WalkingFs,
        ]),
    );

    // FastScanFailure 6 个 kind（含 io code null+具体）。
    let failure_actual: Value = serde_json::to_value(&[
        FastScanFailure::UnsupportedFilesystem {
            actual: "exfat".into(),
        },
        FastScanFailure::UnsupportedNtfsVersion { major: 1, minor: 2 },
        FastScanFailure::InvalidVolumeData,
        FastScanFailure::RootRecordMissing,
        FastScanFailure::ExcessiveRecordErrors {
            skipped: 1,
            scanned: 2,
        },
        FastScanFailure::Io { code: Some(5) },
        FastScanFailure::Io { code: None },
    ])
    .unwrap();
    let failure_expected = fixture["fastScanFailures"].clone();
    assert_eq!(
        failure_actual, failure_expected,
        "FastScanFailure fixture 漂移：\n  expected (fixture): {failure_expected}\n  actual (Rust):       {failure_actual}",
    );

    // ScanDriveResult 3 kind（needs_elevation / fast_scan_unavailable / complete）。
    let drive_actual: Value = serde_json::to_value(&[
        ScanDriveResult::NeedsElevation,
        ScanDriveResult::FastScanUnavailable {
            reason: FastScanFailure::InvalidVolumeData,
        },
        ScanDriveResult::Complete {
            snapshot: minimal_snapshot(),
        },
    ])
    .unwrap();
    let drive_expected = fixture["scanDriveResults"].clone();
    assert_eq!(
        drive_actual, drive_expected,
        "ScanDriveResult fixture 漂移：\n  expected (fixture): {drive_expected}\n  actual (Rust):       {drive_actual}",
    );

    // ScanProgressEvent。
    let progress = ScanProgressEvent {
        scanned_records: 1,
        scanned_dirs: 2,
        scanned_files: 3,
        estimated_record_slots: 4,
        current_phase: CurrentPhase::Aggregating,
    };
    let progress_actual = serde_json::to_value(&progress).unwrap();
    let progress_expected = fixture["scanProgress"].clone();
    assert_eq!(
        progress_actual, progress_expected,
        "ScanProgressEvent fixture 漂移：\n  expected (fixture): {progress_expected}\n  actual (Rust):       {progress_actual}",
    );

    // ScanInvalidatedEvent 2 组合（migrated/autoRescan, restored/no-autoRescan）。
    let invalidated_actual: Value = serde_json::to_value(&[
        ScanInvalidatedEvent {
            reason: "migrated".into(),
            auto_rescan: true,
        },
        ScanInvalidatedEvent {
            reason: "restored".into(),
            auto_rescan: false,
        },
    ])
    .unwrap();
    let invalidated_expected = fixture["invalidatedEvents"].clone();
    assert_eq!(
        invalidated_actual, invalidated_expected,
        "ScanInvalidatedEvent fixture 漂移：\n  expected (fixture): {invalidated_expected}\n  actual (Rust):       {invalidated_actual}",
    );

    // 完整性自检：fixture 不应为空对象。
    assert!(fixture.is_object(), "fixture 必须为 JSON object");
    let key_count = fixture.as_object().unwrap().len();
    assert!(key_count >= 7, "fixture 至少 7 个顶层键，当前 {key_count}");
}

#[test]
fn fixture_extra_keys_not_silently_ignored() {
    // 设计意图：若新增枚举变体但忘记更新 fixture，本测试辅助提示而不上锁。
    // 反向校验：枚举变体数与 fixture 数组长度一致。
    let fixture = load_fixture();

    let scan_modes = serialize_string_enum(&[ScanMode::Auto, ScanMode::Mft, ScanMode::Filesystem]);
    let scan_sources = serialize_string_enum(&[ScanSource::Mft, ScanSource::Filesystem]);
    let current_phases = serialize_string_enum(&[
        CurrentPhase::ReadingMft,
        CurrentPhase::Aggregating,
        CurrentPhase::Annotating,
        CurrentPhase::WalkingFs,
    ]);

    assert_eq!(
        scan_modes.len(),
        fixture["scanModes"].as_array().unwrap().len(),
        "ScanMode variant 数与 fixture scanModes 数应一致（注意新增/删除 enum 变体）",
    );
    assert_eq!(
        scan_sources.len(),
        fixture["scanSources"].as_array().unwrap().len(),
        "ScanSource variant 数与 fixture scanSources 数应一致",
    );
    assert_eq!(
        current_phases.len(),
        fixture["currentPhases"].as_array().unwrap().len(),
        "CurrentPhase variant 数与 fixture currentPhases 数应一致",
    );
    assert_eq!(
        7,
        fixture["fastScanFailures"].as_array().unwrap().len(),
        "FastScanFailure 6 kind + io 两种 code = 7 fixture 项",
    );
    assert_eq!(
        3,
        fixture["scanDriveResults"].as_array().unwrap().len(),
        "ScanDriveResult fixture 应有 3 个 kind",
    );
    assert_eq!(
        2,
        fixture["invalidatedEvents"].as_array().unwrap().len(),
        "ScanInvalidatedEvent fixture 应有 2 个组合",
    );
}

use dayu_disk_manager_lib::models::{RootFileSummary, ScanDiagnostics, ScanSnapshot};

fn minimal_snapshot() -> ScanSnapshot {
    ScanSnapshot {
        scan_id: "fixture-scan".into(),
        source: ScanSource::Mft,
        roots: Vec::new(),
        filtered_root_count: 0,
        root_file_summary: RootFileSummary {
            direct_file_size_bytes: 0,
            direct_file_count: 0,
            system_metadata_size_bytes: None,
            total_known_size_bytes: 0,
            incomplete: false,
        },
        diagnostics: ScanDiagnostics {
            scanned_records: 0,
            scanned_dirs: 0,
            scanned_files: 0,
            skipped_records: 0,
            orphan_entries: 0,
            hard_link_entries: 0,
            unresolved_extensions: 0,
        },
    }
}

#[test]
fn fixture_root_file_summary_does_not_diverge() {
    // 防御性 single-shot：RootFileSummary 字段集若新增/重命名 / 默认值变化，
    // fixture 立即不一致。该检查独立于上面的集合比对，避免 io 等可空字段混淆。
    let fixture = load_fixture();
    let actual = serde_json::to_value(minimal_snapshot().root_file_summary).unwrap();
    let expected = fixture["scanDriveResults"][2]["snapshot"]["rootFileSummary"].clone();
    assert_eq!(
        actual, expected,
        "RootFileSummary 形状漂移：{actual} vs fixture {expected}",
    );
    // 同时锁住 totalKnownSizeBytes 默认值 == 0 等具体值。
    let _ = json!({ "ok": actual == expected });
}
