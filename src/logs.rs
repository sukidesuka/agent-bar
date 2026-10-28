use crate::model::{Snapshot, Task, TaskStatus, Usage, now, timestamp};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

const INITIAL_TAIL: u64 = 512 * 1024;
const READ_LIMIT: u64 = 4 * 1024 * 1024;
pub const ACTIVE_TIMEOUT: i64 = 180;

#[derive(Default)]
struct LogState {
    offset: u64,
    skip_line: bool,
    modified: Option<SystemTime>,
    task: Option<Task>,
    active_turn: Option<String>,
    usage: Option<Usage>,
}

impl LogState {
    fn apply(&mut self, record: &Value) {
        let Some(payload) = record.get("payload") else {
            return;
        };
        let at = record
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(timestamp)
            .unwrap_or(0);
        if record["type"] == "session_meta" {
            if let Some(id) = payload
                .get("id")
                .or_else(|| payload.get("session_id"))
                .and_then(Value::as_str)
            {
                let title = payload
                    .get("cwd")
                    .and_then(Value::as_str)
                    .and_then(|s| s.rsplit(['/', '\\']).find(|s| !s.is_empty()))
                    .unwrap_or("Codex 任务");
                self.task = Some(Task {
                    id: id.into(),
                    title: title.into(),
                    status: TaskStatus::Unknown,
                    updated_at: at,
                    started_at: None,
                    inferred: true,
                });
            }
            return;
        }
        if let Some(limits) = payload.get("rate_limits")
            && let Some(usage) = Usage::parse(limits, at, "本地快照")
            && self
                .usage
                .as_ref()
                .is_none_or(|old| usage.observed_at >= old.observed_at)
        {
            self.usage = Some(usage);
        }
        let Some(task) = self.task.as_mut() else {
            return;
        };
        task.updated_at = task.updated_at.max(at);
        if record["type"] == "response_item"
            && payload["type"] == "message"
            && payload["role"] == "user"
            && let Some(content) = payload.get("content").and_then(Value::as_array)
        {
            for item in content {
                let Some(text) = item.get("text").and_then(Value::as_str) else {
                    continue;
                };
                let text = text.trim();
                if text.is_empty() || text.starts_with('<') || text.starts_with("# AGENTS") {
                    continue;
                }
                task.title = text
                    .lines()
                    .next()
                    .unwrap_or(text)
                    .chars()
                    .take(55)
                    .collect();
                break;
            }
        }
        if record["type"] != "event_msg" {
            return;
        }
        let turn = payload
            .get("turn_id")
            .or_else(|| payload.get("root_turn_id"))
            .and_then(Value::as_str);
        match payload["type"].as_str().unwrap_or("") {
            "task_started" => {
                self.active_turn = turn.map(str::to_owned);
                task.started_at = Some(at);
                task.status = TaskStatus::Active;
            }
            "task_complete" | "task_completed" | "turn_aborted" => {
                if turn.is_some()
                    && self.active_turn.is_some()
                    && turn != self.active_turn.as_deref()
                {
                    return;
                }
                task.status = if payload["type"] == "turn_aborted" {
                    TaskStatus::Interrupted
                } else {
                    TaskStatus::Complete
                };
                self.active_turn = None;
            }
            _ => {}
        }
    }

    fn read(&mut self, path: &Path) -> std::io::Result<()> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let modified = meta.modified().ok();
        if self.offset == meta.len() && self.modified == modified {
            return Ok(());
        }
        if meta.len() < self.offset || (meta.len() == self.offset && self.modified != modified) {
            *self = Self::default();
        }
        if self.offset == 0 {
            // Read only a bounded header for identity/title, then the recent tail.
            let mut header = Vec::new();
            file.by_ref().take(256 * 1024).read_to_end(&mut header)?;
            for line in header
                .split_inclusive(|b| *b == b'\n')
                .filter(|l| l.ends_with(b"\n"))
            {
                if let Ok(record) = serde_json::from_slice::<Value>(line) {
                    self.apply(&record);
                }
            }
            self.offset = meta.len().saturating_sub(INITIAL_TAIL);
            file.seek(SeekFrom::Start(self.offset))?;
            self.skip_line = self.offset > 0;
        } else {
            file.seek(SeekFrom::Start(self.offset))?;
        }
        let mut bytes = Vec::new();
        file.take(READ_LIMIT).read_to_end(&mut bytes)?;
        let complete = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
        let start = if self.skip_line {
            if let Some(i) = bytes.iter().position(|b| *b == b'\n') {
                self.skip_line = false;
                i + 1
            } else {
                self.offset += bytes.len() as u64;
                self.modified = modified;
                return Ok(());
            }
        } else {
            0
        };
        for line in bytes[start..complete].split(|b| *b == b'\n') {
            if let Ok(record) = serde_json::from_slice::<Value>(line) {
                self.apply(&record);
            }
        }
        self.offset += complete as u64;
        // Oversized tool output must not block all later lifecycle records.
        if complete == 0 && bytes.len() as u64 == READ_LIMIT {
            self.offset += bytes.len() as u64;
            self.skip_line = true;
        }
        self.modified = modified;
        Ok(())
    }
}

pub struct LogReader {
    home: PathBuf,
    files: HashMap<PathBuf, LogState>,
    discovered_at: i64,
    discovery_error: Option<String>,
}

impl LogReader {
    pub fn new(home: PathBuf) -> Self {
        Self {
            home,
            files: HashMap::new(),
            discovered_at: 0,
            discovery_error: None,
        }
    }

    fn discover(&mut self, at: i64) {
        self.discovered_at = at;
        let mut found = Vec::new();
        fn walk(path: &Path, depth: usize, found: &mut Vec<(SystemTime, PathBuf)>) {
            let Ok(entries) = fs::read_dir(path) else {
                return;
            };
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() && depth > 0 {
                    walk(&p, depth - 1, found);
                } else if p.extension().is_some_and(|e| e == "jsonl")
                    && let Ok(meta) = entry.metadata()
                {
                    found.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), p));
                }
            }
        }
        let sessions = self.home.join("sessions");
        self.discovery_error = fs::read_dir(&sessions)
            .err()
            .map(|e| format!("无法读取会话目录：{e}"));
        walk(&sessions, 3, &mut found);
        found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        found.truncate(48);
        self.files
            .retain(|path, _| found.iter().any(|(_, p)| p == path));
        for (_, path) in found {
            self.files.entry(path).or_default();
        }
    }

    pub fn scan(&mut self) -> Snapshot {
        self.scan_at(now())
    }
    fn scan_at(&mut self, at: i64) -> Snapshot {
        if at - self.discovered_at >= 30 {
            self.discover(at);
        }
        let mut snapshot = Snapshot {
            connection: "本地快照".into(),
            codex_home: self.home.display().to_string(),
            error: self.discovery_error.clone(),
            scanned_at: at,
            ..Snapshot::default()
        };
        for (path, state) in &mut self.files {
            if let Err(e) = state.read(path) {
                snapshot.error = Some(format!("部分会话暂不可读：{e}"));
                continue;
            }
            if let Some(usage) = &state.usage
                && snapshot
                    .usage
                    .as_ref()
                    .is_none_or(|old| usage.observed_at > old.observed_at)
            {
                snapshot.usage = Some(usage.clone());
            }
            if let Some(task) = &state.task {
                let mut task = task.clone();
                if task.status == TaskStatus::Active && at - task.updated_at > ACTIVE_TIMEOUT {
                    task.status = TaskStatus::Unknown;
                }
                snapshot.tasks.push(task);
            }
        }
        snapshot.tasks.sort_by(|a, b| {
            fn priority(s: &TaskStatus) -> u8 {
                match s {
                    TaskStatus::Active | TaskStatus::Waiting => 0,
                    _ => 1,
                }
            }
            priority(&a.status)
                .cmp(&priority(&b.status))
                .then(b.updated_at.cmp(&a.updated_at))
        });
        snapshot.tasks.truncate(12);
        snapshot
    }
}

pub fn default_home() -> PathBuf {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return path.into();
    }
    if let Some(path) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return PathBuf::from(path).join(".codex");
    }
    PathBuf::from(".codex")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "agentbar-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed),
        ));
        let dir = root.join("sessions/2026/10/28");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-test.jsonl");
        (root, path)
    }
    fn line(kind: &str, payload: Value) -> String {
        format!(
            "{}\n",
            json!({"type":kind,"timestamp":"2026-10-28T00:00:00Z","payload":payload})
        )
    }
    #[test]
    fn tails_partial_records_and_uses_matching_turn_end() {
        let (root, path) = fixture();
        let header = line("session_meta", json!({"id":"test","cwd":"/workspace/demo"}));
        let start = line("event_msg", json!({"type":"task_started","turn_id":"a"}));
        let end = line("event_msg", json!({"type":"task_complete","turn_id":"a"}));
        fs::write(&path, format!("{header}{start}{}", &end[..20])).unwrap();
        let mut reader = LogReader::new(root.clone());
        assert_eq!(
            reader.scan_at(1793145601).tasks[0].status,
            TaskStatus::Active
        );
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&end.as_bytes()[20..])
            .unwrap();
        assert_eq!(
            reader.scan_at(1793145602).tasks[0].status,
            TaskStatus::Complete
        );
        let mut state = LogState::default();
        state.apply(&serde_json::from_str::<Value>(&header).unwrap());
        state.apply(&serde_json::from_str::<Value>(&start).unwrap());
        state.apply(
            &json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"other"}}),
        );
        assert_eq!(state.task.unwrap().status, TaskStatus::Active);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn stale_activity_becomes_unknown_and_truncation_is_recovered() {
        let (root, path) = fixture();
        fs::write(
            &path,
            format!(
                "{}{}",
                line("session_meta", json!({"id":"test"})),
                line("event_msg", json!({"type":"task_started","turn_id":"a"}))
            ),
        )
        .unwrap();
        let mut reader = LogReader::new(root.clone());
        assert_eq!(
            reader.scan_at(1793146000).tasks[0].status,
            TaskStatus::Unknown
        );
        fs::write(&path, line("session_meta", json!({"id":"replaced"}))).unwrap();
        assert_eq!(reader.scan_at(1793146001).tasks[0].id, "replaced");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn oversized_tool_output_does_not_block_later_completion() {
        let (root, path) = fixture();
        fs::write(
            &path,
            format!(
                "{}{}",
                line("session_meta", json!({"id":"test"})),
                line("event_msg", json!({"type":"task_started","turn_id":"a"}))
            ),
        )
        .unwrap();
        let mut reader = LogReader::new(root.clone());
        assert_eq!(
            reader.scan_at(1793145601).tasks[0].status,
            TaskStatus::Active
        );
        let mut file = File::options().append(true).open(&path).unwrap();
        file.write_all(&vec![b'x'; READ_LIMIT as usize + 100])
            .unwrap();
        file.write_all(b"\n").unwrap();
        file.write_all(line("event_msg", json!({"type":"task_complete","turn_id":"a"})).as_bytes())
            .unwrap();
        drop(file);
        reader.scan_at(1793145602);
        assert_eq!(
            reader.scan_at(1793145603).tasks[0].status,
            TaskStatus::Complete
        );
        fs::remove_dir_all(root).unwrap();
    }
}
