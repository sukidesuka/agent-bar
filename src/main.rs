#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use agentbar::model::now;
use agentbar::{
    logs::{LogReader, default_home},
    model::Snapshot,
    proxy::LocalProxy,
};
use std::path::PathBuf;
#[cfg(windows)]
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(windows)]
mod windows;

struct Options {
    home: PathBuf,
    codex: PathBuf,
    logs_only: bool,
    dump: bool,
    demo: bool,
    offset: i32,
    width: i32,
}
fn options() -> Result<Options, String> {
    let mut o = Options {
        home: default_home(),
        codex: "codex".into(),
        logs_only: false,
        dump: false,
        demo: false,
        offset: 12,
        width: 340,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--codex-home"=>o.home=args.next().ok_or("--codex-home 缺少目录")?.into(),
            "--codex-command"=>o.codex=args.next().ok_or("--codex-command 缺少 Codex CLI 路径")?.into(),
            "--logs-only"=>o.logs_only=true, "--dump"=>o.dump=true, "--demo"=>o.demo=true,
            "--offset"=>o.offset=args.next().ok_or("--offset 缺少数值")?.parse::<i32>().map_err(|_|"--offset 需要整数")?.clamp(0,2000),
            "--width"=>o.width=args.next().ok_or("--width 缺少数值")?.parse::<i32>().map_err(|_|"--width 需要整数")?.clamp(280,600),
            "--help" | "-h" => return Err("AgentBar 0.1\n--codex-home <目录>\n--codex-command <Codex CLI 路径>\n--logs-only 仅读取日志\n--dump 输出一次 JSON 诊断\n--demo 展示带有“演示”标记的模拟数据\n--offset <左侧边距，默认12>\n--width <宽度，默认340>".into()),
            _=>return Err(format!("未知参数：{arg}")),
        }
    }
    Ok(o)
}

fn demo() -> Snapshot {
    use agentbar::model::*;
    Snapshot {
        usage: Some(Usage {
            primary: Some(QuotaWindow {
                used_percent: 23.0,
                window_minutes: 300,
                resets_at: now() + 7200,
            }),
            secondary: Some(QuotaWindow {
                used_percent: 13.0,
                window_minutes: 10080,
                resets_at: now() + 172800,
            }),
            observed_at: now(),
            source: "演示数据".into(),
        }),
        tasks: vec![Task {
            id: String::new(),
            title: "AgentBar 界面预览（模拟任务）".into(),
            status: TaskStatus::Active,
            updated_at: now(),
            started_at: Some(now() - 240),
            inferred: false,
        }],
        connection: "演示模式".into(),
        scanned_at: now(),
        ..Snapshot::default()
    }
}

fn main() {
    let options = match options() {
        Ok(o) => o,
        Err(e) => {
            report(&e);
            return;
        }
    };
    if options.dump {
        let mut snapshot = if options.demo {
            demo()
        } else {
            LogReader::new(options.home.clone()).scan()
        };
        if !options.demo && !options.logs_only {
            merge_proxy(&options, &mut snapshot);
        }
        println!("{}", serde_json::to_string_pretty(&snapshot).unwrap());
        return;
    }
    #[cfg(not(windows))]
    {
        report("桌面界面请运行 Windows 版 agentbar.exe；本平台可用 --dump --logs-only 检查数据。");
    }
    #[cfg(windows)]
    {
        let state = Arc::new(Mutex::new(Snapshot {
            connection: "正在读取本地 Codex…".into(),
            ..Snapshot::default()
        }));
        let refresh = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, r, q) = (state.clone(), refresh.clone(), stop.clone());
        let (offset, width) = (options.offset, options.width);
        let worker = std::thread::spawn(move || worker(options, s, r, q));
        if let Err(e) = windows::run(state, refresh, offset, width) {
            report(&e);
        }
        stop.store(true, Ordering::Relaxed);
        let _ = worker.join();
    }
}

fn merge_proxy(options: &Options, snapshot: &mut Snapshot) {
    match LocalProxy::connect(&options.codex, &options.home) {
        Ok(mut proxy) => {
            match proxy.usage() {
                Ok(Some(usage)) => snapshot.usage = Some(usage),
                Ok(None) => {}
                Err(e) => snapshot.error = Some(e),
            }
            if let Ok(tasks) = proxy.tasks() {
                merge_tasks(snapshot, tasks);
            }
            snapshot.connection = "本地 Codex".into();
        }
        Err(e) => {
            snapshot.error.get_or_insert(e);
        }
    }
}

fn merge_tasks(snapshot: &mut Snapshot, tasks: Vec<agentbar::model::Task>) {
    for task in tasks {
        if let Some(old) = snapshot.tasks.iter_mut().find(|old| old.id == task.id) {
            *old = task;
        } else {
            snapshot.tasks.push(task);
        }
    }
    snapshot.tasks.sort_by_key(|t| {
        (
            !matches!(
                t.status,
                agentbar::model::TaskStatus::Active | agentbar::model::TaskStatus::Waiting
            ),
            std::cmp::Reverse(t.updated_at),
        )
    });
    snapshot.tasks.truncate(12);
}

#[cfg(windows)]
fn worker(
    options: Options,
    state: Arc<Mutex<Snapshot>>,
    refresh: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) {
    let mut reader = LogReader::new(options.home.clone());
    let mut proxy = None;
    let mut retry_at = 0;
    let mut usage_at = 0;
    let mut live_usage = None;
    while !stop.load(Ordering::Relaxed) {
        let mut snapshot = if options.demo { demo() } else { reader.scan() };
        let forced = refresh.swap(false, Ordering::Relaxed);
        if !options.demo && !options.logs_only {
            if proxy.is_none() && (now() >= retry_at || forced) {
                proxy = LocalProxy::connect(&options.codex, &options.home).ok();
                retry_at = now() + 60;
            }
            if let Some(client) = proxy.as_mut() {
                let mut failed = false;
                if now() - usage_at >= 60 || forced {
                    match client.usage() {
                        Ok(usage) => {
                            live_usage = usage;
                            usage_at = now();
                        }
                        Err(e) => {
                            snapshot.error = Some(e);
                            failed = true;
                        }
                    }
                }
                if !failed {
                    match client.tasks() {
                        Ok(tasks) => merge_tasks(&mut snapshot, tasks),
                        Err(e) => {
                            snapshot.error = Some(e);
                            failed = true;
                        }
                    }
                }
                if failed {
                    proxy = None;
                    retry_at = now() + 60;
                } else {
                    snapshot.connection = "本地 Codex".into();
                }
            }
            if let Some(usage) = &live_usage
                && snapshot
                    .usage
                    .as_ref()
                    .is_none_or(|old| old.observed_at < usage.observed_at)
            {
                let mut usage = usage.clone();
                if proxy.is_none() {
                    usage.source = "本地快照".into();
                }
                snapshot.usage = Some(usage);
            }
        }
        *state.lock().unwrap_or_else(|p| p.into_inner()) = snapshot;
        for _ in 0..30 {
            if stop.load(Ordering::Relaxed) || refresh.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn report(text: &str) {
    #[cfg(windows)]
    windows::message(text);
    #[cfg(not(windows))]
    eprintln!("{text}");
}
