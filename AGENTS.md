# AgentBar 项目约定

- GitHub 仓库为 `https://github.com/sukidesuka/agent-bar`，主分支为 `main`。
- Rust Windows 常驻小程序；Win32 窗口和 GDI 双缓冲绘制使用 `windows-sys`，不使用 WebView/.NET。界面文案使用中文。
- Windows 窗口只在 UI 线程操作。窗口上下文保存在 `RefCell<App>`，通过 `try_borrow_mut` 防止 Win32 同步重入时重复借用；消息循环期间不得持有可变借用。
- BGRA 像素按四字节分组使用 `as_chunks::<4>()` / `as_chunks_mut::<4>()`；CI 跟随 Rust stable，Rust 1.99 的 Clippy 会拒绝固定分组的 `chunks_exact` 写法。
- 并行测试的临时目录必须含原子序号等唯一标识，不能仅依赖 `SystemTime` 时间戳；Windows 时钟粒度可能让同时执行的用例使用同一路径。
- AgentBar 不直接访问 OpenAI、不读取认证文件。仅允许通过已有 Codex 服务的 `app-server proxy` 查询用量和任务；不得自动启动 App Server、创建/恢复会话或响应审批。
- 会话日志增量读取是后备路径。额度必须保留来源与采样时间；缺失/过期不能当成 0%。日志任务状态须标记推测；无活动 180 秒后改为未知。只从明确服务状态显示等待审批。
- 主屏底部任务栏、图标居中且小组件关闭是当前支持的覆盖布局；其他布局保留托盘入口。
- 用户要求任务栏常驻面板使用透明背景。使用 `WS_EX_LAYERED` 与 `UpdateLayeredWindow` 的逐像素 alpha；GDI 文字用灰度抗锯齿和独立覆盖蒙版，避免白底或 ClearType 彩边。透明空白处允许点击穿透，文字和余量条保留点击入口。
- 任务栏面板余量条上下排列：5 小时在上，每周在下；右侧仅显示重置倒计时。不足一天时向上取整到 30 分钟，格式为 `3h 30m`、`3h` 或 `30m`；一天及以上显示完整天数如 `5d`。省略 Codex 文字、品牌绿点、百分比和任务摘要，任务详情通过点击展开。
- 任务栏与详情中的条长及详情百分比均表示剩余额度，通过 `QuotaWindow::remaining_percent()` 将原始 `used_percent` 转换为 `100 - used_percent`；满额为满条，耗尽为空条，缺失或过期数据仍显示未知/待刷新。条色统一固定为 `#247DDE`。
- 本环境可用 Rust stable、MinGW 和 `x86_64-pc-windows-gnu` target。检查：`cargo test`、`cargo clippy --all-targets -- -D warnings`、`cargo clippy --target x86_64-pc-windows-gnu -- -D warnings`。构建：`cargo build --release --target x86_64-pc-windows-gnu`。
- 运行包由 `python3 scripts/package.py` 生成，只包含 Windows 可执行文件、启动脚本和 README，并生成 EXE/ZIP 的 `SHA256SUMS.txt`。CI 用 `--binary <exe路径>` 指定准确产物，避免选择旧的交叉编译结果；打包检查：`python3 -m unittest discover -s scripts -p 'test_*.py'`。
- GitHub 工作流 `.github/workflows/build-release.yml` 在 `main`/PR 上检查并构建 Windows x64，在推送 `v<版本号>` 标签时发布 Release；标签必须匹配 `Cargo.toml`。手动触发时 `tag` 留空仅构建，填写已有标签则构建并发布该版本。CI 使用 MSVC 和静态 CRT，内置 `GITHUB_TOKEN` 仅在发布任务拥有 `contents: write`；已发布版本不覆盖，未完成草稿可续传。
- `.gitattributes` 统一源码和文档为 LF，Windows `.cmd`/`.bat` 启动脚本为 CRLF；避免由全局 `core.autocrlf` 产生跨平台换行变更。打包测试比较原文件字节，不能假设 Windows Python 创建的文本夹具使用 LF。
- 可用 `cargo run -- --dump --logs-only --codex-home <目录>` 验证数据，不启动桌面界面。不要为常规验证启动开发环境或桌面程序。
- 不把真实会话、用量快照或用户路径写入源码、测试夹具和发布包。`dist/` 与 `target/` 是生成产物。
