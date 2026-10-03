# iPlayer

基于 **Rust + Tauri v2** 的桌面视频播放器。

![tech](https://img.shields.io/badge/Rust-Tauri_2-orange) ![platform](https://img.shields.io/badge/platform-macOS_|_Windows_|_Linux-blue) ![license](https://img.shields.io/badge/license-MIT-green)

## 功能特性

| 需求 | 实现 |
| --- | --- |
| 主流音视频与图片格式 | WebKit 原生格式直接播放；MKV / AVI / WMV / FLV / RMVB / TS 等通过内置 **ffmpeg sidecar** 自动处理；JPG / PNG / GIF / WebP / BMP / SVG / AVIF 等图片直接查看 |
| 左侧文件列表 | 自动扫描当前文件夹内的可播放文件，支持包含子目录、文件名筛选、类型过滤，可一键收起/展开（`Ctrl/⌘ + B`） |
| 底部控制条 | 固定在最下方、完全不透明：播放/暂停、上一个/下一个、进度拖拽（带时间预览）、音量、倍速、循环 |
| 主题 | 默认深色，可在设置或控制条切换「跟随系统 / 浅色 / 深色」，窗口原生外观同步变化（`Ctrl/⌘ + T` 循环切换） |
| 截图 / 音频 / GIF | 当前帧截图 PNG；提取音频为 M4A / MP3 / WAV / FLAC / 原样复制；任意时间区间转 GIF（自动调色板 + 抖动优化） |
| 窗口 | 固定窗口大小开关、画面四种自适应方式（适应/铺满/拉伸/原始尺寸）、窗口置顶（前置显示） |

![iPlayer 截图](docs/screenshot.png)

### 快捷键

| 按键 | 功能 | 按键 | 功能 |
| --- | --- | --- | --- |
| `空格` / `K` | 播放 / 暂停 | `←` / `→` | 快退 / 快进 5s（`Shift` 1s） |
| `↑` / `↓` | 音量 ±5% | `M` | 静音 |
| `F` | 全屏 | `S` | 截图 |
| `G` | GIF 工具箱 | `I` | 媒体信息 |
| `N` / `P` | 下一个 / 上一个 | `[` / `]` | 减速 / 加速（0.5x 步进） |
| `Ctrl/⌘ + B` | 收起/展开文件列表 | `Ctrl/⌘ + T` | 切换主题 |
| `Ctrl/⌘ + .` | 停止 | `Esc` | 关闭弹窗 |

## 目录结构

```
├── docs/                 # README 截图等文档资源
├── src/                  # 前端（原生 HTML/CSS/JS，无构建步骤）
│   ├── index.html
│   ├── styles.css
│   ├── icons.js          # 内联 SVG 图标
│   ├── assets/           # 空状态页 logo
│   └── app.js            # 播放器逻辑
├── src-tauri/
│   ├── tauri.conf.json
│   ├── capabilities/     # 权限配置
│   ├── binaries/         # ffmpeg / ffprobe sidecar
│   ├── icons/            # 应用图标（scripts/make-icons.py 生成）
│   └── src/
│       ├── lib.rs        # 入口 & 命令注册
│       ├── commands.rs   # Tauri 命令
│       ├── ffmpeg.rs     # sidecar 定位与进度流式解析
│       └── media.rs      # 文件扫描 / ffprobe 探测 / 播放规划
└── scripts/              # 图标生成 & sidecar 获取脚本
```

## 开发

```bash
npm install            # 安装 Tauri CLI
npm run fetch:sidecar  # 将 ffmpeg/ffprobe 复制到 src-tauri/binaries（见下）
npm run dev            # 开发模式
npm run build          # 打包 .app / .dmg
```

### ffmpeg sidecar

iPlayer 把 `ffmpeg` / `ffprobe` 作为 [Tauri sidecar](https://tauri.app/develop/sidecar/) 打包，
文件需按 `<名称>-<目标三元组>` 命名放在 `src-tauri/binaries/`，例如：

```
src-tauri/binaries/ffmpeg-aarch64-apple-darwin
src-tauri/binaries/ffprobe-aarch64-apple-darwin
```

`npm run fetch:sidecar` 会按以下顺序获取：

1. `npm i ffmpeg-static ffprobe-static` 提供的**自包含**二进制（推荐，产物可在同类机器上直接运行）
2. 系统 `PATH` 中的 ffmpeg / ffprobe（例如 Homebrew）

> 注意：若第 2 种来源来自 Homebrew，其二进制动态链接了 `/opt/homebrew` 下的 dylib，
> 打包出的 `.app` 在没有相同依赖的机器上无法运行。此时 iPlayer 启动时会自动检测并回退到系统 PATH 中的 ffmpeg。
> 需要完全可分发的产物时，请使用静态编译的 ffmpeg（方案 1）。

运行时解析顺序：应用包内 → `src-tauri/binaries/` → 系统 `PATH`，并会用 `ffmpeg -version` 验证可用性。

## 许可证

[MIT](LICENSE)
