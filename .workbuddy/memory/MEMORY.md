# iPlayer — 项目长期笔记

## 技术栈与结构

- Tauri v2 + Rust（aarch64-apple-darwin），前端为**无构建步骤**的原生 HTML/CSS/JS（`withGlobalTauri: true`，`frontendDist: ../src`）。
- Rust 模块：`commands.rs`（Tauri 命令）、`ffmpeg.rs`（sidecar 定位 + 进度流解析）、`media.rs`（扫描 / ffprobe 探测 / 播放规划）。播放策略：`direct` / `remux` / `transcode`，产物缓存在 app_cache_dir/playback/。
- `media.rs` 的 `kind_of()` 支持三类：video / audio / image（IMAGE_EXTS：jpg/jpeg/png/gif/webp/bmp/svg/ico/avif，前端直接 `<img>` 渲染，不走 ffprobe）。
- ffmpeg/ffprobe 以 externalBin sidecar 打包，解析顺序：包内 → `src-tauri/binaries/` → PATH，OnceLock 缓存验证结果。
- 图标：`scripts/make-icons.py [外部源图]` 生成全套 icns/ico/png；src/assets/logo.png 用于空状态页。

## 拖动进度条崩溃的修复（关键，勿回退）

- 现象：播放视频时拖动进度条 → 应用「意外退出」（SIGABRT，faulting thread `tokio-rt-worker`）。
- 根因：WebKit 在**视频 seek 时会取消正在进行的 Range 请求**，而 wry 的 `WKURLSchemeTask didReceiveData/didReceiveResponse`（`wry/src/wkwebview/class/url_scheme_handler.rs`）在 task 已被停止后仍调用它 → WebKit 抛 `NSInternalInconsistencyException: This task has already been stopped`。objc2 默认让这个 ObjC 异常**直接 unwind 进 Rust**（objc2 文档原话：几乎必然 abort）。
- 修复（两处配合，缺一不可）：
  1. `Cargo.toml` 加 `objc2` 依赖并启用 **`catch-all`** feature → 每次消息发送包 `@catch`，把 ObjC 异常转成可被 tokio 捕获的 Rust panic（该次 Range 请求失败并自动重试，进程存活）。
  2. `[profile.release]` **不能有 `panic = "abort"`**（objc2 文档明确：panic=abort 时 `exception::catch` 无法工作）→ 显式 `panic = "unwind"`。
  3. `lib.rs::install_panic_hook()` 过滤掉这条已知无害的 panic 消息，避免刷屏，其他 panic 照常输出。
- 复现手法（可复用）：临时在前端 `init()` 注入自测钩子（scanFolder 固定测试目录 → playIndex(0) → 用 PointerEvent 在 `#seek` 上派发 pointerdown/move/up 全程拖动），配合一个临时 `_dbg` Tauri 命令把前端状态打到 stderr；**必须清空 app_cache_dir/playback/ 缓存**才稳定复现（缓存命中时不易触发）。验证必须用 **release** 构建（debug 下 panic=unwind 天然不会崩）。
- 环境限制：本机沙箱禁用 `ps`、`screencapture`，CGWindowList 拿不到窗口（无录屏权限）；`pgrep` 在命令替换里不可靠，判断进程存活要用后台任务状态或单独一次 pgrep 调用。

## 本机环境（重要）

- **Rust 通过清华镜像安装**：直连 static.rust-lang.org 约 6KB/s（会卡死），用 `RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup`；cargo crates 镜像为 rsproxy，配置写在 `~/.cargo/config.toml`。
- **`~/.zshrc` 末尾已追加 `[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"`** —— rustup 装时用了 `--no-modify-path`，不加载它新终端会报 `cargo metadata ... No such file or directory`，导致 `npm run build` 失败。
- ffmpeg sidecar 目前是 Homebrew 动态链接版本（仅本机可运行）；要分发需 `npm run fetch:sidecar` 换自包含构建。
- DMG 打包在本机不稳定（bundle_dmg.sh 挂起），`tauri.conf.json` 的 bundle.targets 只保留 `["app"]`。

## UI 规范（用户明确要求）

- 主题色为**黑/白单色**，不使用蓝色：light accent `#101216`，dark accent `#f4f6f9`（深色下用反色保证可见）。
- 控制条：无顶部分隔线；播放组（上一个/播放/下一个/停止）整行居中；播放键无底色无边框，仅三角图标，颜色 = `var(--text)`。
- 倍速 0.5x–3.0x，步进 0.5（含 `[` `]` 快捷键）。
- 侧栏：按钮顺序为「打开文件」在前、「打开文件夹」在后；无底部文件计数行；无「尚未选择文件夹」占位文字。

## 验证手法

- headless Chrome + mock Tauri API 渲染 UI 截图（`--headless=new --enable-unsafe-swiftshader --virtual-time-budget`）。
- headless 下 `<video>` 无法截到画面帧（canvas drawImage 同样无效），README 截图需用 ffmpeg 预渲染帧图后以 `<img>` 注入 stage。
- 无头渲染 modal 时需临时禁用 `backdrop-filter` / `animation`，否则面板不显示（仅测试环境问题）。
