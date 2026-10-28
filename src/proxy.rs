//! Connect to an EXISTING local Codex daemon. Never start a new app-server,
//! resume a thread, send a turn, or read/copy authentication files.
use crate::model::{Task, TaskStatus, Usage, now, timestamp};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

pub struct LocalProxy {
    child: Child,
    input: ChildStdin,
    messages: Receiver<Value>,
    next_id: u64,
}

impl LocalProxy {
    pub fn connect(executable: &Path, home: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(executable);
        cmd.args(["app-server", "proxy"])
            .env("CODEX_HOME", home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = cmd
            .spawn()
            .map_err(|_| "本地代理不可用，使用会话快照".to_string())?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, messages) = mpsc::sync_channel(128);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                if let Ok(value) = serde_json::from_str(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            input,
            messages,
            next_id: 0,
        };
        client.rpc("initialize",json!({"clientInfo":{"name":"agentbar","title":"AgentBar","version":env!("CARGO_PKG_VERSION")}}))?;
        client.send(&json!({"method":"initialized","params":{}}))?;
        Ok(client)
    }
    fn send(&mut self, value: &Value) -> Result<(), String> {
        writeln!(self.input, "{value}")
            .and_then(|_| self.input.flush())
            .map_err(|_| "本地连接已断开".into())
    }
    fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = self
                .messages
                .recv_timeout(remaining)
                .map_err(|_| "本地代理无响应，使用会话快照".to_string())?;
            if message.get("method").is_some()
                || message.get("id").and_then(Value::as_u64) != Some(id)
            {
                continue;
            }
            if message.get("error").is_some() {
                return Err(format!("本地接口 {method} 暂不可用"));
            }
            return message
                .get("result")
                .cloned()
                .ok_or_else(|| "本地接口响应不完整".into());
        }
    }
    pub fn usage(&mut self) -> Result<Option<Usage>, String> {
        self.rpc("account/rateLimits/read", json!({}))
            .map(|v| Usage::parse(&v, now(), "本地 Codex"))
    }
    pub fn tasks(&mut self) -> Result<Vec<Task>, String> {
        let value = self.rpc("thread/list", json!({"limit":100,"archived":false}))?;
        Ok(value
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(parse_task)
            .collect())
    }
}
impl Drop for LocalProxy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn parse_task(value: &Value) -> Option<Task> {
    let status = value.get("status")?;
    let kind = status.get("type")?.as_str()?;
    let waiting = status
        .get("activeFlags")
        .and_then(Value::as_array)
        .is_some_and(|flags| {
            flags
                .iter()
                .any(|f| matches!(f.as_str(), Some("waitingOnApproval" | "waitingOnUserInput")))
        });
    let state = match kind {
        "active" if waiting => TaskStatus::Waiting,
        "active" => TaskStatus::Active,
        "idle" => TaskStatus::Idle,
        "systemError" => TaskStatus::Unknown,
        _ => return None,
    };
    let time = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(timestamp)))
            .unwrap_or(0)
    };
    Some(Task {
        id: value.get("id")?.as_str()?.into(),
        title: value
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| value.get("preview").and_then(Value::as_str))
            .filter(|s| !s.is_empty())
            .unwrap_or("Codex 任务")
            .chars()
            .take(55)
            .collect(),
        status: state,
        updated_at: time("updatedAt"),
        started_at: None,
        inferred: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unloaded_is_unknown_and_waiting_is_only_taken_from_explicit_flags() {
        assert!(parse_task(&json!({"id":"a","status":{"type":"notLoaded"}})).is_none());
        assert_eq!(
            parse_task(
                &json!({"id":"a","status":{"type":"active","activeFlags":["waitingOnApproval"]}})
            )
            .unwrap()
            .status,
            TaskStatus::Waiting
        );
        assert_eq!(
            parse_task(&json!({"id":"a","status":{"type":"idle"}}))
                .unwrap()
                .status,
            TaskStatus::Idle
        );
    }

    #[cfg(unix)]
    #[test]
    fn proxy_handshake_queries_only_read_methods_and_skips_notifications() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let root = std::env::temp_dir().join(format!(
            "agentbar-proxy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let script = root.join("fake-codex");
        fs::write(&script,r#"#!/usr/bin/env python3
import json,sys
assert sys.argv[1:]==['app-server','proxy']
initialized=False
for line in sys.stdin:
    m=json.loads(line)
    method=m['method']
    if method=='initialized':
        initialized=True
        continue
    if method=='initialize':
        result={}
    else:
        assert initialized
        if method=='account/rateLimits/read':
            result={'rateLimits':{'limitId':'codex','primary':{'usedPercent':42,'windowDurationMins':300,'resetsAt':2000000000}}}
        elif method=='thread/list':
            result={'data':[{'id':'a','name':'test','status':{'type':'active','activeFlags':['waitingOnApproval']}}]}
        else:
            raise AssertionError('unexpected mutating method: '+method)
    print(json.dumps({'method':'thread/status/changed','params':{}}),flush=True)
    print(json.dumps({'id':m['id'],'result':result}),flush=True)
"#).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let mut client = LocalProxy::connect(&script, &root).unwrap();
        assert_eq!(
            client
                .usage()
                .unwrap()
                .unwrap()
                .primary
                .unwrap()
                .used_percent,
            42.0
        );
        assert_eq!(client.tasks().unwrap()[0].status, TaskStatus::Waiting);
        drop(client);
        fs::remove_dir_all(root).unwrap();
    }
}
