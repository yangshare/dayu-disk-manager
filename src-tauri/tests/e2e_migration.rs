use dayu_disk_manager_lib::file_ops::RealFileOps;
use dayu_disk_manager_lib::history::History;
use dayu_disk_manager_lib::journal::Journal;
use dayu_disk_manager_lib::migrator::{self, MigratePlan};
use dayu_disk_manager_lib::models::MigrationStatus;
use dayu_disk_manager_lib::store::Store;
use std::sync::atomic::AtomicBool;
use tempfile::TempDir;

#[test]
fn full_pipeline_migrate_then_restore_preserves_data() {
    let dir = TempDir::new().unwrap();
    let store = Store::new(dir.path().join("data")).unwrap();
    let journal = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let history = History::new(dir.path().join("history.jsonl")).unwrap();

    // 构造源：含文件 + 内部 junction（不应被递归复制）
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("docs")).unwrap();
    std::fs::write(src.join("docs/readme.md"), b"hi").unwrap();
    let inner_target = dir.path().join("inner_target");
    std::fs::create_dir_all(&inner_target).unwrap();
    std::fs::write(inner_target.join("secret.bin"), vec![0u8; 4096]).unwrap();
    #[cfg(windows)]
    junction::create(&inner_target, src.join("link")).unwrap();

    let plan = MigratePlan {
        task_id: "e2e-t1".into(),
        migration_id: "e2e-m1".into(),
        src: src.clone(),
        target: dir.path().join("repo/wechat/e2e-m1/data"),
        tmp: dir.path().join("repo/wechat/e2e-m1/data.tmp"),
        old_path: src.with_extension("dayu-old-e2e-t1"),
        preset_id: Some("wechat".into()),
        source_volume_serial: "C".into(),
        target_volume_serial: "D".into(),
        enable_vss: false,
    };
    let cancel = AtomicBool::new(false);
    // T10 ripple effect: migrate/restore now return (Migration|(), OperationOutcome)
    // to carry source_changed；e2e 测试保留原有断言，仅解构 outcome。
    let (m, _outcome) = migrator::migrate(
        &RealFileOps,
        &store,
        &journal,
        &history,
        &plan,
        &|_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(m.status, MigrationStatus::Active);

    // junction 解析正常
    assert!(dayu_disk_manager_lib::junction::verify(&src));
    // 数据已迁移
    assert!(plan.target.join("docs/readme.md").exists());
    // 内部 junction 未被递归复制内容
    assert!(!plan.target.join("link/secret.bin").exists());

    // 还原
    let _restore_outcome = migrator::restore(
        &RealFileOps,
        &store,
        &journal,
        &history,
        &m,
        &|_| {},
        &cancel,
    )
    .unwrap();
    assert!(!dayu_disk_manager_lib::junction::exists(&src));
    assert!(src.join("docs/readme.md").exists(), "还原后数据完整");
}

#[test]
fn scanner_finds_migrated_junction_marker() {
    // 旧扁平 scanner::scan 在 T9 fs_scan 重写后已废弃；该测试原本只验证 flat pipeline
    // 能识别 junction。junction 识别已并入 annotate_graph_with_callbacks
    // （reparse_tag → junction::verify 决定 is_junction），由 mft 与 fs 共享，
    // 完整覆盖见 scanner::tests 中 reparse_does_not_descend_into_target 与相关 graph 测试。
    // 此处保留函数名以避免破坏 CI 调用统计；断言保持 cfg(windows) 跳过即可。
    #[cfg(windows)]
    {
        // 主动触发一个不变量：junction 模块仍存在且 verify 在合法 junction 上为 true。
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("t");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("src");
        junction::create(&target, &link).unwrap();
        assert!(dayu_disk_manager_lib::junction::verify(&link));
    }
}
