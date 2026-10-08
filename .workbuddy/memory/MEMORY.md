# iPlayer — 项目长期笔记

## 技术栈（2026-10-08 起：纯 Rust + GPUI）

- Tauri / HTML+CSS+JS / MSE 转码流水线**已全部退役删除**。现在纯 Rust + `gpui-kit 0.7.1`
  （= crates.io 的 `gpui-pre 0.3.8`，**不依赖 Zed git**），edition 2024。release 二进制 15 MB。
- 依赖只留：`gpui-kit`、`image 0.25.10`（**版本必须匹配 gpui-pre，否则 `image::Frame` 不是同一类型**）、
  `smallvec`、`smol`、`serde/serde_json`、`cpal 0.16`、`rfd 0.15`，外加
  `resvg 0.46`（**版本和特性必须对齐 gpui-pre 自己那份，否则编译出两套 usvg/tiny-skia**）、
  `raw-window-handle 0.6`、`objc2 0.6` + `objc2-app-kit 0.3.2`（只用 NSWindow/NSView/NSResponder 三个特性）。
- 文件：`main.rs`（入口 / `--selftest` / `--info`）、`app.rs`（唯一 UI）、`theme.rs`（黑白调色板）、
  `ffmpeg.rs`、`media.rs`（扫描 + ffprobe + GIF 块解析）、`export.rs`（导出工具箱）、`native.rs`（全屏 / 置顶）、
  `player/{mod,timeline,video,audio,svg}.rs`。
- 打包：`scripts/bundle-macos.sh`（`--dmg` / `--zip` / `--run`），只用 codesign + hdiutil，产物在 `dist/`（已 gitignore）。
- `use gpui_kit::*` = GPUI 本体；组件在 `gpui_kit::component::*`。

## GPUI 坑（勿回退）

- **`cx.notify()` 唤不醒平台帧源**：视频必须 `window.request_animation_frame()`。
- **`WindowOptions::default()` 帧循环不转**：必须 `window_bounds: Some(WindowBounds::centered(size(px,px), cx))`
  + `cx.activate(true)`。
- **render 辅助方法写 `-> impl IntoElement + use<>`**：Rust 2024 的 `impl Trait` 会捕获
  `&mut self`/`&mut Context`，几块内容就没法各自构造再拼。**也别用 `bool::then(|| self.render_x(cx))`**
  （闭包把借用攥到根节点构造完），用 `if … { Some(…) } else { None }`。
- `SliderState` 的 `min`/`max` 只能在构造时链式设，**没有 setter** → 进度条按 0–1 归一化，时长变化不重建实体。
- `SliderState::set_value(v, window, cx)` 要一个**它并不用**的 `&mut Window`，得从监听器 / `tick(window,cx)`
  一路传进来。`Entity::update_in` 要 `VisualContext`，`Context<T>` 不满足 →
  用 `Entity::update(cx, |st, cx| st.set_value(v, window, cx))`。
- `cx.listener(|this, ev, window, cx| …)` 第三参就是 window。
- 没有 `cx.window_handle_placeholder()`，也没有 `gpui_kit::actions::ToggleSidebar` —— 侧栏开关自己写 `on_click`。
- `RenderImage` 是 **BGRA**（gpui `assets.rs` 文档原话）。ffmpeg 输出 `bgra` 正好对上；`image` crate 给 RGBA，
  要手动对调 R/B（`player::tests` 用纯红 PNG 钉死）。
- 标题栏：`appears_transparent = true` + `app_owns_titlebar_drag = true` + 自己 `pl(px(78.))` 让开红绿灯，
  `on_mouse_down(Left, |_, w, _| w.start_window_move())`。
- 字形覆盖不可靠 → 按钮一律用**中文小字 chip**（静音 / 循环 / 信息 / 倍速 / 全屏 / 置顶 / 导出 / 适应）。
- **`Window::window_handle()` 遮住了 `HasWindowHandle::window_handle()`**（前者返回 `AnyWindowHandle`）：
  要拿原生句柄必须写 `<Window as HasWindowHandle>::window_handle(window)`。
- **`toggleFullScreen:` 得先补 `NSWindowCollectionBehaviorFullScreenPrimary`**，否则 AppKit 直接空操作。
  全屏切换有动画，切完前别读 `styleMask`，否则 `app` 侧状态会来回打架（用 `fs_settle` 挡 900ms）。

## 播放内核（勿回退）

- **`ended()` 不能只看"通道空了"**：`smol::channel` 的 `Empty` 只是"这一刻还没解出来"（解码跟不上、刚 seek
  完都会这样），**只有 `Closed` 才算解码线程收工**。还必须要求 `current_pts >= 0`：起播瞬间是 -1，
  不挡住会立刻被判"播完" → `pause()` → 帧循环死掉，表象是"界面卡在起播"。
- **暂停时也要允许挑一帧**：暂停中 seek 后手上是空的，`frame()` 直接返回缓存会让界面一直空着；只有已有画面时才冻结。
- A/V 同步：有音轨时音频钟为准（cpal 回调数已消费帧），无音频回退墙钟。
- 选帧：取 `pts <= pos + TOL(0.004)` 的最新帧，早于 `LATE=0.5s` 的丢弃。
- 背压：解码线程 ↔ 呈现用有界 `smol::channel`(3) + `try_send` 轮询（**别阻塞发送**，取消会不及时）。
- Seek：kill 子进程，`-ss <base>` 放 `-i` **之前**重开；输出时间戳归零。拖进度条按 90ms 节流。
- 倍速：音频走 `atempo` 链（拆成 0.5–2.0 步进）；音频钟按倍速缩放。cpal 的 `Stream` 在 macOS 不是 `Send` → 专用线程持有。
- 切换文件 = 先 `self.player = None` 彻底交出（子进程 / 线程 / 音频流），再开新的。
- **动图是变帧率**：GIF / 动画 WebP 必须先过 `fps=` 滤镜归一化，否则「帧序号 / fps」推算的 pts 会飘。
  判定动画靠 `media::gif_info`（自己走 GIF 块，数图像描述符个数，微秒级）。
- **图片解码三条路**：SVG 走 `player/svg.rs`（resvg）；常规位图走 `image` crate；
  它认不出来的（AVIF / EXR / PSD）退到 ffmpeg 解一帧。tiny-skia 给的是**预乘** RGBA，要先除回 alpha。
- `[profile.release] panic = "unwind"`（不能 abort）。sidecar 查找：exe 同级 → `Contents/Resources` →
  仓库根 `binaries/` → PATH，每个候选真跑 `-version`，结果进 `OnceLock`。
- **sidecar 文件名一律经 `ffmpeg::exe_name()` 拼 `std::env::consts::EXE_SUFFIX`**：Windows 上
  `Path::is_file()` 不补扩展名，查 `ffmpeg` 永远找不到 `ffmpeg.exe`，会静默掉回 PATH。
  macOS 的 `EXE_SUFFIX` 是空串，行为不变。有测试 `sidecar_names_cover_both_spellings` 钉住。

- **滑块用 0–1 归一化，但 `step` 必须一起改小**。`SliderState` 的 `min`/`max` 只能在构造时
  链式设定（没有 setter），所以进度条按 0–1 归一化；而 **`step` 默认是 `1.0`**，指针交互走
  `(value / step).round() * step` → 归一化后每次都被吸附到 **0 或 1**，表现就是「进度条拖不动/
  一拖就跳到底」。必须显式 `.step(0.0001)`。音量条是 0–100、步长 1 本来就合适，所以它没事。
  有两条无头指针测试钉住：`seek_slider_can_be_dragged` + 对照 `volume_slider_can_be_dragged`。
- **GPUI 指针回归可以无头测**：`[dev-dependencies] gpui = { package = "gpui-pre", features = ["test-support"] }`
  + `TestAppContext` / `add_window_view` / `cx.debug_bounds("selector")` / `simulate_mouse_*`。
  要点：① 元素要加 `.debug_selector(|| "…".into())`（非 debug 构建是空操作）；
  ② 按下前必须先来一次 **无按键的 `simulate_mouse_move`**，否则 `hitbox.is_hovered()` 为假，
  拖拽不会启动；③ GPUI 要先越过 `DRAG_THRESHOLD` 才建立拖拽，**建立那一次 move 不派发
  drag_move**，所以拖动要连发几次 move；④ 别在 mouse_down 和 move 之间调 `cx.update`
  （会多画一帧，pending_mouse_down 丢掉，拖拽就起不来了）。
  **单击用 `simulate_click(center, Modifiers::default())` 就够**，不必自己 down+up。
- **测试里要改 View 状态，用 `Entity::update_in(&mut VisualTestContext, |view, window, cx| …)`**：
  `VisualTestContext` 实现了 `VisualContext`（`test_context.rs:1176`），一次就能拿到
  `(&mut App, &mut Window, &mut Context<App>)` —— 改字段、写 `InputState::set_value`、调
  `refresh_view` 全在一个闭包里。反面：`TestAppContext::update` 只给 `&mut App`，而 **`App` 没有
  实现 `AppContext`**（只有 `Context<'_,T>` / `VisualTestContext` / `AsyncApp` … 实现），
  在那儿调不了 `update_entity`；`Entity::update` 也不行，它只给 `&mut C`，拿不到 `Context<App>`。
- **`InputState::set_value` 刻意不发 `InputEvent::Change`**（`emit_events` 临时置假），
  所以程序化改完搜索词必须自己再刷一遍列表，别指望订阅回调。
- **几何回归**：`cx.debug_bounds("…")` 给的 `Bounds<Pixels>` 里 **`Pixels` 字段是私有的**，
  `.0` 取不出来，要 `f32::from(p)`。像「居中/贴右」这类布局断言，量 `debug_bounds` 比量截图靠谱。

## 多平台打包

- `scripts/bundle-macos.sh`（**实机验证过**）：`.app` + `--dmg` / `--zip` / `--run`，只用 codesign/hdiutil。
- `scripts/bundle-windows.ps1`（仅人工复核，本机无 PowerShell）：`dist\iPlayer-win64\` + `-Zip`，
  sidecar 找 `binaries\<tool>-<triple>.exe` 或 `binaries\<tool>.exe`。
- `scripts/bundle-linux.sh`（`bash -n` 过，未在 Linux 实跑）：便携目录 + `.desktop` + `install.sh`
  （用户级应用菜单，不需要 root）+ `--tar`。

## UI 规范（用户明确要求）

- **黑白单色，不用蓝色**：light accent `#101216`，dark accent `#f4f6f9`。
- 控制条无顶部分隔线；播放组（« / ▶ / » / ■）**两态**（照抄原版 CSS `styles.css` 1118–1151）：
  **侧栏展开**时播放组紧挨音量组、两个一起靠右（右组 `auto` 宽 + 左格 `flex_1` 吃掉余量）；
  **侧栏收起**时回到**整行**正中（右组再套一个 `flex_1`，与左格配对）。
  行间距 8px / 14px。中间格**绝不能**用 `flex_1 + justify_center` —— 那只会停在
  「时间 ↔ 右组」那段空隙的中间，右组比时间宽得多，视觉中心会被推到音量条旁边。
  播放键无底色无边框。有 `mod control_bar_tests` 量几何钉住两态。
- 倍速 0.5–3.0x 步进 0.5（`[` / `]`）。侧栏：「打开文件夹」前、「打开文件」后；无底部计数行。
- 侧栏头部按钮从右到左：`清空列表`（close 图标，列表空时压暗）、`重新扫描`。「清空列表」只丢列表、
  **不停播放**（照原版）；连带 `folder` 一起忘掉，否则「重新扫描」会把列表捡回来。
  搜索框有输入时右侧浮现 `×` 清关键字。
- **README / 文案里不许出现 WebView、浏览器字样**（用户明确要求），旧 Tauri 那套一个字不提。
- 画面适应：适应 / 裁切 / 拉伸 / 原始（`FitMode` → `ObjectFit`）。导出面板是舞台右下角的浮层，
  有任务在跑时强制显示；导出进度靠 `tick()` 里持续 `request_animation_frame()` 才转得起来。

## 验证手法

- 回归必须过：`cargo test`（29 个）+ `--selftest`（解码 / seek / 2x 倍速 / 暂停冻结 / 暂停中 seek 出帧 /
  三个导出任务真跑一遍并校验产物 magic）。`--info <file>` 会打印媒体规格，图片还会报真实解码尺寸。
- 沙箱禁用 `ps`、`screencapture`。**GUI 只能靠"进程存活 + 临时 trace 文件"**：在 `tick()` 里把
  `position / is_playing` 追加写 `/tmp/*.log`，跑几秒读回 —— 这次就是靠它抓到"帧循环只转了 3 帧"。
  **用完必须清干净**（`grep -rn "TEMP-TRACE" src/` 应为空）。
- 下 GitHub 必须走 Clash 代理 `socks5h://127.0.0.1:7897`；`binaries/` 里的静态 ffmpeg 下载后**没有执行位**，要 `chmod 755`。

## 拖放回调（勿回退）
- `on_drop` 回调参数类型**必须与 active_drag 的值类型一致**（按 TypeId 匹配）：
  外部文件拖放是 `&ExternalPaths`，写成 `&Arc<ExternalPaths>` 监听器永远不触发。
- 侧栏搜索框透明化：gpui-component 的 `Input::new(..).appearance(false)` 去白底去边框。
