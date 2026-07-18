use dayu_disk_manager_lib::journal::Journal;
use dayu_disk_manager_lib::app_state::recover_pending_decisions;
use tempfile::TempDir;

#[test]
fn crash_after_copied_recovers_as_clean_tmp_retry() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.mark_stage("t1", "copied").unwrap();
    // 模拟"重启"：重新打开同一 journal
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let pending = j2.recover_pending().unwrap();
    assert_eq!(pending.len(), 1);
    let decisions = recover_pending_decisions(&pending);
    assert!(decisions[0].2.contains("清 tmp"));
}

#[test]
fn crash_after_source_renamed_recovers_as_rename_back() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.mark_stage("t1", "copied").unwrap();
    j.mark_stage("t1", "source_renamed").unwrap();
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let pending = j2.recover_pending().unwrap();
    let decisions = recover_pending_decisions(&pending);
    assert!(decisions[0].2.contains("改回原名"));
}

#[test]
fn crash_after_junction_created_keeps_link_and_advises() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.mark_stage("t1", "junction_created").unwrap();
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let pending = j2.recover_pending().unwrap();
    let decisions = recover_pending_decisions(&pending);
    assert!(decisions[0].2.contains("已建链"));
}

#[test]
fn restart_blocks_new_migrate_on_same_source_pending() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    // 重启后对同源发起新迁移应被 journal 路径锁拒绝
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    let res = j2.begin("t2", "m-t2", "migrate", "C:/src", "D:/d2", "D:/d2.tmp", "C:/src.old2");
    assert!(res.is_err(), "同源 pending 时新迁移应被拒绝");
}

#[test]
fn completed_task_not_in_pending_after_restart() {
    let dir = TempDir::new().unwrap();
    let j = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    j.begin("t1", "m-t1", "migrate", "C:/src", "D:/d", "D:/d.tmp", "C:/src.old").unwrap();
    j.complete("t1").unwrap();
    let j2 = Journal::new(dir.path().join("journal.jsonl")).unwrap();
    assert!(j2.recover_pending().unwrap().is_empty());
}
