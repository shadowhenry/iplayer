# iPlayer — 项目长期笔记

纯 Rust + GPUI 媒体播放器（macOS），`gpui-kit 0.7.1` = crates.io `gpui-pre 0.3.8`，edition 2024。
旧 Tauri / WebView 链已全删。固定依赖：`image 0.25.10`、`resvg 0.46`、`cpal 0.16`、`rfd 0.15`、
`objc2 0.6` + `objc2-app-kit 0.3.2`、`raw-window-handle 0.6`。文件：`main.rs` / `app.rs`（唯一 UI）/
`theme.rs` / `ffmpeg.rs` / `media.rs` / `subtitle.rs` / `export.rs` / `native.rs` / `player/*`。

## GPUI 坑（勿回退）
- 回调里绝不 panic（objc 边界 abort）；不同步弹 rfd 面板（`runModal` 重入帧源）。导入/导出 =
  记账 → 下一帧 `tick()` 弹。弹面板前 `native::async_sheet_available(window)` 自查 +
  `catch_unwind` + `.set_parent()`。
- tick 里续帧用 `window.request_animation_frame()`，事件回调一律 `cx.notify()`。
- 系统「打开文件」走 `application.on_open_urls(native::open_urls)`，绝不自挂 `NSAppleEventManager`
  （finishLaunching 会被覆盖）。native 入队后 `smol::channel` 拍一下，`watch_open_docs` 唤醒
  `pump_open_docs`（非轮询）。
- 浮层绝对定位必须 `.id()` + `stop_propagation()`；标题栏 `appears_transparent` +
  `app_owns_titlebar_drag` + `pl(px(78.))` 让红绿灯。
- 取原生句柄：`<Window as HasWindowHandle>::window_handle(window)`。
- 倍速 0.5–2.0 走 `atempo`；切换文件先 `self.player = None`。A/V 同步有音轨用音频钟、无则墙钟。
- 最大化 **只能 `window.zoom_window()`**（GPUI API，丢前台执行器）；事件回调里同步调 AppKit
  `zoom:` 会布局卡旧尺寸。无头测试平台 `zoom_window` 是 `unimplemented!()`，只钉源码扫描。
- 改 GPUI 无头测试：宽度用 `cx.open_window(...)` + `VisualTestContext::from_window`，
  `Pixels` 用 `f32::from(p)` 换算；`debug_bounds` 只认 `.debug_selector`。

## 显存 / 纹理（「播久了自动崩」根因，勿回退）
GPUI 精灵图集只增不减，每解一帧造新 `RenderImage` → 每帧留一块大纹理（720p 实测 +120MB/s，
几分钟被系统 SIGKILL 且无 panic 日志）。正解：`App.shown_frame` 记上一帧纹理，
`render_stage` 里 `needs_retire` 为真就 `window.drop_image(旧)`；判据只看 id，画面消失也要撤。
钉子 `frame_retire_tests`。查崩用 `footprint -p <pid>` 看 `phys_footprint`。

## 外部进程（ffmpeg/ffprobe）EAGAIN 重试（2026-11-04）
- 现象：系统进程数逼近上限时 `posix_spawn`/`fork` 返回 **EAGAIN (os error 35)**，iPlayer 原把
  它当硬失败弹「无法运行 ffprobe/ffmpeg」。这是**瞬时**资源紧张，稍等重试即可。
- 正解：`ffmpeg.rs` 加 `run_output` / `run_status` / `spawn_child`（均取 `&mut Command`，
  只对 `raw_os_error()==Some(35)` 重试，最多 6 次、退避 30→500ms；其它错误立即上抛不重试）。
  所有 ffprobe/ffmpeg 执行点都改走这三个助手：媒体探测 `media.rs::probe`、解码单帧
  `player/mod.rs`、`audio.rs`/`video.rs` 解码线程 `spawn_child`、`export.rs` 转码、以及
  工具发现 `works()`/版本探测。**注意 `Command::output/status/spawn` 取 `&mut self` 不消费**，
  所以重试直接复用同一 `&mut Command`，无需 clone。
- 测试 `retry_tests` 钉：成功路径正常、非 EAGAIN 错误 200ms 内立即返回（不卡退避）。

## UI 规范（用户明确要求，勿回退）
- 画面 `ObjectFit::Contain`（绝不用 Fill）；控制条无顶部分隔线；播放组整行居中。
  舞台已贴窗左右边，左右黑边是比例留的（2026-10-09 确认不改）。
- 标题栏右上角 = 深浅色 · 置顶（最小化/最大化/关闭三颗已删，交 macOS 红绿灯）；
  控制条右端 = 静音·音量·倍速·循环·方向·字幕·工具箱·**最大化**（图标随
  `native::is_window_zoomed` 切 maximize/restore）。
- **紧凑模式（2026-10-31）**：整窗宽 `f32::from(window.window_bounds().get_bounds().size.width)
  < 760.0` 时，右排次组用 `.when(!narrow, …)` 收起，窄窗只留「时间+播放组+最大化」。
  钉子 `narrow_window_hides_secondary_controls` / `wide_window_shows_secondary_controls`。
- **双击视频弹出主界面（2026-10-31）**：Dock 最小化时双击音视频应 deminiaturize 出来播。
  `open_file` 置 `raise_on_open`（并复位 `resume_on_restore`）；`tick` 在 `pump_open_docs` 后
  若 `raise_on_open && native::is_minimized` 就 `native::show_window`（deminiaturize）。
- 面板（角度/导出/字幕）在舞台右下角、互斥、停靠 10s 自动收（`PANEL_TTL=10s`，`tick` 判超时）。
  字幕浮层底部居中；音频曲名浮层 30% 高。倍速 0.5–3.0 步进 0.5。

## 文件关联 / 默认播放器（静默、不弹框）
- `media.rs`：每扩展名申 `com.iplayer.<ext>` UTI + `UTImportedTypeDeclarations`；
  `CFBundleDocumentTypes` 音视频 `LSHandlerRank=Default`、图片 `Alternate`（不抢 Preview）。
  `std_uti(ext)` 必须对齐系统真实 UTI（见 `std_uti_matches_system_utis` 测试），否则
  `LSSetDefaultRoleHandlerForContentType` 吃 `-50` 跳过。关键映射：mp4→`public.mpeg-4`、
  mkv→`com.blurayplayersoftware.matroska`、wma→`com.microsoft.windows-media-wma`、
  m4v→`com.apple.m4v-video`、avi→`public.avi`、flv→`com.adobe.flash.video`、
  wmv→`com.microsoft.windows-media-wmv`、ts→`public.mpeg-2-transport-stream`、
  m2ts/mts→`public.avchd-mpeg-2-transport-stream`、rmvb→`com.real.realmedia-vbr`、opus→`org.xiph.ogg-audio`。
- **静默设默认（务必记牢）**：绝不调会弹确认框的 `LSSetDefaultRoleHandlerForContentType`
  （覆盖 TV/XLPlayer 时每格式弹一次，几十次点击）。正解：直接改写
  `~/Library/Preferences/com.apple.LaunchServices/com.apple.launchservices.secure.plist` 的
  `LSHandlers`，每个 uti 写 `LSHandlerRoleAll = com.iplayer.app`，写完 **双 kill**
  `killall -u $USER cfprefsd` **和 `lsd`**（只杀 cfprefsd 不够，lsd 不重载就仍读旧库）。
- `check_default_player` 首 tick 静默自动设一次（幂等），判据用具体 UTI `public.mpeg-4`。

## 最小化暂停（2026-10-28）
窗口最小化即 `player.pause()` + 置 `resume_on_restore`，还原后续播；`busy` 计入 `|| min`
保证帧循环不停。`stop()` 清标记。纯手动暂停不被还原误续。

## 打包 / 验证
- `scripts/bundle-macos.sh --install`（绝不用 `cp` 覆盖 .app 内二进制，会破坏签名 → 静默 SIGKILL）。
- 回归：`cargo test` + `--selftest`（解码/seek/倍速/暂停冻结/导出真跑校验）。写完钉子先临时
  回退确认变红。`binaries/` 里 ffmpeg 无执行位要 `chmod 755`；下 GitHub 走 `socks5h://127.0.0.1:7897`。
