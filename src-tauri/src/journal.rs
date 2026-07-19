use crate::error::{AppError, AppResult};
use crate::models::JournalEntry;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Journal {
    pub path: PathBuf,
}

impl Journal {
    pub fn new(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Journal { path })
    }

    pub fn begin(
        &self, task_id: &str, migration_id: &str, op: &str,
        src: &str, dst: &str, tmp: &str, old_path: &str,
    ) -> AppResult<()> {
        // 同源/同目标路径锁：只看每个任务的最新状态（与 recover_pending 一致）
        let all = self.read_all()?;
        let mut latest: std::collections::HashMap<String, JournalEntry> = Default::default();
        for e in &all {
            match &e.final_mark {
                Some(_) => { latest.remove(&e.task_id); }
                None => { latest.insert(e.task_id.clone(), e.clone()); }
            }
        }
        for entry in latest.values() {
            if entry.src.eq_ignore_ascii_case(src) || entry.dst.eq_ignore_ascii_case(dst) {
                return Err(AppError::Conflict(format!(
                    "路径已被运行中任务 {} 占用: {}", entry.task_id, entry.src
                )));
            }
        }
        self.append(&JournalEntry {
            task_id: task_id.into(), migration_id: migration_id.into(), op: op.into(),
            stage: "created".into(), src: src.into(), dst: dst.into(), tmp: tmp.into(),
            old_path: old_path.into(), time: now_iso(), final_mark: None,
        })
    }

    pub fn mark_stage(&self, task_id: &str, stage: &str) -> AppResult<()> {
        // 取该任务最新一条作为模板，更新 stage 追加
        let all = self.read_all()?;
        let tmpl = all.iter().rev().find(|e| e.task_id == task_id)
            .ok_or_else(|| AppError::Store(format!("任务不存在: {task_id}")))?;
        self.append(&JournalEntry {
            stage: stage.into(),
            ..tmpl.clone()
        })
    }

    pub fn complete(&self, task_id: &str) -> AppResult<()> {
        self.finalize(task_id, "completed")
    }

    pub fn fail(&self, task_id: &str, reason: &str) -> AppResult<()> {
        // fail 也写一条终态标记（reason 进 message 通过 mark_stage 不够，简化为终态行）
        let all = self.read_all()?;
        let tmpl = all.iter().rev().find(|e| e.task_id == task_id)
            .ok_or_else(|| AppError::Store(format!("任务不存在: {task_id}")))?;
        self.append(&JournalEntry {
            stage: format!("failed: {reason}"),
            ..tmpl.clone()
        })?;
        self.finalize(task_id, "failed")
    }

    pub fn cancel(&self, task_id: &str) -> AppResult<()> {
        self.finalize(task_id, "canceled")
    }

    fn finalize(&self, task_id: &str, mark: &str) -> AppResult<()> {
        let all = self.read_all()?;
        let tmpl = all.iter().rev().find(|e| e.task_id == task_id)
            .ok_or_else(|| AppError::Store(format!("任务不存在: {task_id}")))?;
        self.append(&JournalEntry {
            final_mark: Some(mark.into()),
            ..tmpl.clone()
        })
    }

    /// 启动时调用：返回所有未终结任务的最新阶段快照。
    pub fn recover_pending(&self) -> AppResult<Vec<JournalEntry>> {
        let all = self.read_all()?;
        let mut latest: std::collections::HashMap<String, JournalEntry> = Default::default();
        for e in all {
            match &e.final_mark {
                Some(_) => { latest.remove(&e.task_id); }
                None => { latest.insert(e.task_id.clone(), e); }
            }
        }
        Ok(latest.into_values().collect())
    }

    fn append(&self, entry: &JournalEntry) -> AppResult<()> {
        let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
        let line = serde_json::to_string(entry)?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }

    fn read_all(&self) -> AppResult<Vec<JournalEntry>> {
        if !self.path.exists() { return Ok(Vec::new()); }
        let f = File::open(&self.path)?;
        let r = BufReader::new(f);
        let mut out = Vec::new();
        for line in r.lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<JournalEntry>(&line) {
                Ok(e) => out.push(e),
                Err(_) => continue, // 损坏行跳过，不阻断恢复
            }
        }
        Ok(out)
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fresh() -> Journal {
        let dir = TempDir::new().unwrap();
        Journal::new(dir.path().to_path_buf()).unwrap()
    }

    #[test]
    fn begin_then_mark_stage_appended() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.mark_stage("t1", "copied").unwrap();
        let pending = j.recover_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].stage, "copied");
    }

    #[test]
    fn complete_removes_from_pending() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.complete("t1").unwrap();
        assert!(j.recover_pending().unwrap().is_empty());
    }

    #[test]
    fn begin_rejects_conflicting_source() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        let err = j.begin("t2", "m2", "migrate", "C:/s", "D:/d2", "D:/d2.tmp", "C:/s.old2");
        assert!(err.is_err(), "同源路径不应允许第二个任务");
    }

    #[test]
    fn begin_allows_different_source() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        let res = j.begin("t2", "m2", "migrate", "C:/s2", "D:/d2", "D:/d2.tmp", "C:/s2.old2");
        // 首版只允许一个迁移任务，但 journal 层只锁源/目标路径冲突，第二个不同源应可写入
        assert!(res.is_ok());
    }

    #[test]
    fn recover_pending_returns_latest_stage_per_task() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.mark_stage("t1", "copied").unwrap();
        j.mark_stage("t1", "manifest_ok").unwrap();
        let pending = j.recover_pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].stage, "manifest_ok", "应取最新阶段");
    }

    #[test]
    fn fail_marks_terminal_and_removed_from_pending() {
        let j = fresh();
        j.begin("t1", "m1", "migrate", "C:/s", "D:/d", "D:/d.tmp", "C:/s.old").unwrap();
        j.fail("t1", "磁盘满").unwrap();
        assert!(j.recover_pending().unwrap().is_empty());
    }

    #[test]
    fn begin_allows_restore_after_migrate_completes() {
        // 回归：migrate 完成后，同一 journal 上 begin restore（同源）不应被路径锁误拒。
        // 旧 begin() 遍历全部历史条目，已完成 migrate 任务的中间 final_mark=None 条目
        // （src 相同）会让随后的 restore begin 误判为路径占用而拒绝。
        // 修复后 begin() 按 task_id 聚合只看最新状态（与 recover_pending 一致），故此用例应通过。
        let j = fresh();
        // migrate 任务走完到 complete
        j.begin("t-mig", "m1", "migrate", "C:/src", "D:/data", "D:/data.tmp", "C:/src.old").unwrap();
        j.mark_stage("t-mig", "copied").unwrap();
        j.complete("t-mig").unwrap();
        // 同源发起 restore：begin 必须成功（migrate 已完成，路径已释放）
        let res = j.begin("t-rst", "m1", "restore", "C:/src", "D:/data", "D:/data.tmp", "C:/src.old");
        assert!(res.is_ok(), "migrate 完成后同 journal begin restore 不应被路径锁误拒: {:?}", res.err());
    }
}
