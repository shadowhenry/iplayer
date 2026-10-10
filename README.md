<h1 align="center">
  <img src="docs/logo-rounded.png" alt="iPlayer logo" width="44" height="44" valign="middle">
  iPlayer - 本地媒体播放器
</h1>

<p align="center">基于 <b>Rust + GPUI</b> 的桌面媒体播放器，ffmpeg 解码直出纹理上屏，不产生任何转码缓存。</p>

<p align="center">
  <img src="docs/showcase.jpg" alt="iPlayer 播放界面" width="760">
</p>

## 功能特点

- 音视频通吃：MP4 / MKV / AVI / MOV / WMV / FLV / RMVB / TS / WebM、MP3 / FLAC / APE；解码全交给 ffmpeg，不转码不转封装、不产生磁盘副本
- 图片查看：JPG / PNG / WebP / BMP / ICO / TIFF / AVIF / EXR / PSD，超过 3840 长边自动缩略
- SVG 用 resvg 矢量重绘，放大不糊；动画 GIF 走播放管线，自己循环、进度与倍速照常
- 导出工具箱：截图（无损 PNG）、提取音频（优先无损复制，不行再转 AAC）、转 GIF（两遍调色板，从当前位置起 5 秒），带进度、可取消
- 左侧文件列表：扫描当前文件夹（可选含子目录）、文件名筛选（一键清空关键字）、视频 / 音频 / 图片分类、整栏可收起、一键清空列表
- 播放控制：拖拽进度、播放 / 暂停、上下集、停止、音量、静音、0.5–3.0 倍速、关 / 单曲 / 列表循环
- 字幕：控制条「字幕」按钮里可导入 SRT / WebVTT / ASS，随时显示 / 隐藏（`S`）、切字号、移除；打开媒体时自动认同名字幕（`movie.mp4` → `movie.srt`）
- 图标浮层（画面角度 / 字幕 / 导出工具箱）：鼠标键盘停住 10 秒就自己收起，不会一直挂在右下角
- 画面等比适配（不拉伸、留黑边）、全屏、窗口置顶；控制条最右端有「最大化」：点一下铺满屏幕、再点一下还原（图标随窗口状态在 放大 / 还原 间切换，与系统红绿灯状态同步）
- 媒体信息浮层：分辨率、帧率、编码与像素格式、声道、码率、体积、容器
- 深色 / 浅色黑白配色一键切换；文件或文件夹可直接拖进窗口
- 系统文件关联：访达里双击媒体文件、右键「打开方式 → iPlayer」、拖到 Dock 图标上都能直接开播（`⌘/Ctrl + D` 一键设为默认播放器）

### 快捷键

| 按键 | 功能 | 按键 | 功能 |
| --- | --- | --- | --- |
| `空格` | 播放 / 暂停 | `←` / `→` | 快退 / 快进 5s |
| `↑` / `↓` | 音量 ±5% | `M` | 静音 |
| `P` / `N` | 上一个 / 下一个 | `[` / `]` | 减速 / 加速（0.5x 步进） |
| `F` | 全屏 | `T` | 窗口置顶 |
| `S` | 显示 / 隐藏字幕 | `E` | 展开 / 收起导出面板 |
| `Esc` | 收起浮层面板 / 文件列表 | `⌘/Ctrl + T` | 切换主题 |
| `⌘/Ctrl + .` | 停止 | `⌘/Ctrl + O` | 打开文件夹 |

## 技术架构

| 层 | 技术 |
| --- | --- |
| 界面渲染 | GPUI（`gpui-kit 0.7.1`），纯 Rust 声明式 UI，无 HTML / CSS / JS |
| 视频解码 | ffmpeg 子进程 → `-f rawvideo -pix_fmt bgra` → 管道 → GPUI 纹理 |
| 音频输出 | ffmpeg 子进程 → cpal（CoreAudio / WASAPI / ALSA） |
| 图片解码 | `image` crate（常见位图）/ resvg（SVG）/ ffmpeg 单帧兜底（AVIF、EXR、PSD） |
| 字幕 | 自研解析 SRT / WebVTT / ASS（`src/subtitle.rs`），按播放位置取条，文本层叠在画面上 |
| 时钟同步 | 有音轨时以音频钟为准（cpal 已消费帧数），无音轨回退墙钟 |
| 原生窗口 | `raw-window-handle` + `objc2`，macOS 下直接操作 NSWindow |
| 打包 | `scripts/bundle-{macos,linux}.sh`、`scripts/bundle-windows.ps1` |

## 快速开发

```bash
cargo run --release                    # 空窗口启动
cargo run --release -- <文件或文件夹>   # 启动即播
cargo test                             # 单元测试
```

各平台的前置条件：

- **通用**：Rust（edition 2024）与 ffmpeg / ffprobe —— 装进系统 PATH，或把静态构建放到 `binaries/ffmpeg-<目标三元组>`（Windows 记得带 `.exe`）
- **macOS**：Xcode Command Line Tools
- **Windows**：Visual Studio Build Tools 的「使用 C++ 的桌面开发」+ MSVC 工具链
- **Linux**：Wayland / X11 与音频开发库，例如 `sudo apt install libxkbcommon-dev libwayland-dev libfontconfig1-dev libasound2-dev`

## 打包

三个平台各有一个脚本，产物都自带 ffmpeg / ffprobe，拷到别的机器就能用。

### macOS

```bash
scripts/bundle-macos.sh              # dist/iPlayer.app
scripts/bundle-macos.sh --dmg        # 再加 dist/iPlayer-<版本>.dmg
scripts/bundle-macos.sh --zip --run  # 打成 zip 并直接打开
```

只用 Xcode 自带的 `codesign` / `hdiutil`，顺手做一次 ad-hoc 签名；
首次打开若被 Gatekeeper 拦下，右键 → 打开即可。

### Windows

```powershell
pwsh scripts/bundle-windows.ps1        # dist\iPlayer-win64\
pwsh scripts/bundle-windows.ps1 -Zip   # 再加 dist\iPlayer-<版本>-win64.zip
```

产出免安装目录与 zip；要安装向导的话，用 Inno Setup / NSIS 在外层再包一层。

### Linux

```bash
scripts/bundle-linux.sh            # dist/iPlayer-linux64/
scripts/bundle-linux.sh --tar      # 再加 dist/iPlayer-<版本>-linux64.tar.gz
```

便携目录里带 `.desktop` 和 `install.sh`（挂到用户级应用菜单，不需要 root）；
要单文件分发可以再用 AppImage 包一层。

## License

本项目基于 [MIT 协议](LICENSE) 开源。
