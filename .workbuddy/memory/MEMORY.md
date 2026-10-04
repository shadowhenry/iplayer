# iPlayer — 项目长期笔记

## 技术栈与结构

- Tauri v2 + Rust（aarch64-apple-darwin），前端为**无构建步骤**的原生 HTML/CSS/JS（`withGlobalTauri: true`，`frontendDist: ../src`）。
- Rust 模块：`commands.rs`（Tauri 命令）、`ffmpeg.rs`（sidecar 定位 + 进度流解析）、`media.rs`（扫描 / ffprobe 探测 / 播放规划）、`stream.rs`（**边转码边播放**会话，见下节）。播放策略：`direct` / `remux` / `transcode`，产物缓存在 app_cache_dir/playback/。
- 前端 JS：`app.js`（主逻辑）、`stream.js`（MSE 喂流器，`window.Streamer`）、`viz.js`（频谱）、`icons.js`。加载顺序 index.html：icons → viz → **stream** → app。
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

## 播放可行性判定（关键规则，勿简化）

- 判定**不能只看编解码器名称**：`media.rs::video_playable(vcodec, pix_fmt, vtag)` 还要求
  - 4:2:2 / 4:4:4 一律不可直出；
  - h264 仅接受 8-bit（yuv420p / yuvj420p / nv12）——**10-bit H.264（Hi10P）WebKit 解不了**，动漫 MKV/MP4 高发；
  - hevc 必须是 `hvc1` 标签，`hev1` 需重封装（`-tag:v hvc1`）。
- 派生文件三步兜底：`-c copy` 转封装 → 失败或**产物 ffprobe 复核不过** → 自动整段转码；`prepare_playback(force_transcode)` 供前端在 video error 时再兜一层。
- 缓存 key 前缀 `v2`（强制转码再加 `t`）；改判定逻辑时记得同步升版，否则旧坏缓存会被复用。
- 造测试样本：合法 RMVB 必须 `-c:v rv20 -f rm`；`-c:v libx264 -f rm` 得到的是非法文件（会解码失败）。

### 容器感知的判定（2026-10-04 起，勿回退）

- 分三层判定，不要合并：
  - `video_stream_ok(vcodec, pix_fmt)`：码流本身能否解码（h264 仅 8-bit 4:2:0）。
  - `container_fits_video(ext, vcodec)`：容器是否装得下该编码（VP8/VP9/AV1 只能 WebM，H.264/HEVC 只能 MP4/MOV）。
  - `audio_fits(container, acodec)`：音轨能否 `-c copy` 进该容器（MP4 家族 aac/mp3/alac + `pcm_*`，WebM 仅 opus/vorbis）。
- 判定入参统一走 `StreamTraits` 结构体（ext/vcodec/acodec/pix_fmt/vtag/has_video/has_audio/has_b_frames），不要再加位置参数。
- `NATIVE_CONTAINERS` 含整个 ISO-BMFF 家族（mp4/m4v/mov/3gp/3g2/f4v）——WebView 按内部编码派发，不认文件头的 brand。
- **AVI 陷阱**：AVI/divx 只存一个采样时间戳（无 PTS），ffmpeg 读它时靠 DTS 猜 PTS，**有 B 帧的视频猜错**，
  `-c copy` 出去帧序就是乱的（画面抖动），加 `-fflags +genpts` / `-avoid_negative_ts` 都救不回来。
  故 `container_lacks_pts(ext) && has_b_frames > 0` → 必须转码（`needs_reencode_video()`）。FLV 能存 CTS，不受此限。
- **能直出的绝不重编码**：只要视频码流可解就 `remux`（音轨不兼容时只重编码音轨），仅当视频码流本身不可解才 `transcode`。
- HEVC 标签统一在 remux 时用 `-tag:v hvc1` 改写（`-c copy` 保留源码流，实测 hvcC 参数集完整）。
- `verify_derivative()` 必须断言副本 `plan == "direct"`，否则 `-c copy` 的假成功会漏过去。
- `MediaInfo.plan_reason` 是给用户看的原因文案（含 AVI/B 帧、音轨不兼容等），信息面板「转换原因」行在使用，别删。

### 各格式落点（回答"xx 格式支持吗"时按此说）

- AVI / FLV / WMV / ASF / RM(VB) / MPG(VOB/MXF/DV/AMV) / MKV：**容器不被 WebView 识别**，永远不会 direct，只有 remux 或 transcode 两种结果。
- AVI：DivX/Xvid/MJPEG/无 B 帧以外的 H.264 → transcode（B 帧规则见上）；无 B 帧 H.264 → remux。
- FLV：h264+aac/mp3 → remux；flv1(Spark)/VP6 → transcode。
- WMV/ASF：wmv1/wmv2/wmv3(VC-1)/msmpeg4 → 全部 transcode。

### 无解、必须转码的类别（回答用户时按此说）

DivX/Xvid（`mpeg4`）、MPEG-1/2、VC-1/WMV1/2/3、MS-MPEG4、Sorenson Spark（`flv1`）、VP6、RealVideo、
10-bit H.264（Hi10P）、4:4:4/4:2:2 色度、无 AV1 硬解机器上的 AV1。

注意区分：**`.flv` / `.wmv` / `.mpg` 是容器，不等于编码**。FLV 里装 H.264（含 B 帧）只需 `remux`，
装 `flv1`/VP6 才转码；`.wmv`/`.mpg` 实践中几乎总是老编码，所以看起来"一律转码"，但判据始终是编码而非扩展名。

### 已实测否掉的提速方向（别再试）

- `h264_videotoolbox` **不比** `libx264 -preset veryfast` 快（真实影片内容下瓶颈在**解码**：wmv2 1080p 软解 ≈ 3x 实时，
  编码只占小头）→ 换编码器、调 preset 都无收益。
- `-hwaccel videotoolbox` 对 mpeg2/wmv **无解码器**（`Failed setup for format videotoolbox_vld`）。
- 唯一值钱方向：**边转码边播**（MSE 喂 fMP4，跳转处 kill 后 `-ss` 重开）→ **已实现**，见下节。

## 边转码边播放 / MSE 喂流（2026-10-04 起，勿回退）

- 适用范围：**只对 `plan === "transcode"` 且有视频轨的 video**。direct / remux 仍走原路径（不浪费）。
  任何异常（含 MSE MIME 不被支持 → `code === "NO_MSE"`）静默回落到原「完整转换 + 缓存副本」路径，不弹错。
- 后端 `stream.rs`：ffmpeg `-movflags +frag_keyframe+empty_moov+default_base_moof+negative_cts_offsets -f mp4 pipe:1`
  （fMP4：moov 先写完，可流式喂 MSE）+ `-frag_duration 2000000`；`-ss` 放 `-i` 前（源内定位）；
  `GOP_SECONDS=2.0`，`gop = clamp(round(fps*2),12,400)`，libx264 `veryfast/crf23`，音频 `aac 192k ac 2`。
  读线程读满 64 KiB → `sync_channel(16)`（≈1 MiB）→ 满则阻塞 = **背压**，编码器被播放节奏节流。
  命令 `stream_start/stream_read/stream_status/stream_stop`；`stream_read` 用 `tauri::ipc::Response::new(Vec<u8>)`
  走**原始字节 IPC**（JS 直接拿 ArrayBuffer，无 base64）。`lib.rs` 在 `RunEvent::Exit` 调 `stream::kill_all()` 防孤儿。
- **`-ss` 定位后输出时间戳一律归零**（与源容器无关）→ 定位到别处必须靠前端 `timestampOffset`，别指望 ffmpeg 带偏移。
- 前端 `stream.js`：手工解顶层 box，**只 append 完整 box**；MIME 从 `avcC` 推 `avc1.PPCCLL`（不猜 profile）。
  常量 `PULL=262144, AHEAD=30, MAX_BUFFERED=180, RESTART_GAP=12, IDLE=200, PRIME=0.35, OPEN_TIMEOUT=25000`。
- **三个必踩的坑**：
  1. `ms.duration` 必须预先设（`duration + 1`），否则 WebKit 把 `seekable` 限制在 buffered 内，seek 被钳回。
  2. 跳转后播放头在 gap 中时不能用全局 `bufferEnd()` 判"缓冲够"（会误判停止拉取），要取**播放头所在区间末端** `bufferAhead()`。
  3. 程序自身设 `currentTime` 会触发 `seeking`，需 `restarting` 守卫（`Streamer.repositioning()`）忽略，否则视觉回跳。
- 实测：1080p WMV2 120s/741MB **起播 0.73～1.04 秒**，无磁盘临时文件；前跳/回跳 600ms 内恢复；
  播完自动接下一集；direct/remux/图片/音频四条常规路径无回归。
- 临时自测手法：应用内注入 `__livetest*()` + 临时 `dbg_log` 命令写 `/tmp/iplayer-selftest.log`，
  跑 `target/debug/iplayer` 真实窗口（比 headless Chrome 更贴近 WebKit 行为）。**用完必须清理干净**。

### 切换文件 = 立刻交出一切（2026-10-04 起，勿回退）

- **前端票据**：`state.openSeq`。`openFile` 开头 `const seq = ++state.openSeq`，每个 `await` 后
  `if (stale()) return;`；`openImage` 也要 `openSeq++`。**不要用 `state.current !== file` 判断**——
  用户 A→B→A 时旧调用会误认为自己是当前。`error` 自愈重试路径同样要 seq 守卫。
- `releasePlayer()` = `Streamer.stop()` + `cancelConversion()` + 摘 src + `load()`，是唯一的"交棒"入口。
- **`state.streaming / plan / streamDuration / busyUntilPlaying` 只在 `markStreaming(info)` 里写**，
  且必须 `stale()` 通过后才调；写进 `startStream` 会让慢启动在用户切走后回写全局状态。
- **后端取消**：`ffmpeg::Cancel`（AtomicBool）+ `run_cancellable(...)`；取消在 `-progress` 行循环里检测
  （ffmpeg 的 progress 是墙钟节拍，≈0.5s 一次）→ **不到 1 秒内**停，不需要信号/pid。
  `commands.rs` 的 `BUILDING` 槽 + `claim_build/release_build/cancel_playback`：同一时刻只允许一个派生任务，
  新任务自动取消旧的。**任何失败或取消都必须 `fs::remove_file(&out)`**，否则半成品会被下次的
  `out.is_file() && len > 1024` 当成缓存命中 → 播出坏文件。
- **`stream.rs` 的 `stopped: Arc<AtomicBool>`**：`kill()` 必须先置 stopped 再 kill child；
  `pump_pipe` 用 `try_send` 轮询、`pull` 用 `recv_timeout` 轮询。否则 `rx.recv()`/`tx.send` 会在满载通道上
  **永久阻塞**，每次切文件泄漏 1 线程 + 整个会话（~1 MiB）。
- `stream_stop` / `stream_status` 必须 **async**：sync 命令跑在**主线程**，`kill()` 里的 `child.wait()` 会卡 UI。
- `stream.js::stop()` 里要 `restarting = false`，否则上一个会话的跳转守卫会拦住新文件的 seek。
- 判断行为正常的方法：临时 `_dbg_sessions()` 返回会话表，**每一步都应只有 1 条**；再配 `#toast-host` 文本
  （取消不该弹错）。缓存目录为空 = 半成品已清。
- **本机 Rust 1.99 没有 `std::sync::mpsc::SendTimeoutError`**（`SendTimeoutError` 不在 `sync::mpsc` 里），
  需要超时发送就用 `try_send` + `sleep` 轮询。
- **别再从 release 二进制搜命令名验证注册**：release 下连 `tool_status` 都搜不到（0 次），
  能搜到的 `prepare_playback` 全是 **panic location 表**里的函数名。只信"宏编译通过 + 真机跑通"。

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

- `cargo test --lib planner_tests`（`src-tauri/src/media.rs` 末尾）覆盖播放判定矩阵，改判定逻辑必跑。
- headless Chrome + mock Tauri API 渲染 UI 截图（`--headless=new --no-sandbox --enable-unsafe-swiftshader --virtual-time-budget`）。
  **`--no-sandbox` 必须加**，否则报 `sandbox initialization failed` 且 stdout 为空（看起来像"没输出"）。
  测业务逻辑时把 `index.html` 拷成探针页（保留真实 DOM），在其前面注入 mock `__TAURI__`，用完即删。
- headless 下 `<video>` 无法截到画面帧（canvas drawImage 同样无效），README 截图需用 ffmpeg 预渲染帧图后以 `<img>` 注入 stage。
- 无头渲染 modal 时需临时禁用 `backdrop-filter` / `animation`，否则面板不显示（仅测试环境问题）。
- zsh 不会对未加引号的变量做分词：ffmpeg 参数里的 `-map 0:v:0?` 必须加引号，否则报 `no matches found`。
