# AgentBar

Rust 编写的 Windows 11 任务栏小面板，查看 Codex 剩余额度和本地任务活动。

## 运行

使用 `dist/agentbar.exe`，或双击同目录的 `start-agentbar.cmd`。

- 任务栏小面板背景透明，直接透出系统任务栏；文字颜色跟随系统明暗主题。
- 余量条分上下两行：5 小时在上，每周在下，条长表示剩余额度（例如已用 23% 时显示 77% 长度），额度充足时满条、耗尽时为空。任务栏和详情统一使用固定蓝色 `#247DDE`，不随额度变化。
- 每条右侧显示重置倒计时：不足一天时按 30 分钟一档向上取整，显示 `3h 30m`、`3h` 或 `30m`；一天及以上只显示完整天数，例如 `5d`。面板不显示 Codex 标识、绿点和百分比文字。
- 左键点击余量条或倒计时：展开详情；透明空白处的点击会穿透。
- 点击详情中的任务：打开对应 Codex 对话。
- 右键面板或托盘图标：刷新、打开数据目录、退出。
- 详情失去焦点或按 Esc：收起。
- 单实例运行；不自动安装、不设置开机启动。

当前适配主屏、底部任务栏、图标居中、系统小组件关闭的布局。任务栏隐藏或检测到全屏应用时收起面板；左对齐、小组件启用或左侧空间不足时保留托盘入口。Windows 更新可能影响覆盖效果，需要在实际桌面上验证。

```powershell
.\agentbar.exe --codex-home "C:\Users\你的用户名\.codex"
.\agentbar.exe --logs-only
.\agentbar.exe --offset 16 --width 340
.\agentbar.exe --demo
```

`--demo` 始终明确标注“演示”，不会把模拟任务当作真实数据。默认读取 `CODEX_HOME`；未设置时读取当前 Windows 用户目录的 `.codex`。可用 `--codex-home` 覆盖。

## 数据来源

AgentBar **不直接请求 OpenAI，不读取认证文件，不发送模型对话**。

1. 默认尝试运行 PATH 中 **Codex CLI** 的 `codex app-server proxy`，仅连接已有服务。可用 `--codex-command <CLI完整路径>` 指定；不要指定 Codex 桌面启动器。
2. 通过标准初始化后，只调用 `account/rateLimits/read` 和 `thread/list`。不会启动新 App Server、恢复会话、创建任务或响应审批。Codex 自己可能联网获取最新额度。
3. 连接不可用时读取 `sessions/YYYY/MM/DD/*.jsonl`。首次读取最近 48 个会话的有限头部和尾部，随后增量读取；每 3 秒更新，每 30 秒重新发现文件。代理失败每 60 秒重试；可用 `--logs-only` 完全跳过代理。
4. 用量按日志时间选择最新的 Codex 额度快照，不把其他模型的额度混入。缺失数据显示 `--`；周期已过期显示“待刷新”，不会伪造为 0%。
5. 本地日志任务状态是推测：有开始、没有对应结束且最近有活动时显示执行中；3 分钟没有新事件改为未知。等待确认只采用服务明确返回的状态。任务摘要来自最近用户消息或工作目录，不保证与 Codex 侧栏标题一致。

详情底部显示用量来源和快照年龄。纯日志模式无法保证账号额度实时更新，也无法完整获知所有后台任务及审批状态。

协议参考：[OpenAI App Server 官方文档](https://learn.chatgpt.com/docs/app-server)。日志结构属于版本相关的本地格式，解析器对不认识的记录会跳过。

## 构建和检查

Windows 原生 Rust 工具链：

```powershell
cargo test
cargo build --release
```

WSL/Linux 交叉编译（需要 MinGW 和 `x86_64-pc-windows-gnu` Rust target）：

```bash
cargo test
cargo clippy --all-targets -- -D warnings
cargo clippy --target x86_64-pc-windows-gnu -- -D warnings
cargo build --release --target x86_64-pc-windows-gnu
```

生成 `dist/` 运行包（Python 3.11+）：`python3 scripts/package.py`，Windows 上也可使用 `python scripts/package.py`。

界面使用 `windows-sys` 调用 Win32/GDI，双缓冲绘制；不依赖 WebView 或 .NET。源码中不包含用户用量、会话和凭据。

只检查一次真实数据，不启动桌面界面：

```bash
cargo run -- --dump --logs-only --codex-home /mnt/c/Users/你的用户名/.codex
```

Windows GUI 子系统版本的 `--dump` 建议重定向输出：

```powershell
$p = Start-Process .\agentbar.exe -ArgumentList '--dump','--logs-only' -Wait -PassThru -RedirectStandardOutput .\snapshot.json
Get-Content .\snapshot.json
```

快照可能包含任务摘要和本地路径，分享前请自行检查。
