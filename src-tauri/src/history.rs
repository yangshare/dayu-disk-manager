use crate::error::AppResult;
use crate::models::HistoryEntry;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

#[derive(Clone)]
pub struct History {
    pub path: PathBuf,
}

impl History {
    pub fn new(path: PathBuf) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(History { path })
    }

    pub fn append(&self, e: &HistoryEntry) -> AppResult<()> {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(e)?;
        writeln!(f, "{line}")?;
        f.sync_all()?;
        Ok(())
    }

    /// 按 op 与时间区间 [from, to)（ISO8601 字符串字典序比较）筛选；None 表示不过滤。
    pub fn list(
        &self,
        op_filter: Option<&str>,
        time_range: Option<(&str, &str)>,
    ) -> AppResult<Vec<HistoryEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let f = File::open(&self.path)?;
        let r = BufReader::new(f);
        let mut out = Vec::new();
        for line in r.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let e: HistoryEntry = match serde_json::from_str(&line) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if let Some(op) = op_filter {
                if e.op != op {
                    continue;
                }
            }
            if let Some((from, to)) = time_range {
                if e.time.as_str() < from || e.time.as_str() >= to {
                    continue;
                }
            }
            out.push(e);
        }
        // 按时间升序（append 顺序通常已是升序，这里显式排序保证）
        out.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(out)
    }

    /// 导出全部历史为单个 JSON 数组（设置页"导出操作日志"用）。
    pub fn export_all_json(&self) -> AppResult<String> {
        let all = self.list(None, None)?;
        Ok(serde_json::to_string_pretty(&all)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::HistoryEntry;
    use tempfile::TempDir;

    fn fresh() -> (History, TempDir) {
        let dir = TempDir::new().unwrap();
        let h = History::new(dir.path().join("history.jsonl")).unwrap();
        (h, dir)
    }

    fn entry(op: &str, result: &str, time: &str) -> HistoryEntry {
        HistoryEntry {
            op: op.into(),
            id: "u1".into(),
            src: "C:/s".into(),
            dst: "D:/d".into(),
            result: result.into(),
            time: time.into(),
            duration_sec: 10,
        }
    }

    #[test]
    fn append_then_list_returns_in_order() {
        let (h, _dir) = fresh();
        h.append(&entry("migrate", "ok", "2026-07-18T10:00:00Z"))
            .unwrap();
        h.append(&entry("restore", "ok", "2026-07-18T11:00:00Z"))
            .unwrap();
        let all = h.list(None, None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].op, "migrate");
    }

    #[test]
    fn list_filter_by_op() {
        let (h, _dir) = fresh();
        h.append(&entry("migrate", "ok", "2026-07-18T10:00:00Z"))
            .unwrap();
        h.append(&entry("restore", "ok", "2026-07-18T11:00:00Z"))
            .unwrap();
        h.append(&entry("migrate", "failed", "2026-07-18T12:00:00Z"))
            .unwrap();
        let only_migrate = h.list(Some("migrate"), None).unwrap();
        assert_eq!(only_migrate.len(), 2);
        assert!(only_migrate.iter().all(|e| e.op == "migrate"));
    }

    #[test]
    fn list_filter_by_time_range() {
        let (h, _dir) = fresh();
        h.append(&entry("migrate", "ok", "2026-07-18T10:00:00Z"))
            .unwrap();
        h.append(&entry("migrate", "ok", "2026-07-18T11:30:00Z"))
            .unwrap();
        let ranged = h
            .list(None, Some(("2026-07-18T11:00:00Z", "2026-07-18T12:00:00Z")))
            .unwrap();
        assert_eq!(ranged.len(), 1);
    }
}
