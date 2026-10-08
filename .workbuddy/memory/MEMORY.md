# iPlayer — 项目长期笔记

## 技术栈（2026-10-08 起：纯 Rust + GPUI）

- Tauri / HTML+CSS+JS / MSE 转码流水线**已全部退役删除**。现在纯 Rust + `gpui-kit 0.7.1`
  （= crates.io 的 `gpui-pre 0.3.8`，**不依赖 Zed git**），edition 2024。release 二进制 22 MB（含符号表）。
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
- **无头测试里不能 `simulate_click` 标题栏上的按钮**：整条标题栏挂着 `start_window_move()`，
  测试平台是 `unimplemented!()`，鼠标事件冒泡上去直接 panic。工具栏按钮只断言
  `debug_bounds` 存在，动作在测试里直接调 App 的方法。
- `RenderImage::size(0).width.0` 是 **i32**；`RenderImage` **不是 `Clone`**（要复制就重新 `ImageBuffer` 包一份）。
- `RenderImage` 是 **BGRA**（gpui `assets.rs` 文档原话）。ffmpeg 输出 `bgra` 正好对上；`image` crate 给 RGBA，
  要手动对调 R/B（`player::tests` 用纯红 PNG 钉死）。
- 标题栏：`appears_transparent = true` + `app_owns_titlebar_drag = true` + 自己 `pl(px(78.))` 让开红绿灯，
  `on_mouse_down(Left, |_, w, _| w.start_window_move())`。
- 字形覆盖不可靠 → 按钮一律用**中文小字 chip**（静音 / 循环 / 信息 / 倍速 / 全屏 / 置顶 / 导出 / 适应）。
- **`Window::window_handle()` 遮住了 `HasWindowHandle::window_handle()`**（前者返回 `AnyWindowHandle`）：
  要拿原生句柄必须写 `<Window as HasWindowHandle>::window_handle(window)`。
- **`toggleFullScreen:` 得先补 `NSWindowCollectionBehaviorFullScreenPrimary`**，否则 AppKit 直接空操作。
  全屏切换有动画，切完前别读 `styleMask`，否则 `app` 侧状态会来回打架（用 `fs_settle` 挡 900ms）。
- **主线程上绝不能调"会自己开事件循环"的同步 API**（`runModal` 一类，含 rfd 的
  `FileDialog::save_file()` / `pick_file()`）。GPUI 的帧源是挂在**主队列**上的 dispatch source
  （`gpui-pre-macos/src/display_link.rs`），模态期间照样触发 → 帧回调**重入** GPUI → panic；
  而它站在 `extern "C"` 边界上没法 unwind → 直接 abort，日志只有
  `panic in a function that cannot unwind`。
- **`AsyncFileDialog` 也不保证异步！** rfd 0.15 的 macOS 后端判断
  `NSApp.isRunning() && win.is_some()`（`backend/macos/modal_future.rs:79`），
  不满足就打印 "fallback to sync dialog" 然后**当场同步 `run_modal()`** —— 同一个坑。
  所以弹面板前必须自己过一遍 `native::async_sheet_available(window)`（有 ns_window +
  `NSApplication.isRunning()`），不满足就**不开面板、给 toast**；rfd 调用再包一层
  `catch_unwind`，它内部 panic 也只能变成提示。**且必须 `.set_parent(&*window)`**。
  复现妙招：无头 `#[gpui::test]` 里 `simulate_click("shot")` —— 无头环境必然触发回退，
  rfd 会 panic（"Fallback Sync Dialog Must Be Spawned On Main Thread"），
  这条测试正好把"点导出按钮不能崩"钉住。
- `[profile.release] strip = "debuginfo"`（**不写 `strip = true`**）：只剥 DWARF、留符号表，
  15MB → 22MB，换 `.ips` / backtrace 直接出函数名。panic 钩子里**只用 `let _ = writeln!(stderr)`**
  （`eprintln!` 遇 stderr 断管会自己 panic，已在 panic 中 → 直接 abort，原始消息全丢），
  并且**先写文件**。
  轮询用它返回的 future（`Waker::noop()` 每帧 poll 一次，见 `PendingSave`）。

## 播放内核（勿回退）

- **画面朝向 `Orientation{rot,flip_h,flip_v}` 是"屏幕空间"语义**：`rot` = 屏幕上看到的图
  顺时针转过的角度，翻转作用在转完之后，所以 `flipped_h/v` 里 `rot=(360-rot)%360`。
  有 `canonical()`/`same_effect()` 处理 `上下翻转 = 左右翻转+旋转180°` 的等价写法。
  视频：滤镜接在 `scale=w:h` 之后（`transpose=1/2`、`hflip/vflip`），
  换角度 = `spawn(当前位置)` 重开解码（同 seek 路径）；90/270 时读的字节按 `dims()` 宽高互换。
  静态图片：`player::orient_image` 在 CPU 上按 `map_back` 搬像素，源图另存 `still_base`。
  **两套语义由 `--selftest` 的"真跑 ffmpeg 逐像素核对"钉在一起**，改一处必须两边一起改。

- **GPUI 回调里绝不能 panic**：objc 方法边界上 panic 不能 unwind → 直接 abort。
  click 回调里**禁止同步弹 rfd 对话框**（内部 runModal = 事件分发里再开嵌套事件循环），
  已因此崩过一次（点"截图"）。导出流程：`request_export` 记账 → 下一帧 `tick()` 里弹对话框。
- **main.rs 装了 panic 日志钩子**：任何 panic 落 `~/Library/Logs/iPlayer/crash-<ts>.log`
  （消息+位置+force_capture 栈）。GUI 崩溃先看这里，别再解析无符号 .ips。

- **`ended()` 不能只看"通道空了"**：`smol::channel` 的 `Empty` 只是"这一刻还没解出来"（解码跟不上、刚 seek
  完都会这样），**只有 `Closed` 才算解码线程收工**。还必须要求 `current_pts >= 0`：起播瞬间是 -1，
  不挡住会立刻被判"播完" → `pause()` → 帧循环死掉，表象是"界面卡在起播"。
- **暂停时也要允许挑一帧**：暂停中 seek 后手上是空的，`frame()` 直接返回缓存会让界面一直空着；
  冻结条件已抽成 `freeze_current(playing, has_current, awaiting)`（带真值表单测）——
  **"刚 seek、正等新画面"（`awaiting`）时即便暂停也必须挑帧**，否则画面停在旧位置。
- **seek / 换角度重开解码时绝不把 `current` 清成 None**（拖进度条黑屏闪的真凶）：
  ffmpeg 重新起进程 + 定位 + 解出第一帧要 20ms 起（实测中位 24ms），这段时间旧画面必须留在屏上。
  但 `current_pts` 要归 `-1`，否则往回拖时新帧 pts 更小，会被 `set_current` 的
  `f.pts >= current_pts` 单调性挡死。
- **`awaiting` 要传给帧循环**：`App::tick` 的 `busy` 必须算上 `Player::awaiting()` ——
  暂停时拖进度条，帧循环一停新画面就没人画。
- **拖动进度条只重开视频**：`Player::scrub()`（音频不动）给 `SliderEvent::Change`，
  `Player::seek()`（音视频一起）给 `Release` / 快捷键。一次拖动起停 2 个 ffmpeg 声音会碎。
- `empty_spawn`（本轮重开一帧没上屏 + 通道已关）→ `ended()` 直接为真，救"拖到最右端定位到
  末尾之外，`current_pts` 永远 -1 → 永远判不了播完"的老毛病。
- A/V 同步：有音轨时音频钟为准（cpal 回调数已消费帧），无音频回退墙钟。
- 选帧：取 `pts <= pos + TOL(0.004)` 的最新帧，早于 `LATE=0.5s` 的丢弃。
- 背压：解码线程 ↔ 呈现用有界 `smol::channel`(3) + `try_send` 轮询（**别阻塞发送**，取消会不及时）。
- Seek：kill 子进程，`-ss <base>` 放 `-i` **之前**重开；输出时间戳归零。
  拖动节流 `SCRUB_INTERVAL`（90ms，`pub(crate)`，`--selftest` 会打出实测出帧延迟做配平检查）。
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

- **画面适配 = `ObjectFit::Contain`**（视频与图片统一，`render_stage` 里写死）。播放区尺寸
  不随媒体变；**绝不用 `Fill`**（会把画面拉变形，用户明确否掉）。想贴边也只许换 `Cover`。
  `FitMode` 枚举与 `App::fit` 字段已删。
- **黑白单色，不用蓝色**：light accent `#101216`，dark accent `#f4f6f9`。
- **侧栏行文本一律单行 + `…`**：写作 `.flex_1().min_w(px(0.)).truncate()`。
  `min_w(px(0.))` 不能省（flex 默认 `min-width:auto` 会顶开，省略号不出现）。
  gpui 的 `.truncate()` = overflow_hidden + nowrap + `ELLIPSIS="…"`；**别自己按字数截**，
  宽度由布局决定。
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

- 回归必须过：`cargo test`（70 个）+ `--selftest`（解码 / seek / 2x 倍速 / 暂停冻结 / 暂停中 seek 出帧 /
  **模拟拖动 2 秒（黑屏采样必须 0）+ 预览 seek 出帧延迟** / 换角度 / 三个导出任务真跑一遍并校验产物 magic）。
  `--info <file>` 会打印媒体规格，图片还会报真实解码尺寸。
- **"拖进度条黑屏/卡帧"这类回归要能量化**：`--selftest` 的模拟拖动就是每 100ms 一次 `scrub()`、
  每 2ms 取一次 `frame()`，数取到 `None` 的次数（= 黑屏帧）。修完必须是 0，
  并且延迟中位数要明显小于节流值（否则每个解码进程都会被下一次 seek 掐掉，画面看着是卡住的）。
- **解码类回归可以进 `cargo test`**：用内置 ffmpeg（`ffmpeg::base_command()`）现造一段
  4s 无声 `testsrc` 短片（`-an` 免得拉起 cpal），`media::probe` + `Player::open` 后直接驱动，
  找不到 ffmpeg 就 `return` 跳过。见 `player::tests::scrub_keeps_the_old_frame_until_the_new_one_arrives`。
  **写完要验证它是真钉子**：把旧行为临时放回去跑一遍，确认这条用例确实红。
- `std::env::temp_dir()` 在 macOS 是 `/var/folders/.../T`（`getconf DARWIN_USER_TEMP_DIR`），不是 `/tmp`。
- 沙箱禁用 `ps`、`screencapture`。**GUI 只能靠"进程存活 + 临时 trace 文件"**：在 `tick()` 里把
  `position / is_playing` 追加写 `/tmp/*.log`，跑几秒读回 —— 这次就是靠它抓到"帧循环只转了 3 帧"。
  **用完必须清干净**（`grep -rn "TEMP-TRACE" src/` 应为空）。
- 下 GitHub 必须走 Clash 代理 `socks5h://127.0.0.1:7897`；`binaries/` 里的静态 ffmpeg 下载后**没有执行位**，要 `chmod 755`。

## 拖放回调（勿回退）
- `on_drop` 回调参数类型**必须与 active_drag 的值类型一致**（按 TypeId 匹配）：
  外部文件拖放是 `&ExternalPaths`，写成 `&Arc<ExternalPaths>` 监听器永远不触发。
- 侧栏搜索框透明化：gpui-component 的 `Input::new(..).appearance(false)` 去白底去边框。
