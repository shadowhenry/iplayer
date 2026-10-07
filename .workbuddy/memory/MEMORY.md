# iPlayer — 项目长期笔记

## 技术栈与结构

- Tauri v2 + Rust（aarch64-apple-darwin）；前端为**无构建步骤**原生 HTML/CSS/JS（`withGlobalTauri: true`，`frontendDist: ../src`）。
- Rust 模块：`commands.rs`（命令）、`ffmpeg.rs`（sidecar 定位 + `-progress` 解析）、`media.rs`（扫描 / ffprobe / 播放规划）、`stream.rs`（**边转码边播**）。策略 `direct` / `remux` / `transcode`，缓存在 `app_cache_dir/playback/`。
- 前端 JS：`app.js`（主逻辑）、`stream.js`（MSE 喂流，`window.Streamer`）、`viz.js`（频谱）、`icons.js`。index.html 加载顺序 icons → viz → **stream** → app。
- `media.rs::kind_of()` 三类：video / audio / image（IMAGE_EXTS 直出 `<img>`，不走 ffprobe）。
- ffmpeg/ffprobe 为 externalBin sidecar，解析顺序：**包内 `Contents/MacOS/` → bundle Resources → `src-tauri/binaries/` → PATH**，每个候选真跑一次过 `works()` 才采用，结果进 `OnceLock`。
- 图标：`scripts/make-icons.py [源图]`；`src/assets/logo.png` 空状态页。

## 勿回退：拖动进度条崩溃修复

- 根因：WebKit seek 会取消进行中的 Range 请求，而 wry 的 `WKURLSchemeTask` 在 task 已停止后仍回调 → ObjC 异常 `This task has already been stopped`，objc2 默认让它 unwind 进 Rust → SIGABRT（`tokio-rt-worker`）。
- 修复三件套（缺一不可）：① `Cargo.toml` 加 `objc2` 并启用 **`catch-all`** feature；② `[profile.release]` **不能有 `panic = "abort"`**（显式 `panic = "unwind"`）；③ `lib.rs::install_panic_hook()` 过滤这条已知无害 panic。
- 复现/验证：**必须用 release 构建** + **清空 `app_cache_dir/playback/`**（缓存命中不易触发）。
- 环境限制：沙箱禁用 `ps`、`screencapture`；`pgrep` 在命令替换里不可靠，判断进程存活用单独一次 pgrep 或后台任务状态。

## 勿简化：播放可行性判定

- 三层，不要合并（入参统一走 `StreamTraits` 结构体，别再堆位置参数）：
  - `video_stream_ok(vcodec, pix_fmt)`：码流本身能否解（h264 仅 8-bit yuv420p/yuvj420p/nv12；4:2:2/4:4:4 一律不可直出；**10-bit H.264/Hi10P 不行**）。
  - `container_fits_video(ext, vcodec)`：容器装不装得下（VP8/VP9/AV1 只能 WebM；H.264/HEVC 只能 MP4/MOV）。
  - `audio_fits(container, acodec)`：音轨能否 `-c copy`（MP4 家族 aac/mp3/alac + `pcm_*` + **ac3/eac3**；WebM 仅 opus/vorbis）。
- `NATIVE_CONTAINERS` 含整个 ISO-BMFF 家族（mp4/m4v/mov/3gp/3g2/f4v），WebView 按内部编码派发，不认 brand。
- **AVI 陷阱**：AVI/divx 无 PTS，ffmpeg 靠 DTS 猜，**有 B 帧就猜错**（`-c copy` 出去帧序乱、画面抖动，`+genpts`/`avoid_negative_ts` 都救不回）→ `container_lacks_pts(ext) && has_b_frames > 0` 必须转码。FLV 能存 CTS，不受限。
- **能直出的绝不重编码**：视频码流可解就 `remux`（仅音轨不兼容时重编音轨），仅视频码流不可解才 `transcode`。HEVC 在 remux 时用 `-tag:v hvc1` 改写。
- 派生三步兜底：`-c copy` → 失败或**产物 ffprobe 复核不过** → 整段转码；`prepare_playback(force_transcode)` 供前端 video error 再兜一层。`verify_derivative()` 必须断言副本 `plan == "direct"`。
- 缓存 key 前缀 **`v3`**（强制转码加 `t`）；改判定逻辑要同步升版。AC-3 那次**不需要**升版（旧 aac 副本仍能播；新判 direct 的根本不查缓存）。
- `MediaInfo.plan_reason` 是给用户看的原因文案，信息面板在用，别删。
- 造样本：合法 RMVB 必须 `-c:v rv20 -f rm`（`libx264 -f rm` 是非法文件）。

### 音频：AC-3/E-AC-3 在 macOS WebKit 原生可解（勿回退）

- macOS WebKit 走 AVFoundation，**自带 AC-3 / E-AC-3 解码器**；「浏览器不支持 AC-3」只对 Chromium/Firefox 成立。故 `MP4_ACODECS` 与 `NATIVE_ACODECS` 都含 `ac3/eac3`：`mp4/mov + ac3` → **direct**（零副本）；MKV/TS + AC-3 → `remux` 且 `-c:a copy`。
- **只有 DTS 不行**：容器被接受、视频正常、**音频静音**（RMS=0）→ DTS 继续重编码为 aac，`NATIVE_ACODECS` 里**不要**加 dts。
- 收益（3min 1080p/181MB，MKV+AC-3）：旧 `-c:v copy -c:a aac` 1.81s → 新 `-c copy` 0.17s（≈10×）。
- **判定音频能否播必须真播一遍量 RMS，不要信 `canPlayType`**（本机 `avc1,aac` 返回空串却能播，`ac-3` 返回 `probably`）。手法：`AudioContext → createMediaElementSource → AnalyserNode → getFloatTimeDomainData`，RMS>0 出声。轻量做法：离屏 WKWebView 探针（`swiftc` + `NSWindow.orderFrontRegardless()`）。

### 各格式落点

- 容器不被 WebView 识别（永不为 direct，只有 remux/transcode）：**AVI / FLV / WMV / ASF / RM(VB) / MPG(VOB/MXF/DV/AMV) / MKV**。
- AVI：无 B 帧 H.264 → remux；其余（DivX/Xvid/MJPEG/有 B 帧 H.264）→ transcode。
- FLV：h264 + aac/mp3 → remux；flv1(Spark)/VP6 → transcode。WMV/ASF：wmv1/2/3、msmpeg4 → 全转码。
- 必须转码的编码：DivX/Xvid(`mpeg4`)、MPEG-1/2、VC-1/WMV1/2/3、MS-MPEG4、`flv1`、VP6、RealVideo、10-bit H.264、4:4:4/4:2:2、无硬解的 AV1。
- **`.flv`/`.wmv`/`.mpg` 是容器不等于编码**，判据始终是编码而非扩展名。

### 已实测否掉的提速方向（别再试）

- `h264_videotoolbox` 不比 `libx264 -preset veryfast` 快（瓶颈在**解码**，编码只占小头）。
- `-hwaccel videotoolbox` 对 mpeg2/wmv 无解码器。唯一值钱方向就是「边转码边播」，已实现。

## 边转码边播 / MSE（勿回退）

- 范围：**只对 `plan === "transcode"` 且有视频轨的 video**；direct/remux 走原路径。异常（含 MIME 不支持 → `code === "NO_MSE"`）静默回落，不弹错。
- 后端 `stream.rs`：`-movflags +frag_keyframe+empty_moov+default_base_moof+negative_cts_offsets -f mp4 pipe:1` + `-frag_duration 2000000`；`-ss` 放 `-i` 前；`GOP_SECONDS=2.0`；libx264 veryfast/crf23；音频 aac 192k ac 2。读线程 64 KiB → `sync_channel(16)`（≈1 MiB）满则阻塞 = **背压**。命令 `stream_start/read/status/stop`；`stream_read` 用 `tauri::ipc::Response::new(Vec<u8>)` 走**原始字节 IPC**。`lib.rs` 在 `RunEvent::Exit` 调 `stream::kill_all()`。
- **`-ss` 后输出时间戳一律归零** → 定位靠前端 `timestampOffset`，别指望 ffmpeg 带偏移。
- 前端 `stream.js`：手工解顶层 box，**只 append 完整 box**；MIME 从 `avcC` 推 `avc1.PPCCLL`。常量 `PULL=262144, AHEAD=30, MAX_BUFFERED=180, RESTART_GAP=12, IDLE=200, PRIME=0.35, OPEN_TIMEOUT=25000`。
- **三个必踩的坑**：① `ms.duration` 必须预先设（`duration+1`），否则 WebKit 把 `seekable` 钳在 buffered 内；② 跳转后播放头在 gap 中时要用**播放头所在区间末端** `bufferAhead()` 判缓冲，不能用全局 `bufferEnd()`；③ 程序自己设 `currentTime` 会触发 `seeking`，需 `restarting` 守卫（`Streamer.repositioning()`）。
- 实测：1080p WMV2 120s/741MB **起播 0.73～1.04s**，无磁盘临时文件；前跳/回跳 600ms 内恢复。

### 切换文件 = 立刻交出一切（勿回退）

- **票据 `state.openSeq`**：`openFile` 开头 `const seq = ++state.openSeq`，每个 `await` 后 `if (stale()) return;`；`openImage` 也要 `openSeq++`。**不要用 `state.current !== file` 判断**（A→B→A 会误判）。
- `releasePlayer()` = `Streamer.stop()` + `cancelConversion()` + 摘 src + `load()`，唯一「交棒」入口。
- `state.streaming / plan / streamDuration / busyUntilPlaying` **只在 `markStreaming(info)` 里写**，且须 `stale()` 通过后才调。
- 后端取消：`ffmpeg::Cancel`(AtomicBool) + `run_cancellable()`，在 `-progress` 行循环检测（≈0.5s 节拍）→ **<1s 停**。`commands.rs` 的 `BUILDING` 槽（`claim_build/release_build/cancel_playback`）保证同一时刻只有一个派生任务。**任何失败/取消都必须 `fs::remove_file(&out)`**，否则半成品会被当缓存命中 → 播坏文件。
- `stream.rs` 的 `stopped: Arc<AtomicBool>`：`kill()` 先置 stopped 再 kill child；`pump_pipe` 用 `try_send` 轮询、`pull` 用 `recv_timeout` 轮询，否则满载通道**永久阻塞**（每次切文件泄漏 1 线程 + ~1 MiB）。
- `stream_stop` / `stream_status` 必须 **async**（sync 命令跑主线程，`kill()` 里 `child.wait()` 会卡 UI）。`stream.js::stop()` 要 `restarting = false`。
- 自测：临时 `_dbg_sessions()` 返回会话表，**每步应只有 1 条**；缓存目录为空 = 半成品已清。
- **本机 Rust 1.99 没有 `std::sync::mpsc::SendTimeoutError`** → 用 `try_send` + `sleep` 轮询。
- **别从 release 二进制搜命令名验证注册**（release 下搜不到，命中的全是 panic location 表）。只信「宏编译通过 + 真机跑通」。

## 本机环境（重要）

- **Rust 走清华镜像装**（直连 6KB/s）；crates 镜像 rsproxy，写在 `~/.cargo/config.toml`。`~/.zshrc` 末尾已加 `[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"`（rustup 用了 `--no-modify-path`，不加载会 `cargo metadata: No such file`）。
- **`src-tauri/binaries/` 必须存在**（被 gitignore），否则 build/test 直接失败（`resource path binaries/ffmpeg-aarch64-apple-darwin doesn't exist`）。
  ⚠️ **跑 `npm run fetch:sidecar` 前先 `find src-tauri/binaries -type l -delete`**：脚本用 `writeFileSync` 写目标路径，**会顺着软链写穿**覆盖掉（如 Homebrew ffmpeg）。
- **`npm run build` 报 `tauri: command not found` = `node_modules` 没装**（依赖不在仓库）。`npm install`（2 个包）即可，别全局装 CLI。
- **下 GitHub release 必须走用户 Clash 代理 `--proxy socks5h://127.0.0.1:7897`**（`socks5h` 比 `socks5` 快；`7890` 没监听）。环境里的 `HTTP_PROXY=http://127.0.0.1:50652` 是内部代理，**下不动 GitHub**（`http=000`/exit 56）；Node `fetch`(undici) **默认不读 proxy 环境变量**，所以 `npm run fetch:sidecar` 一直失败，直接 curl 手动下。
- sidecar 现为 **eugeneware/ffmpeg-static b6.1.1 静态构建**（arm64 + x64 各 ffmpeg/ffprobe，~243MB，`otool -L` 只剩系统库 → **可分发**）。实际版本 **ffmpeg 6.0**。curl 下载**没有执行位**，要 `chmod 755`。详情见 `2026-10-07.md`。
- 打包只装 host triple 的 sidecar：arm64 的 `iPlayer.app` ≈ **94 MB**。
- DMG 打包在本机不稳定（`bundle_dmg.sh` 挂起）→ `bundle.targets` 只留 `["app"]`。
- `osascript -e 'quit app "iPlayer"'` 对 release 版无效，要直接 `kill`。

## UI 规范（用户明确要求）

- 主题色**黑/白单色，不用蓝色**：light accent `#101216`，dark accent `#f4f6f9`。
- 控制条：无顶部分隔线；播放组（上一个/播放/下一个/停止）整行居中；播放键无底色无边框，仅三角图标，色 = `var(--text)`。
- 倍速 0.5x–3.0x，步进 0.5（快捷键 `[` `]`）。
- 侧栏按钮顺序：「打开文件」在前、「打开文件夹」在后；无底部文件计数行；无「尚未选择文件夹」占位。

## 验证手法

- `cargo test --lib planner_tests`（`media.rs` 末尾）覆盖播放判定矩阵，改判定必跑。
- headless Chrome + mock Tauri API 渲染 UI 截图：`--headless=new --no-sandbox --enable-unsafe-swiftshader --virtual-time-budget`；**`--no-sandbox` 必须加**，否则 `sandbox initialization failed` 且 stdout 为空（像「没输出」）。把 `index.html` 拷成探针页（保留真实 DOM），前面注入 mock `__TAURI__`，用完即删。
- headless 下 `<video>` 截不到画面帧（canvas drawImage 同样无效）→ 截图需 ffmpeg 预渲染帧图后以 `<img>` 注入 stage。
- 无头渲染 modal 需临时禁用 `backdrop-filter` / `animation`。
- zsh 不做分词：ffmpeg 的 `-map 0:v:0?` **必须加引号**，否则 `no matches found`。
