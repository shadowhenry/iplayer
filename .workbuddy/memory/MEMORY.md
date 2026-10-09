# iPlayer — 项目长期笔记

## 技术栈
纯 Rust + GPUI（`gpui-kit 0.7.1` = crates.io `gpui-pre 0.3.8`，不依赖 Zed git），edition 2024。
旧 Tauri / HTML+CSS+JS / MSE 转码链已全删（README 不许提 WebView / 浏览器）。
依赖固定版本：`image 0.25.10`（必须与 gpui-pre 同版本，否则 `image::Frame` 不同型）、`resvg 0.46`
（版本+特性要对齐 gpui-pre 自带那份，否则两套 usvg/tiny-skia）、`cpal 0.16`、`rfd 0.15`、
`objc2 0.6` + `objc2-app-kit 0.3.2`、`raw-window-handle 0.6`。
文件：`main.rs` / `app.rs`（唯一 UI）/ `theme.rs` / `ffmpeg.rs` / `media.rs` / `subtitle.rs` /
`export.rs` / `native.rs` / `icons.rs` / `viz.rs` / `player/*`。

## GPUI 坑（勿回退）
- 回调里绝不能 panic（objc 边界不能 unwind → abort）；回调里也不许同步弹 rfd 面板（`runModal`
  会重入挂在主队列上的帧源）。`AsyncFileDialog` 在 `NSApp.isRunning()==false` 时静默退回同步
  `runModal` → 弹面板前先 `native::async_sheet_available(window)` 自查 + `catch_unwind` 兜底 +
  必须 `.set_parent()`。导入 / 导出流程 = 记账 → 下一帧 `tick()` 里弹。panic 日志
  `~/Library/Logs/iPlayer/crash-*.log`。
- `Window::request_animation_frame()` 只在渲染路径成立（内部 `current_view().last().unwrap()`）；
  tick 里用它续帧，事件回调一律 `cx.notify()`（有扫源码守卫测试）。
- render 辅助方法写 `-> impl IntoElement + use<>`；别用 `bool::then(|| self.render_x(cx))`。
- `WindowOptions::default()` 帧循环不转 → 要 `window_bounds: Some(centered(...))` + `cx.activate(true)`。
- `SliderState` 的 min/max 只能构造时链式设；0–1 归一化时 `step` 必须一起改小（0.0001），
  否则指针被吸附到 0/1；`set_value` 要一个它并不用的 `&mut Window`。
- `RenderImage` 是 BGRA、不是 Clone；`size(0).width.0` 是 i32。
- `Window::window_handle()` 遮住 `HasWindowHandle::window_handle()` → 取原生句柄写
  `<Window as HasWindowHandle>::window_handle(window)`。
- 标题栏：`appears_transparent` + `app_owns_titlebar_drag` + `pl(px(78.))` 让开红绿灯；
  无头测试不能点标题栏按钮（整条挂着 `start_window_move()`，测试平台 `unimplemented!()`）。
- `toggleFullScreen:` 前要补 `NSWindowCollectionBehaviorFullScreenPrimary`；`fs_settle` 挡 900ms。
- 字形覆盖不可靠 → 按钮一律中文小字 chip / 内置 SVG。
- 浮层与舞台在一条冒泡链上（`Window::hit_test` 反向遍历，同点的 hitbox 全收）→ 绝对定位浮层必须
  `.id()` + `cx.listener(|_,_,_,cx| cx.stop_propagation())`。
- 指针回归可无头测：`TestAppContext` + `add_window_view` + `.debug_selector()` + `simulate_mouse_*`；
  按下前先来一次无按键 `simulate_mouse_move`；拖动要连发几次 move；`debug_bounds` 只认
  `.debug_selector`（`.id` 不算，要几何断言两个都写），`Pixels` 要 `f32::from(p)`；
  改 View 状态用 `Entity::update_in(&mut VisualTestContext, |v, window, cx| …)`。
  `add_window_view` 固定开 1920×1080 → 量"默认窗宽挤不挤"要用 `cx.open_window(...)` +
  `VisualTestContext::from_window(*h.deref(), cx)`（需 `use std::ops::Deref`）。
- 挑图标：临时测试里用 resvg 按真实像素（15/19px）放大渲染成 PNG 用 Read 挑。
- `InputState::set_value` 不发 Change 事件，程序化改完要自己刷列表。

## 播放内核（勿回退）
- seek / 换角度重开解码时绝不把 `current` 清成 None（只把 `current_pts` 归 -1）；
  冻结条件抽成 `freeze_current(playing, has_current, awaiting)`，`App::tick` 的 busy 要算上 `awaiting()`。
- `ended()` 只认通道 `Closed`（`Empty` 只是这一帧还没解出来）且要求 `current_pts >= 0`；
  `empty_spawn` 直接判播完。
- 拖动只 `Player::scrub()`（只重开视频），松手才 `seek()`；`SCRUB_INTERVAL` 90ms 节流。
- A/V 同步：有音轨以音频钟为准，无音频回退墙钟；取 `pts <= pos + 0.004` 的最新帧，早于 0.5s 丢；
  背压用有界 channel(3) + `try_send`。
- 倍速走 `atempo` 链（0.5–2.0 步进）；切换文件先 `self.player = None` 彻底交出。
- 画面朝向 `Orientation{rot,flip_h,flip_v}` 是屏幕空间语义；视频（滤镜）/图片（CPU 搬像素）两套实现，
  由 `--selftest` 逐像素核对钉住，改一处两边一起改。
- 动图（GIF / 动画 WebP）先过 `fps=` 归一化；图片解码三条路 SVG(resvg) / image crate / ffmpeg 兜底
  （tiny-skia 给预乘 RGBA，要先除回 alpha）。
- sidecar 名一律经 `ffmpeg::exe_name()` 拼 `EXE_SUFFIX`（Windows `Path::is_file()` 不补扩展名）。
- `[profile.release] strip = "debuginfo"`（别写 true）+ `panic = "unwind"`。

## UI 规范（用户明确要求）
- 画面适配一律 `ObjectFit::Contain`（绝不用 Fill）；静态图片按原尺寸、不放大；黑白单色（不用蓝色）；
  控制条无顶部分隔线；播放组始终整行居中（左右各一个 1fr 槽配对）。
  舞台容器本身已贴窗口左右边缘（逐像素核对过）；用户看到的左右黑边是画面比例留下的，
  2026-10-09 用户确认**不改**。真要贴边只有 Cover 一条路（裁掉超出方向），别再动。
- 按钮排布：标题栏右上角 = 深浅色 · 置顶 · 最小化 · 最大化 · 关闭X；
  控制条右端 = 静音 · 音量 · 倍速 · 循环 · 切换方向 · 字幕 · 工具箱。
- 图标分工：循环 = 环形箭头 `loop`/`loop1`（单曲带中心 "1"）；画面角度 = 方框+内部弧箭头 `rotate`；
  字幕 = `captions`（圆角框 + 两条短横线）。旧 `repeat`/`repeat1` 已删（有测试钉住）。
- 角度面板 / 导出面板 / 字幕面板都在舞台右下角（`right/bottom 12px`），三者互斥不叠。
  **停靠 10 秒没人碰就自动收起**（用户要求，别一直挂着）：`panel_seen: Option<Instant>` 记
  "最近活动时刻"，`PANEL_TTL = 10s`；`note_activity()` 挂在根节点的 `on_mouse_move` +
  `capture_any_mouse_down` 和键盘上（GPUI 没被 occlude 的祖先都在 hover 链上，根节点收得到全窗口动作）；
  判超时在 `tick` 里（面板开着时每帧 `request_animation_frame` 续帧，否则帧循环一停就没人看表）；
  导出任务 / 文件面板挂着时不算"没人用"。Esc 走 `close_panels()`。
- 侧栏行文本 `.flex_1().min_w(px(0.)).truncate()`；倍速 0.5–3.0 步进 0.5；「清空列表」只丢列表不停播放。
- 点播放区域 = 播放 / 暂停（浮层自己 stop_propagation）。
- 音频舞台曲名浮层（`now_playing`）：音符 + 一行名字，横向居中，纵向钉在 30% 高处（频谱柱最高 41%）；
  名字取 `song_title(tag, path)`（容器标签 title 优先，退回文件名；`MediaInfo.title`）。
- 字幕浮层：底部居中（下边距 6%），多行逐行渲染，深色半透明底 + 白字（任何画面上都可读）；
  打开文件时自动找同名 sidecar（`.srt`/`.ass`/`.vtt`）。

## 打包 / 验证
- `scripts/bundle-macos.sh`（`--dmg` / `--zip` / `--run` / `--install`）；`bundle-windows.ps1`（未实跑）/
  `bundle-linux.sh`（`bash -n` 过）。**`cp` 覆盖 .app 内二进制会破坏签名 → macOS 静默 SIGKILL**，
  必须走 `--install`。
- 回归：`cargo test` + `--selftest`（解码 / seek / 倍速 / 暂停冻结 / 模拟拖动黑屏采样为 0 /
  导出任务真跑校验 magic）。写完钉子要临时回退旧行为确认变红。
- GUI 只能靠"进程存活 + 临时 trace 文件"；下 GitHub 走 `socks5h://127.0.0.1:7897`；
  `binaries/` 里下载的 ffmpeg 没有执行位，要 `chmod 755`。
- 拖放：`on_drop` 的参数类型必须与 active_drag 的值类型一致（外部文件是 `&ExternalPaths`）。
