//! iPlayer 主界面。
//!
//! 布局照抄原版：自定义标题栏 / 左侧文件列表 + 舞台 / 底部控制条。
//! 画面走 [`crate::player`] 的纯原生渲染（ffmpeg 解成 BGRA 直接上屏），
//! 不经过任何 GPU 解码或 HTML 层。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Div, Entity, ExternalPaths, FocusHandle, Focusable,
    ImageSource, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, ObjectFit, ParentElement as _, Render, RenderImage, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Window, div, img,
    px, relative,
};

use crate::export::{self, Export};
use crate::icons;
use crate::media::{self, MediaFile, MediaInfo};
use crate::native;
use crate::player::{Orientation, Player};
use crate::subtitle::{self, Subtitles};
use crate::theme::{self, Palette};
use crate::viz;

// Dock 右键菜单的「显示主界面」/ Dock 图标点击，把藏起来的窗口亮回来。
gpui_kit::actions!(iplayer, [ShowMainWindow]);

/// 空舞台 logo 的显示边长与圆角半径（用户要求 10px 圆角）。
const LOGO_SIZE: f32 = 76.0;
const LOGO_RADIUS: f32 = 10.0;

/// 把矩形图裁成圆角（`radius` 以图片像素为单位），边缘 1px 羽化做抗锯齿。
///
/// 为什么要自己裁：logo 源图是**直角**的 200×200 不透明方图，而
/// GPUI 的 `img()` 虽然在样式里收下 `rounded()`，走的却是 sprite atlas
/// 光栅化路径，圆角遮罩未必生效；`overflow_hidden` 在这个版本里也只裁
/// 矩形（`ContentMask` 只有 bounds，没有圆角）。烘进像素最稳。
fn round_corners(img: &mut image::RgbaImage, radius: f32) {
    let (w, h) = (img.width() as f32, img.height() as f32);
    let r = radius.clamp(0.0, w.min(h) / 2.0);
    if r <= 0.5 || w <= 1.0 || h <= 1.0 {
        return;
    }
    for y in 0..img.height() {
        for x in 0..img.width() {
            // 点到圆角矩形边界的有符号距离（正=图外）
            let qx = (x as f32 + 0.5 - w / 2.0).abs() - (w / 2.0 - r);
            let qy = (y as f32 + 0.5 - h / 2.0).abs() - (h / 2.0 - r);
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;
            let cover = (0.5 - d).clamp(0.0, 1.0);
            if cover < 1.0 {
                let px = img.get_pixel_mut(x, y);
                px.0[3] = (px.0[3] as f32 * cover).round() as u8;
            }
        }
    }
}

/// 空舞台的 logo。`include_bytes!` 编进二进制，`.app` 里不需要带 assets/。
fn load_logo() -> Option<Arc<RenderImage>> {
    let img = image::load_from_memory(include_bytes!("../assets/logo.png")).ok()?;
    let mut rgba = img.to_rgba8();
    // 圆角按**显示尺寸**折算到图片像素：10px @76px 显示 → 200px 图里约 26px
    let radius = LOGO_RADIUS * rgba.width() as f32 / LOGO_SIZE;
    round_corners(&mut rgba, radius);
    let (w, h) = (rgba.width(), rgba.height());
    Some(crate::player::image_from_straight_rgba(
        rgba.into_raw(),
        w,
        h,
    ))
}

/// 拖动进度条时两次真正 seek 之间的最小间隔。每次 seek 都要重开 ffmpeg，
/// 不节流的话手一抖就能把解码进程打爆。
///
/// 这个值要和"一次预览 seek 出帧要多久"配平：`--selftest` 会把中位/最慢延迟打出来，
/// 节流比它还短的话，每个进程都还没吐帧就被下一次 seek 掐掉，画面看着就是卡住的。
pub(crate) const SCRUB_INTERVAL: Duration = Duration::from_millis(90);
/// seek / 换文件之后多渲染几帧，等新画面从解码线程里出来。
const WARMUP_FRAMES: u8 = 24;
/// 提示条存活时长。
const TOAST_TTL: Duration = Duration::from_secs(3);

/// 图标弹框（角度 / 工具箱 / 字幕）的停靠时限：鼠标键盘都停住这么久就自己收起来
/// （用户要求「10 秒后自动消失，而不是一直停靠」）。
const PANEL_TTL: Duration = Duration::from_secs(10);
/// 播放组四个按键（« / ▶ / » / ■）的图标统一尺寸 —— 取原来播放键的图标大小
/// （44px 方框 × 0.55），避免小方框里的图标显得比播放键小一圈。
// 控制条所有图标键共用这一个图标尺寸：播放组四键 + 右下角（静音/循环/截图）。
// 演变：26px 方框 × 0.55 ≈ 14.3px →（反馈太小）放大一倍 28.6px →（反馈太大）缩 1/3 ≈ 19px。
// 只改图标，方框（36 / 播放键 44）不动 —— 点击热区保持不变，只是图形更收敛。
const PLAY_ICON: f32 = 28.6 * 2.0 / 3.0;

/// 字幕的基准字号与三档缩放（面板里的「小 / 中 / 大」）。
const SUB_FONT: f32 = 17.0;
const SUB_SCALES: [f32; 3] = [0.85, 1.0, 1.25];
const SUB_SIZE_LABELS: [&str; 3] = ["小", "中", "大"];


#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Video,
    Audio,
    Image,
}

impl Filter {
    const ALL: [Filter; 4] = [Filter::All, Filter::Video, Filter::Audio, Filter::Image];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "全部",
            Filter::Video => "视频",
            Filter::Audio => "音频",
            Filter::Image => "图片",
        }
    }

    fn matches(self, kind: &str) -> bool {
        match self {
            Filter::All => true,
            Filter::Video => kind == "video",
            Filter::Audio => kind == "audio",
            Filter::Image => kind == "image",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LoopMode {
    Off,
    One,
    All,
}

impl LoopMode {
    fn next(self) -> Self {
        match self {
            LoopMode::Off => LoopMode::All,
            LoopMode::All => LoopMode::One,
            LoopMode::One => LoopMode::Off,
        }
    }

    fn label(self) -> &'static str {
        match self {
            LoopMode::Off => "循环关",
            LoopMode::One => "单曲循环",
            LoopMode::All => "列表循环",
        }
    }

    fn is_on(self) -> bool {
        !matches!(self, LoopMode::Off)
    }

    fn toast(self) -> String {
        format!("循环：{}", self.label())
    }
}

/// 舞台上正在显示的东西。
enum Stage {
    Empty,
    /// 纯音频页：均衡器可视化铺满播放区，中间叠一行曲名
    Audio,
    /// 静态图片
    Image(Arc<RenderImage>),
}

/// 音频舞台上要显示的曲名：容器标签里的 `title` 优先，没有就用文件名（去掉扩展名）。
/// 两边都拿不到（比如文件已经不在列表里）就返回 `None`，舞台上不叠这一层。
fn song_title(tag: Option<&str>, path: Option<&str>) -> Option<String> {
    if let Some(t) = tag.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(t.to_string());
    }
    let name = std::path::Path::new(path?)
        .file_stem()?
        .to_string_lossy()
        .to_string();
    (!name.trim().is_empty()).then_some(name)
}

/// 把 `catch_unwind` 抓到的 panic 载荷变成一句能显示的话。
/// 只认 `&str` / `String` 两种（`panic!` 只会用这俩），别的给个笼统说法。
fn panic_reason(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "内部错误".to_string())
}

pub struct App {
    // — 文件 —
    folder: Option<PathBuf>,
    recursive: bool,
    files: Vec<MediaFile>,
    /// `files` 里当前可见的下标
    view: Vec<usize>,
    filter: Filter,
    search: Entity<InputState>,
    focus: FocusHandle,
    focused_once: bool,
    _subs: Vec<Subscription>,

    // — 播放 —
    /// 当前播放项在 `view` 里的下标
    current: Option<usize>,
    player: Option<Player>,
    info: Option<MediaInfo>,
    /// 正在播/正在看的绝对路径，用来给列表打高亮
    active_path: Option<String>,
    stage: Stage,

    // — 控制条 —
    /// 0.0 – 1.0 的归一化位置；这样不用因为时长变化去改滑块的 max
    seek: Entity<SliderState>,
    volume: Entity<SliderState>,
    scrubbing: bool,
    last_scrub: Instant,
    speed: f64,
    loop_mode: LoopMode,
    muted: bool,
    /// 静音前的音量，取消静音时恢复
    volume_level: f32,
    dark: bool,
    sidebar: bool,
    /// 系统里媒体文件现在是不是归本应用打开（右上角链条图标据此点亮）
    is_default_player: bool,
    /// 启动后第一次 tick 去问一次系统（`App::new` 里没有 `Context` 发 toast）
    check_default_once: bool,
    /// 舞台上是否叠一层媒体信息
    show_info: bool,
    /// 当前播的是不是动画 GIF（这种文件天生就该循环）
    looping_gif: bool,
    /// 用户手动调的画面朝向（旋转 / 翻转）。换文件时复位。
    orient: Orientation,
    /// 静态图片**未经朝向变换**的原始纹理。用户调角度时从它重算，
    /// 而不是在已经转过一次的图上再转（那样会累积误差）。
    still_base: Option<Arc<RenderImage>>,
    /// "画面角度"面板是否展开
    orient_panel: bool,
    /// 窗口是否全屏 / 是否置顶
    fullscreen: bool,
    pinned: bool,
    /// 刚点完全屏的时刻。动画期间不去读系统状态，免得来回打架。
    fs_settle: Option<Instant>,
    /// 导出面板是否展开
    export_panel: bool,
    /// 正在跑的导出任务（同一时刻只允许一个）
    export: Option<Export>,
    /// 已经弹出、正等用户选路径的保存面板。**非阻塞**，靠帧循环轮询。
    dialog: Option<PendingSave>,

    // — 字幕 —
    /// 当前载入的字幕（换文件时清掉，再找同名 sidecar）
    subs: Option<Subtitles>,
    /// 字幕是否显示（用户要求：可以隐藏字幕）。换字幕时重新打开。
    sub_visible: bool,
    /// 字幕面板是否展开
    sub_panel: bool,
    /// 字号档位，索引进 [`SUB_SCALES`]
    sub_size: usize,
    /// 已经弹出、正等用户选字幕文件的非阻塞面板
    sub_dialog: Option<PendingOpen>,
    /// 三个图标弹框**最近一次有人碰**的时刻（开面板 / 点鼠标 / 按键都会刷新）。
    /// 只有真有面板停靠着时才有意义；`tick` 拿它判 [`PANEL_TTL`] 超时自动收起。
    panel_seen: Option<Instant>,
    toast: Option<(String, Instant)>,
    warmup: u8,
    /// 空舞台中央的品牌 logo（编译期嵌进二进制，打包不用带 assets/）
    logo: Option<Arc<RenderImage>>,
    /// 音频舞台的“均衡器”可视化状态
    viz: viz::Viz,
    /// 上一帧 viz.step 的时间，用来算 dt
    viz_last: Option<Instant>,
}

/// 一个已经弹出、正等用户选路径的保存面板。
///
/// **必须用 rfd 的异步版**（底层是 `beginSheetModalForWindow:completionHandler:`，
/// 一块挂在窗口上的 sheet），绝不碰同步的 `save_file()`：后者内部是 `runModal`，
/// 会在主线程上再套一层事件循环。而 GPUI 的帧源恰好是一块挂在**主队列**上的
/// dispatch source —— 模态期间照样触发，于是帧回调重入 GPUI 直接 panic；
/// 偏偏这个回调站在 `extern "C"` 边界上没法 unwind，进程当场 abort
/// （崩溃日志里只有一句 "panic in a function that cannot unwind"）。
/// rfd 非阻塞面板返回的 future（`save_file` / `pick_file` 是同一个类型）。
type DialogFuture = Pin<Box<dyn Future<Output = Option<rfd::FileHandle>>>>;

struct PendingSave {
    kind: export::Kind,
    src: PathBuf,
    /// 弹面板那一刻冻结的导出起点与总时长
    pos: f64,
    dur: f64,
    fut: DialogFuture,
}

/// 已经弹出、正等用户**挑一个文件**的打开面板（导入字幕用）。
/// 和 [`PendingSave`] 同一套理由：绝不能碰同步的 `pick_file()`。
struct PendingOpen {
    fut: DialogFuture,
}

impl PendingOpen {
    /// `None` = 面板还开着；`Some(Some(p))` = 选了 `p`；`Some(None)` = 用户取消。
    fn poll(&mut self) -> Option<Option<PathBuf>> {
        let mut cx = TaskContext::from_waker(std::task::Waker::noop());
        match self.fut.as_mut().poll(&mut cx) {
            Poll::Pending => None,
            Poll::Ready(picked) => Some(picked.map(|h| PathBuf::from(h.path()))),
        }
    }
}

impl PendingSave {
    /// 轮询一次：`None` = 面板还开着；`Some(Some(p))` = 选了 `p`；
    /// `Some(None)` = 用户取消。
    fn poll(&mut self) -> Option<Option<PathBuf>> {
        let mut cx = TaskContext::from_waker(std::task::Waker::noop());
        match self.fut.as_mut().poll(&mut cx) {
            Poll::Pending => None,
            Poll::Ready(picked) => Some(picked.map(|h| PathBuf::from(h.path()))),
        }
    }
}

/// 列表行首的类型图标。
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "video" => "film",
        "audio" => "music",
        "image" => "image",
        _ => "image",
    }
}

fn kind_cn(kind: &str) -> &'static str {
    match kind {
        "video" => "视频",
        "audio" => "音频",
        "image" => "图片",
        _ => "文件",
    }
}

impl App {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 红绿灯的"关闭"= **藏起来**，不是销毁：窗口没了 App 状态（播放列表、
        // 进度）就跟着没了，Dock 里也没法"回到主界面"。返回 false 拦下关闭，
        // 再自己 orderOut；Dock 图标点击（on_reopen）/ Dock 菜单「显示主界面」
        // 都能把它 orderFront 回来。真正退出走右上角的 X（cx.quit）。
        window.on_window_should_close(cx, |window, _cx| {
            native::hide_window(window);
            false
        });

        // 滑块 hover 时的描边环取的是组件主题的 ring 色 —— 在控制条上同样是
        // 一圈灰晕（用户视为"阴影"），直接全局关掉。搜索框的聚焦环也走这个
        // 颜色，输入框本身是无边框样式，关掉无碍。
        gpui_kit::component::Theme::global_mut(cx).ring = gpui_kit::Hsla::transparent_black();

        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("筛选文件名…")
                .clean_on_escape()
        });
        let sub_search = cx.subscribe_in(&search, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                this.refresh_view(cx);
            }
        });

        // 进度条按 0–1 归一化（`min`/`max` 只能在构造时链式设，没有 setter）。
        // 注意 `step` 默认是 1.0 —— 归一化后每次指针交互都会被 `(v/step).round()*step`
        // 吸附到 0 或 1，表现就是「进度条拖不动」。给一个比像素还细的步长。
        let seek = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(1.0)
                .step(0.0001)
                .default_value(0.0)
        });
        let sub_seek = cx.subscribe_in(&seek, window, |this, _, event, _, cx| match event {
            SliderEvent::Change(v) => {
                this.scrubbing = true;
                // 拖动过程中节流：每 90ms 只放一次「预览 seek」（只重开视频，
                // 不动音频），画面始终留着上一帧，所以不会黑屏闪。
                // 松手（Release）时才是精确的完整 seek。
                if this.last_scrub.elapsed() >= SCRUB_INTERVAL {
                    this.last_scrub = Instant::now();
                    this.scrub_to_ratio(v.start() as f64, cx);
                } else {
                    cx.notify();
                }
            }
            SliderEvent::Release(v) => {
                this.scrubbing = false;
                this.seek_to_ratio(v.start() as f64, cx);
            }
        });

        let volume = cx.new(|_| SliderState::new().min(0.0).max(100.0).default_value(100.0));
        let sub_vol = cx.subscribe_in(&volume, window, |this, _, event, _, cx| {
            let (SliderEvent::Change(v) | SliderEvent::Release(v)) = event;
            this.set_volume(v.start() / 100.0, cx);
        });

        Self {
            folder: None,
            recursive: false,
            files: Vec::new(),
            view: Vec::new(),
            filter: Filter::All,
            search,
            focus: cx.focus_handle(),
            focused_once: false,
            _subs: vec![sub_search, sub_seek, sub_vol],
            current: None,
            player: None,
            info: None,
            active_path: None,
            stage: Stage::Empty,
            seek,
            volume,
            scrubbing: false,
            last_scrub: Instant::now() - SCRUB_INTERVAL,
            speed: 1.0,
            loop_mode: LoopMode::Off,
            muted: false,
            volume_level: 1.0,
            dark: true,
            sidebar: true,
            is_default_player: false,
            check_default_once: true,
            // 舞台上是否叠一层媒体信息（默认关；纯音频舞台永不显示）
            show_info: false,
            looping_gif: false,
            orient: Orientation::IDENTITY,
            still_base: None,
            orient_panel: false,
            fullscreen: false,
            pinned: false,
            fs_settle: None,
            export_panel: false,
            export: None,
            dialog: None,
            subs: None,
            sub_visible: true,
            sub_panel: false,
            sub_size: 1,
            sub_dialog: None,
            panel_seen: None,
            toast: None,
            warmup: 0,
            logo: load_logo(),
            viz: viz::Viz::new(),
            viz_last: None,
        }
    }

    fn pal(&self) -> &'static Palette {
        if self.dark { &theme::DARK } else { &theme::LIGHT }
    }

    /// 命令行给了一个媒体文件/文件夹时的入口。
    pub fn load_initial(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.load_path(path, cx);
    }

    fn toast(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.toast = Some((msg.into(), Instant::now()));
        cx.notify();
    }

    // ── 文件 ────────────────────────────────────────────────────────────

    fn load_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        match media::scan_dir(&dir, self.recursive) {
            Ok(list) => {
                let n = list.len();
                self.folder = Some(dir);
                self.files = list;
                self.refresh_view(cx);
                if n == 0 {
                    self.toast("这个文件夹里没有媒体文件", cx);
                } else {
                    self.toast(format!("已载入 {n} 个文件"), cx);
                }
            }
            Err(e) => self.toast(e, cx),
        }
    }

    /// 从对话框 / 拖放 / 命令行拿到的一个路径：目录就载入，
    /// 文件就顺带把同目录的列表也读出来，这样"下一个"才有得可切。
    fn load_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.load_dir(path, cx);
            return;
        }
        let recursive = self.recursive;
        if let Some(dir) = path.parent() {
            if self.folder.as_deref() != Some(dir) {
                if let Ok(list) = media::scan_dir(dir, recursive) {
                    self.folder = Some(dir.to_path_buf());
                    self.files = list;
                    self.refresh_view(cx);
                }
            }
        }
        self.open_file(path, cx);
    }

    /// 清空左侧列表。只丢列表本身 —— 正在播的画面不动，跟原版一致；
    /// 载入的文件夹也一并忘掉，否则「重新扫描」会把手刚清掉的列表又捡回来。
    fn clear_list(&mut self, cx: &mut Context<Self>) {
        if self.files.is_empty() {
            return;
        }
        let n = self.files.len();
        self.files.clear();
        self.view.clear();
        self.folder = None;
        // 索引已经失效：列表空着，current 指向哪都没有意义
        self.current = None;
        self.toast(format!("已清空列表（{n} 项）"), cx);
    }

    /// 清掉搜索框里的关键字。`set_value` 刻意不发 Change 事件（免得外部
    /// 设值时被自己的订阅回调打回来），所以列表刷新得自己来。
    fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.read(cx).value().is_empty() {
            return;
        }
        self.search
            .update(cx, |st, cx| st.set_value("", window, cx));
        self.refresh_view(cx);
    }

    fn refresh_view(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).value().to_lowercase();
        let filter = self.filter;
        self.view = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| filter.matches(&f.kind))
            .filter(|(_, f)| q.is_empty() || f.name.to_lowercase().contains(q.as_str()))
            .map(|(i, _)| i)
            .collect();

        // 筛选把当前项挤出去了，但播放要继续 —— 只是高亮没了
        cx.notify();
    }

    // ── 播放 ────────────────────────────────────────────────────────────

    /// 关掉当前播放：这一步必须把所有解码资源交出去。
    /// 切换失败时的统一收口：清空舞台、摘掉高亮、把原因告诉用户。
    fn fail(&mut self, msg: impl Into<String>, cx: &mut Context<Self>) {
        self.stage = Stage::Empty;
        self.active_path = None;
        self.toast(msg, cx);
    }

    fn release(&mut self) {
        self.player = None;
        self.info = None;
        self.still_base = None;
        // 字幕是"针对这一个文件"的：换文件 / 停止时先清掉，
        // `open_file` 随后会去找同名 sidecar（`movie.mp4` → `movie.srt`）。
        self.subs = None;
        // 换文件 / 停止：均衡器的柱子和音符一并清场
        self.viz.reset();
        self.viz_last = None;
    }

    fn open_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.release();
        // 手调的画面角度是"针对这一个文件"的，换文件就复位；
        // 不然打开一部正常的片子却歪着，比要重调一次更烦人。
        self.orient = Orientation::IDENTITY;

        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let path_str = path.to_string_lossy().to_string();

        // 直接打开一个文件（对话框 / 命令行 / 拖放）时，列表并不知道"当前是第几个"。
        // 这里补上，否则播完接下一集的逻辑就无从谈起。
        self.current = self
            .view
            .iter()
            .position(|&fi| self.files[fi].path == path_str);

        // 图片：静态的直接解码成纹理。动画 GIF 例外 —— 让它掉下去走播放器，
        // 这样进度条 / 暂停 / 倍速全都白捡，也不用把几十帧一次性塞进内存。
        self.looping_gif = false;
        if media::kind_of(&ext) == Some("image") {
            let animated =
                ext == "gif" && media::gif_info(&path).is_some_and(|g| g.is_animated());
            if !animated {
                match crate::player::load_image(&path) {
                    Ok(image) => {
                        // 原图存一份；用户调角度时从它重算，而不是在转过的图上再转
                        self.stage = Stage::Image(image.clone());
                        self.still_base = Some(image);
                        self.active_path = Some(path_str);
                        self.warmup = 0;
                    }
                    Err(e) => self.fail(e, cx),
                }
                cx.notify();
                return;
            }
            self.looping_gif = true;
        }

        let info = match media::probe(&path) {
            Ok(i) => i,
            Err(e) => return self.fail(e, cx),
        };

        let has_video = info.has_video;

        match Player::open(info.clone()) {
            Ok(mut p) => {
                p.set_volume(self.volume_value());
                p.set_speed(self.speed);
                p.play();
                if let Some(note) = &p.audio_note {
                    self.toast(format!("音频不可用：{note}"), cx);
                }
                self.player = Some(p);
                self.info = Some(info);
                self.active_path = Some(path_str);
                self.stage = if has_video {
                    Stage::Empty
                } else {
                    Stage::Audio
                };
                self.auto_load_sidecar(&path, cx);
                // 画面的摆放统一在 render_stage 里写死为 Contain：
                // 播放区尺寸不随视频变，画面按原始比例自适应缩放进去，
                // 富裕空间留黑边 —— 绝不再拉伸变形（用户明确要求）。
                self.warmup = WARMUP_FRAMES;
                cx.notify();
            }
            Err(e) => self.fail(e, cx),
        }
    }

    fn play_view_index(&mut self, view_index: usize, cx: &mut Context<Self>) {
        let Some(&file_index) = self.view.get(view_index) else {
            return;
        };
        let path = PathBuf::from(&self.files[file_index].path);
        self.current = Some(view_index);
        self.open_file(path, cx);
    }

    fn stop(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.release();
        self.stage = Stage::Empty;
        self.active_path = None;
        self.current = None;
        self.looping_gif = false;
        self.set_seek_ratio(0.0, window, cx);
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.view.len();
        if n == 0 {
            return;
        }
        let cur = self.current.unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(n as isize) as usize;
        self.play_view_index(next, cx);
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.player {
            p.toggle();
            self.warmup = WARMUP_FRAMES;
            cx.notify();
        }
    }

    fn seek_to(&mut self, pos: f64, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.player {
            p.seek(pos);
            self.warmup = WARMUP_FRAMES;
            cx.notify();
        }
    }

    /// 拖动进度条过程中的轻量 seek：只重开视频解码，不碰音频。
    fn scrub_to(&mut self, pos: f64, cx: &mut Context<Self>) {
        if let Some(p) = &mut self.player {
            p.scrub(pos);
            self.warmup = WARMUP_FRAMES;
            cx.notify();
        }
    }

    fn seek_to_ratio(&mut self, ratio: f64, cx: &mut Context<Self>) {
        let Some(p) = &self.player else { return };
        let dur = p.duration();
        if dur > 0.0 {
            self.seek_to((ratio.clamp(0.0, 1.0)) * dur, cx);
        }
    }

    fn scrub_to_ratio(&mut self, ratio: f64, cx: &mut Context<Self>) {
        let Some(p) = &self.player else { return };
        let dur = p.duration();
        if dur > 0.0 {
            self.scrub_to((ratio.clamp(0.0, 1.0)) * dur, cx);
        }
    }

    fn set_seek_ratio(&mut self, ratio: f32, window: &mut Window, cx: &mut Context<Self>) {
        self.seek
            .update(cx, |st, cx| st.set_value(ratio, window, cx));
    }

    fn nudge(&mut self, secs: f64, cx: &mut Context<Self>) {
        if let Some(p) = &self.player {
            let target = (p.position() + secs).clamp(0.0, p.duration().max(0.0));
            self.seek_to(target, cx);
        }
    }

    /// 真正送到播放器的音量：静音时是 0，否则是上次记住的音量。
    fn volume_value(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume_level }
    }

    fn set_volume(&mut self, v: f32, cx: &mut Context<Self>) {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.001 {
            self.muted = true;
        } else {
            self.muted = false;
            self.volume_level = v;
        }
        let applied = self.volume_value();
        if let Some(p) = &mut self.player {
            p.set_volume(applied);
        }
        cx.notify();
    }

    fn toggle_mute(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.muted = !self.muted;
        let applied = self.volume_value();
        if let Some(p) = &mut self.player {
            p.set_volume(applied);
        }
        // 滑块要跟着回到 0 / 原值；按钮的划线图标就是状态，不用提示再复述一遍
        self.volume
            .update(cx, |st, cx| st.set_value(applied * 100.0, window, cx));
        cx.notify();
    }

    fn set_speed(&mut self, speed: f64, cx: &mut Context<Self>) {
        self.speed = speed;
        if let Some(p) = &mut self.player {
            p.set_speed(speed);
        }
        self.warmup = WARMUP_FRAMES;
        cx.notify();
    }

    // ── 窗口 ────────────────────────────────────────────────────────────

    fn toggle_fullscreen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if native::toggle_fullscreen(window) {
            // 全屏切换有动画，styleMask 要到动画结束才更新，
            // 所以先按"点了就变了"记着，等动画走完再跟系统对齐。
            self.fullscreen = !self.fullscreen;
            self.fs_settle = Some(Instant::now());
        } else {
            self.toast("当前平台不支持全屏切换", cx);
        }
        cx.notify();
    }

    fn toggle_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = !self.pinned;
        // 成功时不再弹"窗口已置顶"——按钮自身的亮灭就是状态，别用提示复述一遍
        if native::set_always_on_top(window, next) {
            self.pinned = next;
        } else {
            self.toast("当前平台不支持窗口置顶", cx);
        }
        cx.notify();
    }

    /// 控制条最右端那颗「最大化」：点一下最大化，再点一下还原。
    ///
    /// **实现上必须借 GPUI 的 `Window::zoom_window()`**，不能自己顺着原生窗口
    /// 句柄去同步调 AppKit 的 `zoom:` —— 同步调用的那一刻我们正处在 GPUI 的
    /// 事件分发里（App 借用中），AppKit 立刻改窗口大小并回调 `set_frame_size`
    /// → GPUI 想 `handle.update(…)` 做 `bounds_changed`，结果被 `.log_err()`
    /// 悄悄吞掉，布局就停在旧尺寸上。表现就是用户报的
    /// 「窗口变大了，但里面的播放区还是小的」。
    /// GPUI 的这条路径内部把原生调用丢到前台执行器上稍后执行，回调回来时
    /// 已经没人借用，布局才跟得上。
    ///
    /// 这条是**三个平台通用**的：macOS 走 `zoom:`、Linux 走窗口管理器最大化、
    /// Windows 走 `SW_MAXIMIZE`（只有无头测试平台是 `unimplemented!()`）。
    fn toggle_maximize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.zoom_window();
        cx.notify();
    }

    /// 「最大化」按钮的图标：没最大化 = 放大框，已最大化 = 还原的双层框。
    ///
    /// 状态直接读窗口的真实状态位（`native::is_window_zoomed`，即 AppKit 的
    /// `isZoomed`），不自己存一份 —— 这样点绿色交通灯最大化之后按钮图标
    /// 也会跟着变，不会出现"按钮说没最大化、窗口却是大的"。
    fn maximize_glyph(zoomed: bool) -> &'static str {
        if zoomed { "restore" } else { "maximize" }
    }

    // ── 画面角度 ────────────────────────────────────────────────────────

    /// 舞台上此刻有没有"画面"可以调 —— 视频/动图靠播放器，静态图片靠 `still_base`。
    fn has_picture(&self) -> bool {
        self.still_base.is_some() || self.player.as_ref().is_some_and(|p| p.info.has_video)
    }

    fn toggle_orient_panel(&mut self, cx: &mut Context<Self>) {
        self.orient_panel = !self.orient_panel;
        if self.orient_panel {
            // 浮层都占右下角，别叠在一起
            self.export_panel = false;
            self.sub_panel = false;
        }
        self.arm_panel_timer();
        cx.notify();
    }

    // ── 图标弹框的停靠超时 ──────────────────────────────────────────────

    /// 现在有没有弹框停靠着。
    fn panel_open(&self) -> bool {
        self.orient_panel || self.export_panel || self.sub_panel
    }

    /// 开 / 关面板之后重新上表：还有面板停靠就从此刻重新计时，全关了就把表停下。
    fn arm_panel_timer(&mut self) {
        self.panel_seen = self.panel_open().then(Instant::now);
    }

    /// 有任何鼠标 / 键盘动作就叫一声，把「最近活动时刻」推到当前。
    ///
    /// 没有面板停靠时直接返回 —— 免得每次鼠标移动都往 App 里写一下。
    fn note_activity(&mut self) {
        if self.panel_open() {
            self.panel_seen = Some(Instant::now());
        }
    }

    /// 把三个弹框全收起来（停靠超时走这里；Esc 也复用）。
    fn close_panels(&mut self, cx: &mut Context<Self>) {
        self.orient_panel = false;
        self.export_panel = false;
        self.sub_panel = false;
        self.panel_seen = None;
        cx.notify();
    }

    /// 换一个画面朝向：视频重开一次解码（滤镜链带上新角度），
    /// 静态图片在 CPU 上重算一遍纹理。位置、播放状态都不受影响。
    fn apply_orientation(&mut self, orient: Orientation, cx: &mut Context<Self>) {
        if !self.has_picture() {
            self.toast("先打开一个文件再调画面角度", cx);
            return;
        }
        if self.orient.same_effect(orient) {
            self.orient = orient;
        } else {
            self.orient = orient;
            if let Some(p) = &mut self.player {
                p.set_orientation(orient);
            }
            if let Some(base) = &self.still_base {
                let base = base.clone();
                if let Some(img) = crate::player::orient_image(&base, orient) {
                    self.stage = Stage::Image(img);
                }
            }
        }
        let note = self.orient.label();
        let msg = if self.orient.is_identity() {
            "画面角度：已复位".to_string()
        } else {
            format!("画面角度：{note}")
        };
        self.toast(msg, cx);
        cx.notify();
    }

    // ── 导出 ────────────────────────────────────────────────────────────

    fn toggle_export_panel(&mut self, cx: &mut Context<Self>) {
        self.export_panel = !self.export_panel;
        if self.export_panel {
            self.orient_panel = false;
            self.sub_panel = false;
        }
        self.arm_panel_timer();
        cx.notify();
    }

    /// 点了某个导出入口：当场把**非阻塞**的保存面板挂到窗口上。
    ///
    /// 面板一挂就返回，事件分发立刻退出，绝不会把主线程塞进嵌套事件循环 ——
    /// 这是修掉"点截图 / 转 GIF 就崩"的关键（原因见 [`PendingSave`]）。
    fn request_export(&mut self, kind: export::Kind, window: &mut Window, cx: &mut Context<Self>) {
        if self.export.is_some() {
            self.toast("上一个导出还没结束", cx);
            return;
        }
        if self.dialog.is_some() {
            self.toast("先处理完弹出的保存面板", cx);
            return;
        }
        let Some(src) = self.active_path.clone().map(PathBuf::from) else {
            self.toast("先打开一个媒体文件", cx);
            return;
        };
        if self.player.is_none() {
            // 静态图片没有时间轴，导出这一套用不上
            self.toast("图片直接复制就行，导出只对音视频有效", cx);
            return;
        }

        let pos = self.player.as_ref().map(|p| p.position()).unwrap_or(0.0);
        let dur = self.player.as_ref().map(|p| p.duration()).unwrap_or(0.0);
        let has_audio = self.info.as_ref().is_some_and(|i| i.has_audio);

        if kind == export::Kind::Audio && !has_audio {
            self.toast("这个文件没有音轨", cx);
            return;
        }
        if kind == export::Kind::Gif && dur > 0.0 && dur - pos < 0.5 {
            self.toast("已经到结尾了，先把进度往前拖一点", cx);
            return;
        }

        let tag =
            (kind == export::Kind::Snapshot).then(|| export::safe_tag(&media::format_time(pos)));
        let suggested = export::default_dest(&src, kind, tag.as_deref());
        let (name, exts) = kind.filter();

        // rfd 的"异步"面板在它认为环境不支持时会**静默退回同步 `runModal`**，
        // 那等于在主线程里再开一层事件循环 → GPUI 被重入 → panic → abort。
        // 这就是"点截图 / 转 GIF / 提取音频就崩"的根因，所以先自己确认：
        // 走不了非阻塞 sheet 就干脆不开面板，绝不能让它掉进同步回退。
        if !crate::native::async_sheet_available(window) {
            self.toast("当前环境打不开非阻塞保存面板，这次先不导出了", cx);
            return;
        }

        // 再兜一层：万一 rfd 内部还是 panic（比如它自己的环境判断和上面不一致），
        // 也要变成一句提示，而不是把进程带走。
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rfd::AsyncFileDialog::new()
                .set_directory(suggested.parent().unwrap_or(&src))
                .set_file_name(
                    suggested
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default(),
                )
                .add_filter(name, exts)
                // 一定要显式给父窗口：rfd 找不到窗口同样会退回同步 `runModal`。
                .set_parent(&*window)
                .save_file()
        }));
        let fut = match opened {
            Ok(fut) => fut,
            Err(payload) => {
                let why = panic_reason(&payload);
                self.toast(format!("打不开保存面板：{why}"), cx);
                return;
            }
        };

        self.dialog = Some(PendingSave {
            kind,
            src,
            pos,
            dur,
            fut: Box::pin(fut),
        });
        // 面板本身不阻塞，结果靠帧循环每帧轮询。
        //
        // ⚠️ 这里**不能**顺手加一句"要下一帧"：那个 API（见 tick 里唯一的那处调用）
        // 内部要 `self.current_view()`（= `rendered_entity_stack.last().unwrap()`），
        // 只在渲染 / prepaint 里成立；而本函数是**点击回调**，栈是空的 →
        // `Option::unwrap()` on None 直接 panic，而点击是从 ObjC 的
        // `extern "C"` 进来的，panic 不能 unwind → abort，日志里只剩
        // "panic in a function that cannot unwind"。
        // 用户报的"点截图 / 转 GIF / 提取音频就崩"最后就卡在这一句上。
        // `cx.notify()` 安排一次重绘，`tick()` 在渲染里再把帧循环续起来。
        cx.notify();
    }

    /// 用户在保存面板里点了"存储"：把 ffmpeg 任务丢给后台线程。
    #[allow(clippy::too_many_arguments)]
    fn begin_export(
        &mut self,
        kind: export::Kind,
        src: PathBuf,
        pos: f64,
        dur: f64,
        dest: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let job = match kind {
            export::Kind::Snapshot => Export::snapshot(&src, pos, dest),
            export::Kind::Audio => Export::audio(&src, dest, dur),
            export::Kind::Gif => {
                let take = if dur > 0.0 {
                    export::GIF_SECONDS.min(dur - pos).max(0.2)
                } else {
                    export::GIF_SECONDS
                };
                Export::gif(&src, pos, take, dest)
            }
        };

        match job {
            Ok(job) => {
                self.export = Some(job);
                self.export_panel = true;
                // 起来之后帧循环得继续转，不然进度条不会动
                window.request_animation_frame();
                cx.notify();
            }
            Err(e) => self.toast(e, cx),
        }
    }

    fn cancel_export(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.export {
            job.cancel();
        }
        self.toast("正在取消…", cx);
    }

    // ── 字幕 ────────────────────────────────────────────────────────────

    /// 打开媒体文件时顺手找同名 sidecar 字幕（`movie.mp4` → `movie.srt`）。
    /// 播放器都这么干，省得每次手动导入一遍；找到了就明说一句，别偷偷加东西。
    fn auto_load_sidecar(&mut self, media: &PathBuf, cx: &mut Context<Self>) {
        let Some(path) = subtitle::sidecar(media) else {
            return;
        };
        match subtitle::load(&path) {
            Ok(s) => {
                let (n, name) = (s.len(), s.name.clone());
                self.subs = Some(s);
                self.sub_visible = true;
                self.toast(format!("已自动载入同名字幕 {name}（{n} 条）"), cx);
            }
            Err(e) => self.toast(format!("同名字幕载入失败：{e}"), cx),
        }
    }

    fn toggle_sub_panel(&mut self, cx: &mut Context<Self>) {
        self.sub_panel = !self.sub_panel;
        if self.sub_panel {
            // 三个面板都占舞台右下角，一次只开一个
            self.export_panel = false;
            self.orient_panel = false;
        }
        self.arm_panel_timer();
        cx.notify();
    }

    /// 面板里点「导入字幕…」：当场挂上**非阻塞**的打开面板。
    ///
    /// 与导出同一套理由（见 [`PendingSave`]）：同步的 `pick_file()` 内部是
    /// `runModal`，会把主队列上的帧源重入 → panic → abort。所以先自查环境、
    /// 再 `catch_unwind` 兜底，无论如何点击的结果只能是"弹面板"或"给提示"。
    fn request_import_subtitle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sub_dialog.is_some() {
            self.toast("先处理完弹出的字幕面板", cx);
            return;
        }
        if !crate::native::async_sheet_available(window) {
            self.toast("当前环境打不开非阻塞文件面板，这次先不导入了", cx);
            return;
        }
        // 默认落在当前媒体所在目录，找同名字幕最省事
        let start = self
            .active_path
            .as_deref()
            .map(PathBuf::from)
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .filter(|d| d.is_dir());
        let parent = &*window;
        let opened = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut dlg = rfd::AsyncFileDialog::new().add_filter("字幕文件", subtitle::EXTENSIONS);
            if let Some(dir) = &start {
                dlg = dlg.set_directory(dir);
            }
            // 一定要显式给父窗口：rfd 找不到窗口同样会退回同步 `runModal`
            dlg.set_parent(parent).pick_file()
        }));
        let fut = match opened {
            Ok(fut) => fut,
            Err(payload) => {
                let why = panic_reason(&payload);
                self.toast(format!("打不开文件面板：{why}"), cx);
                return;
            }
        };
        self.sub_dialog = Some(PendingOpen { fut: Box::pin(fut) });
        // 面板不阻塞，结果靠帧循环每帧轮询（同导出面板）
        cx.notify();
    }

    /// 选好了字幕文件：解析进内存，并顺手打开显示。
    fn load_subtitle(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match subtitle::load(&path) {
            Ok(s) => {
                let (n, name, fmt) = (s.len(), s.name.clone(), s.format.label());
                self.subs = Some(s);
                self.sub_visible = true;
                self.toast(format!("已载入字幕 {name}（{fmt} · {n} 条）"), cx);
            }
            Err(e) => self.toast(e, cx),
        }
    }

    /// 显示 / 隐藏字幕（面板里那一行，快捷键 `S` 也走这里）。
    fn toggle_subtitles(&mut self, cx: &mut Context<Self>) {
        if self.subs.is_none() {
            self.toast("还没有字幕，控制条「字幕」按钮里可以导入", cx);
            return;
        }
        self.sub_visible = !self.sub_visible;
        let msg = if self.sub_visible {
            "字幕：显示"
        } else {
            "字幕：隐藏"
        };
        self.toast(msg, cx);
    }

    fn remove_subtitle(&mut self, cx: &mut Context<Self>) {
        if self.subs.take().is_none() {
            self.toast("当前没有字幕", cx);
            return;
        }
        self.toast("已移除字幕", cx);
    }

    /// 字号在小 → 中 → 大之间转圈。
    fn cycle_sub_size(&mut self, cx: &mut Context<Self>) {
        self.sub_size = (self.sub_size + 1) % SUB_SCALES.len();
        let label = SUB_SIZE_LABELS[self.sub_size];
        self.toast(format!("字幕大小：{label}"), cx);
    }

    fn sub_scale(&self) -> f32 {
        SUB_SCALES[self.sub_size.min(SUB_SCALES.len() - 1)]
    }

    /// 把 iPlayer 设成媒体文件的默认打开方式（访达"全部更改"背后的那个 API）。
    fn set_default_player(&mut self, cx: &mut Context<Self>) {
        match native::set_default_role_handler(&media::all_utis()) {
            Ok(n) => {
                self.is_default_player = true;
                self.toast(
                    format!("已把 iPlayer 设为默认播放器（{n} 类文件）——双击媒体文件就会用它打开"),
                    cx,
                );
            }
            Err(e) => self.toast(format!("设置失败：{e}"), cx),
        }
    }

    /// 启动时查一次"系统里媒体文件归谁打开"：不是我们就顺手提示怎么改。
    /// 已经是默认、或根本没在 .app 里跑（拿不到 bundle id）就什么都不做。
    fn check_default_player(&mut self, cx: &mut Context<Self>) {
        self.is_default_player = false;
        let (Some(own), Some(current)) = (
            native::own_bundle_id(),
            native::default_handler_bundle_id("public.movie"),
        ) else {
            return;
        };
        self.is_default_player = current == own;
        if !self.is_default_player {
            self.toast("按 ⌘D 可以把 iPlayer 设为默认播放器", cx);
        }
    }

    /// 每帧推进：更新进度、处理播完、驱动下一帧。
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focused_once {
            self.focused_once = true;
            window.focus(&self.focus, cx);
        }

        // 系统让本应用打开的文件（访达双击 / 右键"打开方式" / 拖到 Dock 图标）。
        // 它**不走 argv**，是 native 那边从 Apple Event 里收进队列的，这里每帧捞一次。
        // 放在最前面：不能被后面任何一个提前 return 跳过。
        if let Some(path) = native::take_open_doc() {
            let extra = std::iter::from_fn(native::take_open_doc).count();
            self.load_path(path, cx);
            if extra > 0 {
                self.toast(
                    format!("已打开选中的第一个文件（同批还有 {extra} 个，都在左侧列表里）"),
                    cx,
                );
            }
        }

        // 第一帧问一次系统：媒体文件现在归谁打开
        if self.check_default_once {
            self.check_default_once = false;
            self.check_default_player(cx);
        }

        if let Some((_, at)) = &self.toast {
            if at.elapsed() > TOAST_TTL {
                self.toast = None;
            }
        }

        // 图标弹框停靠超时：鼠标键盘都停住 `PANEL_TTL` 就自己收起来
        //（用户要求：不要一直挂在右下角）。
        // 面板开着时每帧续一次帧 —— 帧循环一停就没人来看表了（暂停播放、
        // 没有媒体时尤其明显）。真正的"有没有人碰"由 `note_activity`
        // 在鼠标 / 键盘事件里刷新，这里只负责判时间。
        if self.panel_open() {
            let now = Instant::now();
            let seen = *self.panel_seen.get_or_insert(now);
            // 两种情况先别收：
            // ① 导出任务在跑 —— 面板是"强制显示"的（渲染条件里还有 export.is_some()），
            //    收了又开，白白空转帧循环；
            // ② 正挂着文件选择面板（保存 / 打开字幕）—— 用户此刻在跟系统 sheet 打交道，
            //    应用窗口里当然没有鼠标动作，不代表他不用了。
            let busy = self.export.is_some() || self.dialog.is_some() || self.sub_dialog.is_some();
            if !busy && now.saturating_duration_since(seen) >= PANEL_TTL {
                self.close_panels(cx);
            } else {
                window.request_animation_frame();
            }
        }

        // 保存面板（非阻塞 sheet）每帧轮询一次：用户选完就开跑，选完之前
        // 得一直要帧，否则帧循环一停结果就没人接。放在最前面，免得被后面的
        // 提前 return 跳过。
        if let Some(mut d) = self.dialog.take() {
            match d.poll() {
                None => {
                    self.dialog = Some(d);
                    window.request_animation_frame();
                }
                Some(Some(dest)) => {
                    self.begin_export(d.kind, d.src, d.pos, d.dur, dest, window, cx);
                }
                Some(None) => cx.notify(), // 用户点了取消
            }
            // 面板选择期间不算"没人用"（见下面停靠超时那一段），
            // 一选完就从此刻重新计时，别让面板"选完就立刻消失"。
            self.note_activity();
        }

        // 字幕的打开面板同理：每帧轮询一次，选完之前得一直要帧
        if let Some(mut d) = self.sub_dialog.take() {
            match d.poll() {
                None => {
                    self.sub_dialog = Some(d);
                    window.request_animation_frame();
                }
                Some(Some(path)) => self.load_subtitle(path, cx),
                Some(None) => cx.notify(),
            }
            self.note_activity();
        }

        if self.warmup > 0 {
            self.warmup -= 1;
        }

        // 全屏切换有动画，切完之前别去问系统状态，否则会自己跟自己打架
        if let Some(at) = self.fs_settle {
            if at.elapsed() < Duration::from_millis(900) {
                window.request_animation_frame();
            } else {
                self.fs_settle = None;
                let fs = native::is_fullscreen(window);
                if fs != self.fullscreen {
                    self.fullscreen = fs;
                    cx.notify();
                }
            }
        }

        // 导出任务跑在后台线程，这里只负责收租
        let finished = self
            .export
            .as_ref()
            .and_then(|job| job.poll().map(|r| (r, job.dest.clone(), job.label.clone())));
        if let Some((result, dest, label)) = finished {
            // 落到这里说明线程已经收工，drop 里的 join 是即时的
            self.export = None;
            // 任务跑着的时候面板不算"没人用"，收工后重新计时
            self.note_activity();
            let name = dest
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            match result {
                Ok(()) => self.toast(format!("{label} → {name}"), cx),
                Err(e) if e == export::CANCELLED => self.toast("已取消导出", cx),
                Err(e) => self.toast(format!("导出失败：{e}"), cx),
            }
        }

        if self.player.is_none() {
            if self.export.is_some() {
                window.request_animation_frame();
            }
            return;
        }

        let ended = self.player.as_ref().is_some_and(|p| p.ended());
        if ended {
            if self.looping_gif {
                // 动图天生就该一直转，不受循环模式影响
                self.seek_to(0.0, cx);
                if let Some(p) = &mut self.player {
                    p.play();
                }
            } else {
                match self.loop_mode {
                    LoopMode::One => {
                        self.seek_to(0.0, cx);
                        if let Some(p) = &mut self.player {
                            p.play();
                        }
                    }
                    LoopMode::All => self.step(1, cx),
                    LoopMode::Off => {
                        let has_next = self
                            .current
                            .map(|c| c + 1 < self.view.len())
                            .unwrap_or(false);
                        if has_next {
                            self.step(1, cx);
                        } else {
                            if let Some(p) = &mut self.player {
                                p.pause();
                            }
                            cx.notify();
                        }
                    }
                }
            }
        } else {
            let playing = self.player.as_ref().is_some_and(|p| p.is_playing());
            if playing && !self.scrubbing {
                if let Some(p) = &self.player {
                    let dur = p.duration();
                    if dur > 0.0 {
                        let ratio = (p.position() / dur).clamp(0.0, 1.0) as f32;
                        self.set_seek_ratio(ratio, window, cx);
                    }
                }
            }
        }

        // 只要还在播（或刚 seek 完、正等第一帧出来，或后台在编码）就继续要帧。
        // 注意：`cx.notify()` 唤不醒平台帧源，视频必须显式 request。
        // 「等第一帧」也要算进来：拖进度条时多半是暂停状态，seek 完新画面到了却没人画，
        // 就会一直停在旧画面上。
        let busy = self
            .player
            .as_ref()
            .is_some_and(|p| p.is_playing() || p.awaiting());

        // 音频舞台：推进“均衡器”可视化（真实频谱走 Tap，静默退合成）。
        // 舞台尺寸拿不到精确值就按视口扣掉侧栏/标题栏/控制条估一个 ——
        // 柱子与音符都用占比坐标，估差只影响每根柱的最小高度，可接受。
        if matches!(self.stage, Stage::Audio) && self.player.is_some() {
            let now = Instant::now();
            let dt = self
                .viz_last
                .map_or(1.0 / 60.0, |t| (now - t).as_secs_f32().min(0.05));
            self.viz_last = Some(now);
            let playing = self.player.as_ref().is_some_and(|p| p.is_playing());
            let audible = !self.muted && self.volume_value() > 0.0;
            let vs = window.viewport_size();
            let stage_w = f32::from(vs.width) - if self.sidebar { 252.0 } else { 0.0 };
            let stage_h = f32::from(vs.height) - 104.0;
            let spectrum = self.player.as_ref().and_then(|p| p.spectrum());
            let src = spectrum
                .as_ref()
                .map(|(tap, rate)| (tap.as_ref(), *rate));
            self.viz.step(dt, stage_w, stage_h, playing, audible, src);
            if playing || self.viz.busy() {
                window.request_animation_frame();
            }
        }

        if busy || self.warmup > 0 || self.export.is_some() {
            window.request_animation_frame();
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = ev.keystroke.modifiers;
        match ev.keystroke.key.as_str() {
            "space" => self.toggle(cx),
            "left" => self.nudge(-5.0, cx),
            "right" => self.nudge(5.0, cx),
            "up" => self.set_volume((self.volume_value() + 0.05).min(1.0), cx),
            "down" => self.set_volume((self.volume_value() - 0.05).max(0.0), cx),
            "[" => self.set_speed((self.speed - 0.5).max(0.5), cx),
            "]" => self.set_speed((self.speed + 0.5).min(3.0), cx),
            "m" => self.toggle_mute(window, cx),
            "escape" => {
                if self.panel_open() {
                    self.close_panels(cx);
                } else {
                    self.sidebar = !self.sidebar;
                    cx.notify();
                }
            }
            "p" if !m.control => self.step(-1, cx),
            "n" if !m.control => self.step(1, cx),
            "." if m.control => self.stop(window, cx),
            "t" if m.control => {
                self.dark = !self.dark;
                cx.notify();
            }
            "o" if m.control => self.pick_folder(cx),
            // ⌘D = 把 iPlayer 设为默认播放器（链条图标已从标题栏去掉，
            // 这里是界面里剩下的唯一入口；命令行还有 `iplayer --set-default`）
            "d" if m.control => self.set_default_player(cx),
            "f" if !m.control => self.toggle_fullscreen(window, cx),
            "t" if !m.control => self.toggle_pin(window, cx),
            // 画面比例循环（`a`）已随"裁切"按钮一起移除
            "e" if !m.control => self.toggle_export_panel(cx),
            // 字幕显示 / 隐藏（面板里也有一行，快捷键只是图快）
            "s" if !m.control => self.toggle_subtitles(cx),
            // 信息浮层按钮已从控制条移除，改由快捷键开关
            "i" if !m.control => {
                self.show_info = !self.show_info;
                cx.notify();
            }
            _ => {}
        }
    }

    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.load_dir(dir, cx);
        }
    }

    fn pick_file(&mut self, cx: &mut Context<Self>) {
        // 过滤器用 `media::DOC_TYPES` 那份清单 —— 和 Info.plist 里声明给系统的
        // 是同一个来源，改一处两边都跟着变（少一个扩展名，那种文件就既不在
        // "打开方式"里、也选不出来）。
        let exts = media::all_exts();
        let picked = rfd::FileDialog::new()
            .add_filter("媒体文件", &exts)
            .pick_file();
        if let Some(f) = picked {
            self.load_path(f, cx);
        }
    }

    // ── 绘制零件 ────────────────────────────────────────────────────────

    /// 标题栏开关按钮的图标颜色：激活时亮、平时灰。
    fn tb_color(&self, active: bool) -> gpui_kit::Hsla {
        if active {
            self.pal().text()
        } else {
            self.pal().muted()
        }
    }

    /// 一枚染色的 SVG 图标（来自内置图标集，渲染结果全局缓存）。
    fn icon_el(
        &self,
        name: &'static str,
        size: f32,
        color: gpui_kit::Hsla,
    ) -> AnyElement {
        img(ImageSource::Render(icons::render(
            name,
            size,
            &icons::hex_of(color),
        )))
        .w(px(size))
        .h(px(size))
        .flex_none()
        .into_any_element()
    }

    /// 图标按钮：定尺方框 + 居中 SVG 图标；`active` 时用激活底色高亮。
    fn icon_btn_c(
        &self,
        id: &'static str,
        icon: &'static str,
        size: f32,
        color: gpui_kit::Hsla,
        active: bool,
    ) -> Stateful<Div> {
        self.icon_btn_sz(id, icon, size, size * 0.55, color, active)
    }

    /// 图标按钮（方框与图标尺寸解耦）：播放组四个键的**图标**要一样大，
    /// 但方框可以大小不同（播放键的方框更大），所以单独留出 `icon_size`。
    fn icon_btn_sz(
        &self,
        id: &'static str,
        icon: &'static str,
        size: f32,
        icon_size: f32,
        color: gpui_kit::Hsla,
        active: bool,
    ) -> Stateful<Div> {
        let pal = self.pal();
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .w(px(size))
            .h(px(size))
            .rounded_md()
            .cursor_pointer()
            .when(active, |d| d.bg(pal.active()))
            .when(!active, |d| d.hover(|s| s.bg(pal.hover())))
            .child(self.icon_el(icon, icon_size, color))
    }

    /// 常规图标按钮：正文色图标，无激活底色（播放组那一排用）。
    fn icon_btn(
        &self,
        id: &'static str,
        icon: &'static str,
        size: f32,
    ) -> Stateful<Div> {
        let pal = self.pal();
        self.icon_btn_c(id, icon, size, pal.text(), false)
    }

    /// 落地页上的大号按钮：描边 + 图标 + 文字。
    fn stage_btn(
        &self,
        id: &'static str,
        icon: &'static str,
        label: &str,
    ) -> Stateful<Div> {
        let pal = self.pal();
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .gap(px(7.))
            .px(px(15.))
            .py(px(7.))
            .rounded_md()
            .border_1()
            .border_color(pal.line())
            .cursor_pointer()
            // 悬浮：底色加深 + 线框提亮，与侧栏按钮同一套反馈
            .hover(|s| s.bg(pal.hover()).border_color(pal.active()))
            .child(self.icon_el(icon, 14., pal.text()))
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(pal.text())
                    .child(SharedString::from(label.to_string())),
            )
    }

    /// 小圆角标签按钮，用于倍速 / 循环 / 静音这类开关。
    fn chip(&self, id: impl Into<gpui_kit::ElementId>, label: &str, active: bool) -> Stateful<Div> {
        let pal = self.pal();
        self.chip_fg(
            id,
            label,
            if active { pal.text() } else { pal.muted() },
            active,
        )
    }

    /// 不参与"选中"语义的文字 chip（倍速等）：底色透明，文字直接用正文色，
    /// 别让它像未选中的筛选 chip 一样灰着（用户反馈"1.0x 太暗"）。
    fn chip_bright(
        &self,
        id: impl Into<gpui_kit::ElementId>,
        label: &str,
    ) -> Stateful<Div> {
        self.chip_fg(id, label, self.pal().text(), false)
    }

    fn chip_fg(
        &self,
        id: impl Into<gpui_kit::ElementId>,
        label: &str,
        fg: gpui_kit::Hsla,
        active: bool,
    ) -> Stateful<Div> {
        let pal = self.pal();
        div()
            .id(id)
            .flex_none()
            .px(px(8.))
            .py(px(3.))
            .rounded_md()
            .text_size(px(11.))
            .cursor_pointer()
            .text_color(fg)
            // 两种状态悬浮都给高亮反馈，选中态再深一档
            .when(active, |d| {
                d.hover(|s| s.bg(pal.active()).text_color(pal.text()))
            })
            .when(!active, |d| {
                d.hover(|s| s.bg(pal.hover()).text_color(pal.text()))
            })
            .child(SharedString::from(label.to_string()))
    }

    /// 图标 + 文字的标签按钮（侧栏的「打开文件夹 / 打开文件」）。
    fn chip_ic(
        &self,
        id: &'static str,
        icon: &'static str,
        label: &str,
        active: bool,
    ) -> Stateful<Div> {
        let pal = self.pal();
        let (fg, ic) = if active {
            (pal.on_accent(), pal.on_accent())
        } else {
            (pal.text(), pal.text())
        };
        div()
            .id(id)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .px(px(8.))
            .py(px(3.))
            .rounded_md()
            // 淡淡的一圈线框，与空舞台中央的「打开文件」大按钮同款
            .border_1()
            .border_color(pal.line())
            .cursor_pointer()
            .when(active, |d| d.bg(pal.accent()))
            // 悬浮时底色变深、线框同步提亮一档（参考空舞台中央大按钮的高亮反馈）
            .when(!active, |d| {
                d.hover(|s| s.bg(pal.hover()).border_color(pal.active()))
            })
            .child(self.icon_el(icon, 13., ic))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(fg)
                    .child(SharedString::from(label.to_string())),
            )
    }

    /// 标题栏副标题：在播文件名 → 文件夹名 → 空闲时的固定描述。
    fn title_sub(&self) -> String {
        if let Some(p) = &self.active_path {
            if let Some(name) = std::path::Path::new(p).file_name() {
                return name.to_string_lossy().to_string();
            }
        }
        if let Some(d) = &self.folder {
            return d.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| d.display().to_string());
        }
        "视频播放器".to_string()
    }


    fn file_name(&self) -> String {
        self.active_path
            .as_deref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// 舞台上那层小字要显示的两行：文件名 + 一串规格。
    fn info_lines(&self) -> Option<(String, String)> {
        if let Stage::Image(image) = &self.stage {
            let size = image.size(0);
            let ext = self
                .active_path
                .as_deref()
                .and_then(|p| std::path::Path::new(p).extension())
                .map(|e| e.to_string_lossy().to_uppercase())
                .unwrap_or_default();
            return Some((
                self.file_name(),
                format!("图片 · {} × {} · {}", size.width.0, size.height.0, ext),
            ));
        }

        let info = self.info.as_ref()?;
        let mut parts: Vec<String> = Vec::new();
        if info.has_video {
            parts.push(format!("{}×{}", info.width, info.height));
            if info.fps > 0.0 {
                parts.push(format!("{:.2} fps", info.fps));
            }
            if !info.vcodec.is_empty() {
                parts.push(if info.pix_fmt.is_empty() {
                    info.vcodec.clone()
                } else {
                    format!("{} ({})", info.vcodec, info.pix_fmt)
                });
            }
            if info.rotate != 0 {
                parts.push(format!("旋转 {}°", info.rotate));
            }
        }
        if !info.acodec.is_empty() {
            parts.push(format!(
                "{} {}ch {} Hz",
                info.acodec, info.channels, info.sample_rate
            ));
        }
        if info.bitrate > 0 {
            parts.push(format!("{:.2} Mbps", info.bitrate as f64 / 1_000_000.0));
        }
        if info.size > 0 {
            parts.push(media::format_size(info.size));
        }
        if !info.format_name.is_empty() {
            parts.push(info.format_name.clone());
        }
        Some((self.file_name(), parts.join("  ·  ")))
    }

    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pal = self.pal();
        div()
            .id("titlebar")
            .flex()
            .items_center()
            .flex_none()
            .h(px(38.))
            .pl(px(78.)) // 让开 macOS 的红绿灯
            .pr(px(10.))
            .gap_2()
            .bg(pal.bg())
            .on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            .child(
                self.icon_btn("tb-sidebar", "panelLeft", 26.)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar = !this.sidebar;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(12.5))
                            .text_color(pal.text())
                            .child("iPlayer"),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(pal.muted())
                            .overflow_hidden()
                            .child(SharedString::from(self.title_sub())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(3.))
                    // 右上角这一排，用户指定的顺序（从左到右）：深浅色 · 置顶。
                    // 最小化 / 最大化 / 关闭三颗已按用户要求整个移除 —— 这三件事
                    // 交给 macOS 自带的红绿灯（左上角那三颗）就够了；关窗走 ⌘W。
                    // 原先挤在这里的画面角度 / 工具箱 / 全屏挪到了底部控制条，
                    // 「链条」（设默认播放器）图标整个去掉 —— 功能保留在快捷键
                    // ⌘D 和 `iplayer --set-default`。
                    .child(
                        self.icon_btn("tb-theme", if self.dark { "sun" } else { "moon" }, 26.)
                            .debug_selector(|| "tb-theme".to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.dark = !this.dark;
                                cx.notify();
                            })),
                    )
                    .child(
                        self.icon_btn_c("tb-pin", "pin", 26., self.tb_color(self.pinned), self.pinned)
                            .debug_selector(|| "tb-pin".to_string())
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_pin(window, cx))),
                    ),
            )
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pal = self.pal();
        let can_clear = !self.files.is_empty();

        let rows: Vec<AnyElement> = self
            .view
            .iter()
            .enumerate()
            .map(|(vi, &fi)| {
                let f = &self.files[fi];
                let is_active = self.active_path.as_deref() == Some(f.path.as_str());
                let name = f.name.clone();
                let meta = format!("{} · {}", kind_cn(&f.kind), media::format_size(f.size));
                div()
                    .id(("row", vi))
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap(px(2.))
                    .px(px(9.))
                    .py(px(6.))
                    .rounded_md()
                    .cursor_pointer()
                    .when(is_active, |d| d.bg(pal.active()))
                    .when(!is_active, |d| d.hover(|s| s.bg(pal.hover())))
                    .on_click(cx.listener(move |this, _, _, cx| this.play_view_index(vi, cx)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .overflow_hidden()
                            .child(self.icon_el(kind_icon(&f.kind), 13., pal.muted()))
                            .child(
                                // 文件名只占一行，放不下的用「…」收尾。
                                // min_w(px(0.)) 不能省：flex 子项默认 min-width:auto 会被
                                // 内容顶开，宽度压不下去，省略号也就没机会出现。
                                // `truncate()` = overflow_hidden + whitespace_nowrap + 省略号。
                                div()
                                    .flex_1()
                                    .min_w(px(0.))
                                    .text_size(px(12.5))
                                    .line_height(px(17.))
                                    .text_color(pal.text())
                                    .truncate()
                                    .debug_selector(move || format!("row-name-{vi}"))
                                    .child(SharedString::from(name)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(pal.muted())
                            .child(SharedString::from(meta)),
                    )
                    .into_any_element()
            })
            .collect();

        let empty = self.view.is_empty();
        let has_query = !self.search.read(cx).value().is_empty();
        let filters = Filter::ALL.map(|f| {
            let active = self.filter == f;
            self.chip(("chip", f as usize), f.label(), active)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.filter = f;
                    this.refresh_view(cx);
                }))
        });
        let recursive = self.recursive;
        let deep_chip = self.chip("deep", "含子目录", recursive).on_click(
            cx.listener(|this, _, _, cx| {
                this.recursive = !this.recursive;
                if let Some(dir) = this.folder.clone() {
                    this.load_dir(dir, cx);
                } else {
                    cx.notify();
                }
            }),
        );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .w(px(252.))
            .h_full()
            .bg(pal.panel())
            .border_r_1()
            .border_color(pal.line())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .p(px(10.))
                    .child(
                        self.chip_ic("open-file", "film", "打开文件", false)
                            .flex_1()
                            .py(px(5.))
                            .on_click(cx.listener(|this, _, _, cx| this.pick_file(cx))),
                    )
                    .child(
                        self.chip_ic("open-folder", "folder", "打开文件夹", false)
                            .flex_1()
                            .py(px(5.))
                            .on_click(cx.listener(|this, _, _, cx| this.pick_folder(cx))),
                    )
                    .child(
                        self.icon_btn("rescan", "refresh", 26.).on_click(cx.listener(
                            |this, _, _, cx| {
                                // 重新扫描当前文件夹；文件有增删这里能看到
                                if let Some(dir) = this.folder.clone() {
                                    this.load_dir(dir, cx);
                                } else {
                                    cx.notify();
                                }
                            },
                        )),
                    )
                    .child(
                        // 列表空着的时候把按钮压暗，一眼能看出没东西可清
                        self.icon_btn_c(
                            "clear-list",
                            "close",
                            26.,
                            if can_clear { pal.text() } else { pal.muted() },
                            false,
                        )
                        .debug_selector(|| "clear-list".to_string())
                        .on_click(cx.listener(|this, _, _, cx| this.clear_list(cx))),
                    ),
            )
            .children(self.folder.as_ref().map(|dir| {
                div()
                    .flex_none()
                    .px(px(10.))
                    .pb(px(8.))
                    .text_size(px(10.5))
                    .text_color(pal.muted())
                    // 长路径同样只给一行（原来只 overflow_hidden，会折成好几行）
                    .truncate()
                    .child(SharedString::from(dir.display().to_string()))
            }))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(6.))
                    .px(px(10.))
                    .pb(px(8.))
                    .child(self.icon_el("search", 13., pal.muted()))
                    .child(div().flex_1().min_w(px(0.)).child(
                        // 去掉输入框的白底和边框，融进侧栏面板
                        Input::new(&self.search).appearance(false),
                    ))
                    // 有输入才浮现的清除按钮（原版是 `.hidden` 切换）
                    .when(has_query, |d| {
                        d.child(
                            self.icon_btn("clear-search", "close", 18.)
                                .debug_selector(|| "clear-search".to_string())
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.clear_search(window, cx)),
                                ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(4.))
                    .px(px(10.))
                    .pb(px(8.))
                    .children(filters)
                    .child(div().flex_1())
                    .child(deep_chip),
            )
            .child(
                div()
                    .id("file-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .px(px(6.))
                    .pb(px(8.))
                    .when(empty, |d| {
                        d.child(
                            div()
                                .p(px(14.))
                                .text_size(px(11.5))
                                .text_color(pal.muted())
                                .child("还没有文件。点上面的按钮选一个文件夹，或把文件拖进来。"),
                        )
                    })
                    .children(rows),
            )
    }

    fn render_stage(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.pal();
        // 视频 / 图片一律「保持比例、完整显示」：播放区多大就是多大，
        // 画面自己缩放进去，放不满的方向留黑边。Fill 会把画面拉变形，已弃用。
        // （播放区容器本身**本来就贴窗口左右边缘** —— 用户看到的左右黑边
        //   是画面比例留下的，不是布局留白；2026-10-09 用户确认不改。）
        let fit = ObjectFit::Contain;
        let frame = self.player.as_mut().and_then(|p| p.frame());

        let body: AnyElement = if let Some(image) = frame {
            img(ImageSource::Render(image))
                .object_fit(fit)
                .size_full()
                .into_any_element()
        } else {
            match &self.stage {
                // 静态图片：**原尺寸展示**（1 图片像素 = 1 物理像素），只有
                // 比播放区还大时才等比缩小，绝不放大 —— 用户明确要求
                // "图片展示原尺寸即可，不用拉伸放大"。
                Stage::Image(image) => {
                    // 播放区可用尺寸：整窗扣掉侧栏（252）与底部控制区（104），
                    // 与音频可视化的估法保持一致。
                    let vs = window.viewport_size();
                    let avail_w = f32::from(vs.width) - if self.sidebar { 252.0 } else { 0.0 };
                    let avail_h = f32::from(vs.height) - 104.0;
                    // RenderImage 的 size 是**像素**；除以缩放系数才是逻辑点，
                    // 这样 1 图片像素正好落在 1 物理像素上（Retina 不发糊也不虚大）。
                    let s = image.size(0);
                    let k = window.scale_factor().max(1.0);
                    let nat_w = s.width.0 as f32 / k;
                    let nat_h = s.height.0 as f32 / k;
                    let scale = (avail_w / nat_w).min(avail_h / nat_h).min(1.0).max(0.02);
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size_full()
                        .child(
                            img(ImageSource::Render(image.clone()))
                                .w(px(nat_w * scale))
                                .h(px(nat_h * scale)),
                        )
                        .into_any_element()
                }
                Stage::Audio => {
                    // 均衡器可视化铺满整块播放区，中间叠一行曲名
                    //（用户要求「音乐播放时播放区域中间显示歌曲名称」）。
                    div()
                        .relative()
                        .size_full()
                        .child(self.viz.render(pal.text()))
                        .children(self.now_playing(pal))
                        .into_any_element()
                }
                Stage::Empty if self.player.is_some() => {
                    // 解码刚起来、第一帧还没到，别把落地页闪出来
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size_full()
                        .text_size(px(12.))
                        .text_color(pal.muted())
                        .child("正在起播…")
                        .into_any_element()
                }
                Stage::Empty => {
                    let fmt1 = "MP4 · MKV · AVI · MOV · WMV · FLV · RMVB · TS · WebM · MP3";
                    let fmt2 = "FLAC · APE · JPG · PNG · GIF · WebP · SVG …";
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .justify_center()
                        .gap(px(10.))
                        .size_full()
                        .children(self.logo.clone().map(|logo| {
                            img(ImageSource::Render(logo))
                                .w(px(LOGO_SIZE))
                                .h(px(LOGO_SIZE))
                                // 圆角已烘进像素（见 `round_corners`）；这里再写一遍
                                // 是双保险 —— 万一渲染器又支持图片圆角遮罩了也对得上
                                .rounded(px(LOGO_RADIUS))
                                .mb(px(6.))
                        }))
                        .child(
                            div()
                                .text_size(px(20.))
                                .text_color(pal.text())
                                .child("iPlayer"),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(pal.muted())
                                .child("打开一个文件夹，或把文件直接拖到这里"),
                        )
                        .child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .mt(px(6.))
                                .child(
                                    self.stage_btn("empty-open-file", "film", "打开文件")
                                        .on_click(cx.listener(|this, _, _, cx| this.pick_file(cx))),
                                )
                                .child(
                                    self.stage_btn("empty-open-folder", "folder", "打开文件夹")
                                        .on_click(cx.listener(|this, _, _, cx| this.pick_folder(cx))),
                                ),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .items_center()
                                .gap(px(3.))
                                .mt(px(16.))
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(pal.muted().opacity(0.75))
                                        .child(fmt1),
                                )
                                .child(
                                    div()
                                        .text_size(px(10.5))
                                        .text_color(pal.muted().opacity(0.75))
                                        .child(fmt2),
                                ),
                        )
                        .into_any_element()
                }
            }
        };

        // 左上角媒体信息浮层：默认关（快捷键 i 可开）；
        // 纯音频舞台一律不显示 —— 那块区域只留均衡器 + 中央曲名（见 `now_playing`）。
        let overlay = if self.show_info && !matches!(self.stage, Stage::Audio) {
            self.info_lines().map(|(name, meta)| {
                div()
                    .absolute()
                    .top(px(10.))
                    .left(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .max_w(px(620.))
                    .px(px(10.))
                    .py(px(7.))
                    .rounded_md()
                    .bg(pal.panel().opacity(0.86))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(pal.text())
                            .overflow_hidden()
                            .child(SharedString::from(name)),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(pal.muted())
                            .overflow_hidden()
                            .child(SharedString::from(meta)),
                    )
                    .into_any_element()
            })
        } else {
            None
        };

        // 字幕层：载了字幕、且没被隐藏、且正播着东西，才会叠出来
        let subtitle = self.render_subtitle();

        // 导出面板：右下角浮层。有任务在跑时强制显示。
        // 同角度面板：面板内的按钮不许把点击漏给"点画面播/停"。
        let export_panel = if self.export_panel || self.export.is_some() {
            Some(
                div()
                    .id("export-panel")
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .debug_selector(|| "export-panel".to_string())
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(self.render_export_panel(cx, pal))
                    .into_any_element(),
            )
        } else {
            None
        };

        // 画面角度面板：**右下角**浮层（用户指定，从右上角挪下来），
        // 与导出面板同位 —— 两者互斥（开一个会收起另一个），不会叠。
        // 浮层自己吃掉点击 —— 否则点面板里的按钮会冒泡到舞台根节点的
        // 「点画面播放 / 暂停」上，转个角度顺带把片子暂停了。
        let orient_panel = if self.orient_panel {
            Some(
                div()
                    .id("orient-panel")
                    .debug_selector(|| "orient-panel".to_string())
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(self.render_orient_panel(cx, pal))
                    .into_any_element(),
            )
        } else {
            None
        };

        // 字幕面板：第三个右下角浮层（与角度 / 导出面板互斥）。
        let sub_panel = if self.sub_panel {
            Some(
                div()
                    .id("sub-panel")
                    .debug_selector(|| "sub-panel".to_string())
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                    .child(self.render_sub_panel(cx, pal))
                    .into_any_element(),
            )
        } else {
            None
        };

        div()
            .id("stage")
            .debug_selector(|| "stage".to_string())
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .bg(pal.stage())
            // 点画面 = 播放 / 暂停（照大多数播放器的习惯）。
            // 没打开媒体时 `toggle()` 自己会空转，所以落地页上点空白处什么也不会发生；
            // 两个浮层面板已经 `stop_propagation`，不会误触。
            .when(self.player.is_some(), |d| d.cursor_pointer())
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
            .child(body)
            // 字幕压在画面下方，但要在几个浮层**之下**（浮层是操作区，不能被字幕盖住）
            .children(subtitle)
            .children(overlay)
            .children(orient_panel)
            .children(export_panel)
            .children(sub_panel)
            .into_any_element()
    }

    /// 音频舞台中央显示的曲名（没在播东西 / 拿不到名字就返回 None）。
    fn audio_title(&self) -> Option<String> {
        song_title(
            self.info.as_ref().map(|i| i.title.as_str()),
            self.active_path.as_deref(),
        )
    }

    /// 音频舞台中间的曲名浮层：一个音符 + 一行曲名。
    ///
    /// 位置**顶在画面 30% 高度处**，而不是正好居中 —— 频谱柱最高能顶到
    /// 41% 处，摆正中会被柱子从下面穿过。横向整幅铺开、内容居中，
    /// 长曲名按 62% 宽度自动折行并居中。
    ///
    /// 这一层不带任何交互，所以点击照样落到舞台根节点上：
    /// 点曲名和点别处一样能播放 / 暂停。
    fn now_playing(&self, pal: &'static Palette) -> Option<AnyElement> {
        let title = self.audio_title()?;
        Some(
            div()
                .absolute()
                .top(relative(0.30))
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(10.))
                .child(self.icon_el("music", 24., pal.muted()))
                .child(
                    div()
                        .max_w(relative(0.62))
                        .text_center()
                        .text_size(px(20.))
                        .line_height(px(27.))
                        .text_color(pal.text())
                        .debug_selector(|| "now-playing".to_string())
                        .child(SharedString::from(title)),
                )
                .into_any_element(),
        )
    }

    /// 叠在画面下方的字幕。
    ///
    /// 横向铺满、内容居中，靠**下边距 6%** 定位（落在控制条之上、离画面中心远远的）；
    /// 多行逐行渲染（字幕里的换行是硬换行）。底衬是半透明黑 + 白字：
    /// 字幕底下的画面内容不可控，只有这样才能保证亮画面黑字、暗画面白字都看清，
    /// 所以这一层不跟主题走（深浅两套主题下长得一样）。
    fn render_subtitle(&self) -> Option<AnyElement> {
        if !self.sub_visible {
            return None;
        }
        let subs = self.subs.as_ref()?;
        let pos = self.player.as_ref()?.position();
        let text = subs.at(pos)?;

        let size = SUB_FONT * self.sub_scale();
        let ink = gpui_kit::Hsla::from(gpui_kit::rgb(0xf7f8fa));
        let lines: Vec<AnyElement> = text
            .lines()
            .map(|line| {
                div()
                    .text_size(px(size))
                    .line_height(px(size * 1.35))
                    .text_color(ink)
                    .child(SharedString::from(line.to_string()))
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .absolute()
                .bottom(relative(0.06))
                .left_0()
                .right_0()
                .flex()
                .flex_col()
                .items_center()
                .child(
                    div()
                        .id("subtitle")
                        .debug_selector(|| "subtitle".to_string())
                        .max_w(relative(0.84))
                        .px(px(12.))
                        .py(px(4.))
                        .rounded_md()
                        .bg(gpui_kit::Hsla::from(gpui_kit::rgb(0x000000)).opacity(0.62))
                        .flex()
                        .flex_col()
                        .items_center()
                        .children(lines),
                )
                .into_any_element(),
        )
    }

    /// 画面角度面板：四向调整 + 复位，顶部显示当前朝向。
    fn render_orient_panel(
        &self,
        cx: &mut Context<Self>,
        pal: &'static Palette,
    ) -> AnyElement {
        const CARD_W: f32 = 172.0;
        let cur = self.orient;

        let card = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .w(px(CARD_W))
            .p(px(8.))
            .rounded_md()
            .bg(pal.panel().opacity(0.97))
            .border_1()
            .border_color(pal.line())
            .child(
                div()
                    .px(px(8.))
                    .pb(px(4.))
                    .text_size(px(10.5))
                    .text_color(pal.muted())
                    // 当前朝向直接写在标题里，省得用户猜自己转了几圈
                    .child(SharedString::from(format!("画面角度 · {}", cur.label()))),
            );

        let ops: [(&'static str, &'static str, Orientation); 4] = [
            ("orient-ccw", "左转 90°", cur.rotated_ccw()),
            ("orient-cw", "右转 90°", cur.rotated_cw()),
            ("orient-fliph", "左右翻转", cur.flipped_h()),
            ("orient-flipv", "上下翻转", cur.flipped_v()),
        ];

        let rows = ops.into_iter().map(|(id, label, next)| {
            div()
                .id(id)
                .flex()
                .items_center()
                .px(px(8.))
                .py(px(6.))
                .rounded_md()
                .cursor_pointer()
                .text_size(px(11.5))
                .text_color(pal.text())
                .hover(|s| s.bg(pal.hover()))
                .debug_selector(move || id.to_string())
                .on_click(
                    cx.listener(move |this, _, _, cx| this.apply_orientation(next, cx)),
                )
                .child(SharedString::from(label))
        });

        card.child(
            div()
                .flex()
                .flex_col()
                .gap(px(1.))
                .children(rows)
                .child(
                    div()
                        .id("orient-reset")
                        .flex()
                        .items_center()
                        .px(px(8.))
                        .py(px(6.))
                        .mt(px(3.))
                        .border_t_1()
                        .border_color(pal.line())
                        .rounded_md()
                        .cursor_pointer()
                        .text_size(px(11.5))
                        .text_color(if cur.is_identity() { pal.muted() } else { pal.text() })
                        .hover(|s| s.bg(pal.hover()))
                        .debug_selector(|| "orient-reset".to_string())
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.apply_orientation(Orientation::IDENTITY, cx)
                        }))
                        .child("恢复原始"),
                ),
        )
        .into_any_element()
    }

    /// 字幕面板里的一行（图标 + 文字，右侧一条小提示）——
    /// 四行共用同一副骨架，动作各自由调用处挂 `.on_click`。
    ///
    /// `on` = 这一行"现在能起作用"：不能时文字压暗（比如没有字幕时的
    /// 显示 / 隐藏与移除），点了只会得到一句提示而不是静默。
    fn sub_row(
        &self,
        pal: &'static Palette,
        id: &'static str,
        icon: &'static str,
        label: &str,
        hint: &str,
        on: bool,
    ) -> Stateful<Div> {
        let fg = if on { pal.text() } else { pal.muted().opacity(0.7) };
        div()
            .id(id)
            .debug_selector(move || id.to_string())
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .px(px(8.))
            .py(px(6.))
            .rounded_md()
            .cursor_pointer()
            .text_color(fg)
            .hover(|s| s.bg(pal.hover()))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(7.))
                    .overflow_hidden()
                    .child(self.icon_el(icon, 14., pal.muted()))
                    .child(div().text_size(px(12.)).child(SharedString::from(label.to_string()))),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(pal.muted())
                    .child(SharedString::from(hint.to_string())),
            )
    }

    /// 字幕面板：导入 · 显示/隐藏 · 字号 · 移除，顶部一行写明当前字幕。
    fn render_sub_panel(&self, cx: &mut Context<Self>, pal: &'static Palette) -> AnyElement {
        const CARD_W: f32 = 236.0;
        let has = self.subs.is_some();

        let head = match &self.subs {
            Some(s) => format!("{} · {} 条", s.name, s.len()),
            None => "未载入（可自动找同名字幕）".to_string(),
        };
        let toggle_label = if self.sub_visible { "隐藏字幕" } else { "显示字幕" };
        let size_label = SUB_SIZE_LABELS[self.sub_size.min(SUB_SIZE_LABELS.len() - 1)];

        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .w(px(CARD_W))
            .p(px(10.))
            .rounded_md()
            .bg(pal.panel().opacity(0.97))
            .border_1()
            .border_color(pal.line())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .pb(px(4.))
                    .child(self.icon_el("captions", 13., pal.muted()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(pal.muted())
                            .child(SharedString::from(head)),
                    ),
            )
            .child(
                self.sub_row(pal, "sub-import", "download", "导入字幕…", ".srt · .vtt · .ass", true)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.request_import_subtitle(window, cx)
                    })),
            )
            .child(
                self.sub_row(pal, "sub-toggle", "captions", toggle_label, "S", has)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_subtitles(cx))),
            )
            .child(
                self.sub_row(pal, "sub-size", "expand", "字幕大小", size_label, true)
                    .on_click(cx.listener(|this, _, _, cx| this.cycle_sub_size(cx))),
            )
            .child(
                self.sub_row(pal, "sub-remove", "close", "移除字幕", "", has)
                    .on_click(cx.listener(|this, _, _, cx| this.remove_subtitle(cx))),
            )
            .into_any_element()
    }

    /// 导出面板：空闲时列三个动作，跑任务时显示进度和取消按钮。
    fn render_export_panel(
        &self,
        cx: &mut Context<Self>,
        pal: &'static Palette,
    ) -> AnyElement {
        const CARD_W: f32 = 236.0;
        const INNER_W: f32 = CARD_W - 20.0;

        let card = div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .w(px(CARD_W))
            .p(px(10.))
            .rounded_md()
            .bg(pal.panel().opacity(0.97))
            .border_1()
            .border_color(pal.line());

        if let Some(job) = &self.export {
            let pct = job.percent() as f32;
            return card
                .child(
                    div()
                        .text_size(px(11.5))
                        .text_color(pal.text())
                        .overflow_hidden()
                        .child(SharedString::from(job.label.clone())),
                )
                .child(
                    div()
                        .h(px(4.))
                        .w_full()
                        .rounded_full()
                        .bg(pal.hover())
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(pal.accent())
                                .w(px(INNER_W * pct / 100.0)),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(pal.muted())
                                .child(SharedString::from(format!("{:.0}%", pct))),
                        )
                        .child(
                            self.chip("export-cancel", "取消", false).on_click(
                                cx.listener(|this, _, _, cx| this.cancel_export(cx)),
                            ),
                        ),
                )
                .into_any_element();
        }

        let rows = export::Kind::ALL.map(|kind| {
            let ic = match kind {
                export::Kind::Snapshot => "camera",
                export::Kind::Audio => "download",
                export::Kind::Gif => "gif",
            };
            let sel = match kind {
                export::Kind::Snapshot => "export-snapshot",
                export::Kind::Audio => "export-audio",
                export::Kind::Gif => "export-gif",
            };
            div()
                .id(("export-row", kind as usize))
                .debug_selector(move || sel.to_string())
                .flex()
                .items_center()
                .justify_between()
                .gap(px(8.))
                .px(px(8.))
                .py(px(6.))
                .rounded_md()
                .cursor_pointer()
                .text_color(pal.text())
                .hover(|s| s.bg(pal.hover()))
                .on_click(
                    cx.listener(move |this, _, window, cx| {
                        this.request_export(kind, window, cx)
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.))
                        .overflow_hidden()
                        .child(self.icon_el(ic, 14., pal.muted()))
                        .child(div().text_size(px(12.)).child(kind.label())),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(pal.muted())
                        .child(kind.hint()),
                )
                .into_any_element()
        });

        card.child(
            div()
                .px(px(8.))
                .text_size(px(11.))
                .text_color(pal.muted())
                .child("导出"),
        )
        .children(rows)
        .into_any_element()
    }

    fn render_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let pal = self.pal();
        let (pos, dur) = match &self.player {
            Some(p) => (p.position(), p.duration()),
            None => (0.0, 0.0),
        };
        let playing = self.player.as_ref().is_some_and(|p| p.is_playing());
        let has_player = self.player.is_some();
        let speed_label = format!("{:.1}x", self.speed);

        // 右：音量 / 倍速 / 循环。先单独造好，下面按侧栏状态决定要不要给它套一个等宽槽。
        let right = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .debug_selector(|| "vol-cluster".to_string())
            .opacity(if has_player { 1.0 } else { 0.45 })
            .child(
                self.icon_btn_sz(
                    "mute",
                    if self.muted { "volumeX" } else { "volume" },
                    36.,
                    PLAY_ICON,
                    pal.text(),
                    self.muted,
                )
                .on_click(cx.listener(|this, _, window, cx| this.toggle_mute(window, cx))),
            )
            .child({
                // 音量条：组件滑块自带一圈半透明圆底（条色 50%）、hover 时再叠
                // 3px 描边环 —— 深色控制条上就是尾巴上那团灰晕（用户视为阴影）。
                // 组件没有关掉它们的 API，照搬进度条的做法：组件隐身只留指针
                // 交互，条和滑块都自己画 —— 3px 细条 + 实心圆点，无半透明层。
                let frac = (self.volume.read(cx).value().start() / 100.0).clamp(0.0, 1.0);
                div()
                    .relative()
                    .w(px(76.))
                    .debug_selector(|| "vol-box".to_string())
                    .child(
                        // 轨道：底轨 20% + 已播实色，与进度条同一画法
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .top_0()
                            .bottom_0()
                            .flex()
                            .items_center()
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .h(px(3.))
                                    .rounded_full()
                                    .bg(pal.text().opacity(0.2))
                                    .child(
                                        div()
                                            .absolute()
                                            .left_0()
                                            .top_0()
                                            .bottom_0()
                                            .w(relative(frac))
                                            .rounded_full()
                                            .bg(pal.text()),
                                    ),
                            ),
                    )
                    .child(
                        // 滑块：实心圆点，圆心压在已播末端（与组件 thumb 同位，
                        // 满音量时右半会探出方框 5px —— 与常规滑块一致）
                        div()
                            .absolute()
                            .left(relative(frac))
                            .top_0()
                            .bottom_0()
                            .w(px(10.))
                            .ml(px(-5.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().w(px(10.)).h(px(10.)).rounded_full().bg(pal.text())),
                    )
                    .child(
                        Slider::new(&self.volume)
                            .horizontal()
                            .bg(pal.text().opacity(0.0))
                            .text_color(pal.text().opacity(0.0)),
                    )
            })
            .child(
                // 单个 chip 循环 0.5 → 3.0（原 Tauri 版是下拉）；
                // 键盘 [ / ] 仍可逐级微调
                self.chip_bright("speed", &speed_label).on_click(
                    cx.listener(|this, _, _, cx| {
                        let next = if this.speed >= 3.0 {
                            0.5
                        } else {
                            (this.speed + 0.5).min(3.0)
                        };
                        this.set_speed(next, cx);
                    }),
                ),
            )
            .child(
                self.icon_btn_sz(
                    "loop",
                    match self.loop_mode {
                        LoopMode::One => "loop1",
                        _ => "loop",
                    },
                    36.,
                    PLAY_ICON,
                    // 用户要求右下角图标提亮：不再按开关状态用次要色，
                    // 开启态靠底色高亮区分
                    pal.text(),
                    self.loop_mode.is_on(),
                )
                .debug_selector(|| "loop".to_string())
                .on_click(cx.listener(|this, _, _, cx| {
                    this.loop_mode = this.loop_mode.next();
                    let msg = this.loop_mode.toast();
                    this.toast(msg, cx);
                })),
            )
            // 「信息」按钮已按用户要求从控制条去掉；浮层仍可用快捷键 i 开关。
            // 「截图」按钮也去掉了 —— 工具箱面板里本来就有这一项（少一颗重复按钮）。
            // 循环右边这组"画面 / 窗口工具"（用户指定）：切换方向 · 字幕 · 工具箱 · 最大化。
            // 前三个原先都在标题栏右上角，那一片现在只剩窗口开关类（深浅色 · 置顶；
            // 最小化 / 关闭交给 macOS 红绿灯）。全屏没有再给按钮 ——
            // 绿色交通灯和快捷键 ⌘F 都还在。开关态照旧靠底色高亮，不用暗色。
            .child(
                self.icon_btn_sz(
                    "ctl-orient",
                    "rotate",
                    36.,
                    PLAY_ICON,
                    pal.text(),
                    self.orient_panel || !self.orient.is_identity(),
                )
                .debug_selector(|| "ctl-orient".to_string())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_orient_panel(cx))),
            )
            // 字幕（用户要求：加在「工具箱」左侧）：导入 / 显示隐藏 / 字号 / 移除
            .child(
                self.icon_btn_sz(
                    "ctl-sub",
                    "captions",
                    36.,
                    PLAY_ICON,
                    pal.text(),
                    self.sub_panel || (self.subs.is_some() && self.sub_visible),
                )
                .debug_selector(|| "ctl-sub".to_string())
                .on_click(cx.listener(|this, _, _, cx| this.toggle_sub_panel(cx))),
            )
            .child(
                self.icon_btn_sz("ctl-export", "toolbox", 36., PLAY_ICON, pal.text(), self.export_panel)
                    .debug_selector(|| "ctl-export".to_string())
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_export_panel(cx))),
            )
            // 最右端（用户要求「右下角增加一个最大化的图标」）：点一下最大化、
            // 再点一下还原。图标按窗口真实状态位切换（`isZoomed`），所以点绿色
            // 交通灯最大化之后这里也会同步变成「还原」的样子。
            // 注意走的是 GPUI 的 `Window::zoom_window()`（异步），
            // 千万别改成同步调 AppKit —— 原因见 `toggle_maximize` 的注释。
            .child({
                let zoomed = native::is_window_zoomed(window);
                self.icon_btn_sz(
                    "ctl-max",
                    Self::maximize_glyph(zoomed),
                    36.,
                    PLAY_ICON,
                    pal.text(),
                    zoomed,
                )
                .debug_selector(|| "ctl-max".to_string())
                .on_click(cx.listener(|this, _, window, cx| this.toggle_maximize(window, cx)))
            });

        // 右组永远套一个跟左侧时间区等宽的 `1fr`，左右一配对，
        // 中间的播放组就固定落在整行正中 —— 不再随侧栏折叠换位置。
        let right_slot = div()
            .flex()
            .flex_1()
            .min_w(px(0.))
            .items_center()
            .justify_end()
            .child(right);

        div()
            .flex()
            .flex_col()
            .flex_none()
            .bg(pal.bg())
            .child(
                div()
                    .flex_none()
                    // 左右各留 24px（34 → 29 → 24，用户两次各要求收 5px）
                    .px(px(24.))
                    .pt(px(6.))
                    // 无头测试靠这个选择器找进度条的坐标（非 debug 构建是空操作）
                    .debug_selector(|| "seek-row".to_string())
                    .child({
                        // 组件滑轨固定 6px 高、调不细 —— 这里把它整体隐形
                        // （条色/滑块色全透明），细条自己画。指针交互
                        //（点击/拖动/节流）仍完全由组件承担，行为不变。
                        let frac = self.seek.read(cx).value().start().clamp(0.0, 1.0);
                        div()
                            .relative()
                            .w_full()
                            .flex()
                            .flex_col()
                            // 自绘 3px 细条：底轨 20%、已播部分实色
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .relative()
                                            .w_full()
                                            .h(px(3.))
                                            .rounded_full()
                                            .bg(pal.text().opacity(0.2))
                                            .child(
                                                div()
                                                    .absolute()
                                                    .left_0()
                                                    .top_0()
                                                    .bottom_0()
                                                    .w(relative(frac))
                                                    .rounded_full()
                                                    .bg(pal.text()),
                                            ),
                                    ),
                            )
                            .child(
                                Slider::new(&self.seek)
                                    .horizontal()
                                    .bg(pal.text().opacity(0.0))
                                    .text_color(pal.text().opacity(0.0)),
                            )
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    // 间距照抄原版（14px），播放组永远整行居中
                    .gap(px(14.))
                    // 与上面进度条同宽：左右各 24px（19 = 24 - 5，用户要求控制区再收 5px）
                    .px(px(19.))
                    .pt(px(4.))
                    .pb(px(10.))
                    .debug_selector(|| "ctrl-row".to_string())
                    // 左：时间。flex_1 吃掉全部余量 —— 侧栏展开时把后面两组挤到右端。
                    // pl(5px)：用户要求时间右移 5px，正好与上面进度条的左缘（24px）对齐。
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
                            .pl(px(5.))
                            .gap(px(4.))
                            .text_size(px(12.))
                            .text_color(pal.text())
                            .child(SharedString::from(media::format_time(pos)))
                            .child(div().text_color(pal.muted()).child("/"))
                            .child(
                                div()
                                    .text_color(pal.muted())
                                    .child(SharedString::from(media::format_time(dur))),
                            ),
                    )
                    // 中：播放组。宽度自适应；侧栏收起时左右各一个等宽 1fr，它自然居中
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .gap(px(4.))
                            .debug_selector(|| "play-group".to_string())
                            .child(
                                self.icon_btn_sz("ctl-prev", "prev", 36., PLAY_ICON, pal.text(), false)
                                    .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))),
                            )
                            .child(
                                self.icon_btn_sz(
                                    "ctl-play",
                                    if playing { "pause" } else { "play" },
                                    44.,
                                    PLAY_ICON,
                                    pal.text(),
                                    false,
                                )
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
                            )
                            .child(
                                self.icon_btn_sz("ctl-next", "next", 36., PLAY_ICON, pal.text(), false)
                                    .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
                            )
                            .child(
                                self.icon_btn_sz("ctl-stop", "stop", 36., PLAY_ICON, pal.text(), false)
                                    .on_click(cx.listener(|this, _, window, cx| this.stop(window, cx))),
                            ),
                    )
                    // 右：音量 / 倍速 / 循环（槽宽随侧栏状态在 auto / 1fr 之间切）
                    .child(right_slot),
            )
    }
}
impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tick(window, cx);

        let pal = self.pal();
        let sidebar_open = self.sidebar;
        let toast = self.toast.clone();

        // 四块内容按顺序各自构造好，再把它们拼进根节点 ——
        // 不要用 `bool::then(|| ...)`，那会让闭包把 self/cx 的借用一直攥到根节点构造完。
        let stage = self.render_stage(window, cx);
        let sidebar = if sidebar_open {
            Some(self.render_sidebar(cx))
        } else {
            None
        };
        let titlebar = self.render_titlebar(cx);
        let controls = self.render_controls(window, cx);
        let focus = self.focus.clone();

        let mut root = div()
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.note_activity();
                this.on_key(ev, window, cx)
            }))
            // 有面板停靠时，**窗口里任何一次鼠标动作 / 点击**都算"还在用"，
            // 把停靠超时的表重新拨到此刻（点哪儿都算，不只是点面板里）。
            // 注意：GPUI 的事件分派里没被 occlude 的祖先也在 hover 链上，
            // 所以挂在根节点上能收到全窗口的动作。
            .on_mouse_move(cx.listener(|this, _: &MouseMoveEvent, _, _| this.note_activity()))
            .capture_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, _, _| this.note_activity()))
            // Dock 菜单「显示主界面」在窗口还"活跃"（只是被 orderOut 藏了）
            // 时走这条：把窗口亮回来。无活跃窗口时由 main.rs 里的全局监听兜住。
            .on_action(cx.listener(|_, _: &ShowMainWindow, window, cx| {
                native::show_window(window);
                cx.notify();
            }))
            .flex()
            .flex_col()
            .size_full()
            .overflow_hidden()
            .bg(pal.bg())
            .text_color(pal.text())
            .on_drop(cx.listener(
                // 注意：泛型参数必须是 `ExternalPaths` 本身 —— gpui 用
                // `TypeId::of::<T>()` 对上 active_drag 的具体类型（外部拖放
                // 存的是 `Arc<ExternalPaths>`，`as_ref()` 后是 ExternalPaths）。
                // 写成 `&Arc<ExternalPaths>` 会静默匹配不上，拖放整个失灵。
                |this, paths: &ExternalPaths, _, cx| {
                    if let Some(first) = paths.paths().first() {
                        this.load_path(first.clone(), cx);
                    }
                },
            ))
            .child(titlebar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .children(sidebar)
                    .child(stage),
            )
            .child(controls);

        if let Some((msg, _)) = toast {
            root = root.child(
                div()
                    .absolute()
                    .bottom(px(92.))
                    .left(px(0.))
                    .right(px(0.))
                    .flex()
                    .justify_center()
                    .child(
                        div()
                            .px(px(12.))
                            .py(px(6.))
                            .rounded_md()
                            .bg(pal.accent())
                            .text_color(pal.on_accent())
                            .text_size(px(12.))
                            .child(SharedString::from(msg)),
                    ),
            );
        }

        root
    }
}

impl Focusable for App {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{load_logo, round_corners};

    /// 「要下一帧」这个动作只许出现在渲染路径里。
    ///
    /// `Window::request_animation_frame()` 内部是
    /// `current_view()` → `rendered_entity_stack.last().unwrap()`，只在渲染 /
    /// prepaint 期间成立。放进点击 / 键盘回调里就是 `Option::unwrap()` on None，
    /// 而回调是 ObjC `extern "C"` 进来的，panic 不能 unwind → 整个进程 abort，
    /// 日志里只有一句 "panic in a function that cannot unwind"（用户报的
    /// "点导出按钮就崩"就是这么来的，见 `request_export` 里的注释）。
    ///
    /// 扫源码而不是跑用例，是因为要做到"越界调用"得有真窗口 + 真 NSApp 才跑得到那一句。
    /// 规则有两条，缺一不可：
    /// 1. 只有 `tick` 和它专用的小助手 `begin_export` 里能调这个 API；
    /// 2. `begin_export` 本身也只能被 `tick` 调 —— 否则它就成了"能被事件回调间接调到"的洞。
    #[test]
    fn animation_frames_are_requested_only_from_the_render_path() {
        let src = include_str!("app.rs");
        // 拼出来是为了让"要扫的那个字符串"不出现在这条测试自己的源码里
        // （否则测试会先扫到自己）。
        let call = concat!(".request_animation", "_frame()");
        let helper_call = concat!("self.begin", "_export(");

        /// 某个方法的函数体范围（下一个同缩进的 `fn` 之前都算它的）。
        fn fn_range(src: &str, sig: &str) -> (usize, usize) {
            let start = src.find(sig).unwrap_or_else(|| panic!("找不到 {sig}"));
            let rest = &src[start + 1..];
            let end = match rest.find("\n    fn ") {
                Some(i) => start + 1 + i,
                None => src.len(),
            };
            (start, end)
        }

        let tick = fn_range(src, "    fn tick(");
        let begin = fn_range(src, "    fn begin_export(");

        let mut at = 0usize;
        let mut hits = 0usize;
        while let Some(i) = src[at..].find(call) {
            let pos = at + i;
            at = pos + call.len();
            hits += 1;
            let inside = (pos > tick.0 && pos < tick.1) || (pos > begin.0 && pos < begin.1);
            assert!(
                inside,
                "app.rs 第 {} 行在渲染路径之外要下一帧；事件回调里只能用 cx.notify()",
                src[..pos].matches('\n').count() + 1
            );
        }
        assert!(hits > 0, "一个调用点都没有？那这条测试本身失效了");

        // `begin_export` 必须只有 tick 一个调用者，不然上面那条豁免就漏了
        let mut at = 0usize;
        while let Some(i) = src[at..].find(helper_call) {
            let pos = at + i;
            at = pos + 1;
            let line = src[..pos].matches('\n').count() + 1;
            assert!(
                pos > tick.0 && pos < tick.1,
                "app.rs 第 {line} 行在 tick 之外调了 begin_export —— 它会要下一帧，只能从渲染路径进"
            );
        }
    }

    /// 落地页 logo 编译期嵌入，解码必须一直可用；顺便钉住尺寸与 R/B 通路。
    #[test]
    fn embedded_logo_decodes() {
        let logo = load_logo().expect("assets/logo.png 应能随二进制解码");
        let size = logo.size(0);
        assert_eq!((size.width.0, size.height.0), (200, 200));
    }

    /// 圆角是**烘进像素**的（`img().rounded()` 在这套 GPUI 上不一定生效）：
    /// 四角必须透掉、边中点与中心必须保持不透明。
    #[test]
    fn round_corners_clears_only_the_corners() {
        let mut img = image::RgbaImage::from_pixel(40, 40, image::Rgba([255, 255, 255, 255]));
        round_corners(&mut img, 10.0);

        let a = |x: u32, y: u32| img.get_pixel(x, y).0[3];
        assert_eq!(a(0, 0), 0, "左上角应当完全透明");
        assert_eq!(a(39, 0), 0, "右上角应当完全透明");
        assert_eq!(a(0, 39), 0, "左下角应当完全透明");
        assert_eq!(a(39, 39), 0, "右下角应当完全透明");
        assert_eq!(a(20, 0), 255, "上边中点应当保持不透明");
        assert_eq!(a(0, 20), 255, "左边中点应当保持不透明");
        assert_eq!(a(20, 20), 255, "中心应当保持不透明");

        // 圆角内缘（距边 4px、距顶 4px 处）在半径 10 的圆弧之内 → 不透明
        assert_eq!(a(4, 4), 255, "圆弧内侧不该被削掉");
    }
}

#[cfg(test)]
mod pointer_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext, point};

    use super::*;

    fn harness(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    /// 对照：音量条按下并拖动应当跟手 —— 证明这套无头指针模拟是可用的。
    #[gpui::test]
    fn volume_slider_can_be_dragged(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let box_ = cx.debug_bounds("vol-box").expect("音量条应当被渲染");

        cx.simulate_mouse_move(box_.center(), None, Modifiers::default());
        cx.simulate_mouse_down(box_.center(), MouseButton::Left, Modifiers::default());
        for frac in [0.2f32, 0.4, 0.8] {
            cx.simulate_mouse_move(
                point(box_.origin.x + box_.size.width * frac, box_.center().y),
                MouseButton::Left,
                Modifiers::default(),
            );
        }
        cx.simulate_mouse_up(box_.center(), MouseButton::Left, Modifiers::default());

        let v = cx.update(|_, cx| {
            held.borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .volume
                .read(cx)
                .value()
                .end()
        });
        assert!((v - 80.0).abs() < 6.0, "音量条拖到 80% 应当约 80，实际 {v}");
    }

    /// 进度条必须能被按下并拖动。
    #[gpui::test]
    fn seek_slider_can_be_dragged(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let row = cx.debug_bounds("seek-row").expect("进度条那一行应当被渲染");

        cx.simulate_mouse_move(row.center(), None, Modifiers::default());
        cx.simulate_mouse_down(row.center(), MouseButton::Left, Modifiers::default());
        for frac in [0.35f32, 0.55, 0.75] {
            cx.simulate_mouse_move(
                point(row.origin.x + row.size.width * frac, row.center().y),
                MouseButton::Left,
                Modifiers::default(),
            );
        }
        cx.simulate_mouse_up(row.center(), MouseButton::Left, Modifiers::default());

        let v = cx.update(|_, cx| {
            held.borrow()
                .as_ref()
                .unwrap()
                .read(cx)
                .seek
                .read(cx)
                .value()
                .end()
        });
        assert!((v - 0.75).abs() < 0.06, "进度条拖到 75% 应当约 0.75，实际 {v}");
    }
}

/// 拖放回归：外部文件拖进窗口（无论落在舞台还是列表上）都必须被加载。
/// 复刻 gpui 自测的手法 —— 直接向窗口派发 `FileDropEvent::Entered/Submit`，
/// 走的就是平台层拖放进来后被翻译成合成 MouseUp 的同一条路。
#[cfg(test)]
mod drop_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{ExternalPaths, FileDropEvent, TestAppContext, VisualTestContext, point, px};
    use gpui_kit::InputEvent as _;

    use super::*;

    fn harness(cx: &mut TestAppContext) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn temp_png(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("iplayer-drop-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut src = image::RgbaImage::new(4, 4);
        for px_ in src.pixels_mut() {
            *px_ = image::Rgba([255, 0, 0, 255]);
        }
        src.save(&path).unwrap();
        path
    }

    /// 隔离测试：裸 div + on_drop，验证 gpui 无头链路本身是否通。
    #[gpui::test]
    fn bare_div_drop_works(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let dropped: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
        struct Bare(Rc<RefCell<bool>>);
        impl Render for Bare {
            fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
                let flag = self.0.clone();
                div()
                    .size_full()
                    .bg(gpui::black())
                    .on_drop(move |_: &ExternalPaths, _, _| {
                        *flag.borrow_mut() = true;
                    })
            }
        }
        let flag = dropped.clone();
        let (_, cx) = cx.add_window_view(move |_, _| Bare(flag));
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let pos = point(px(300.), px(200.));
        cx.update(|window, cx| {
            window.dispatch_event(
                FileDropEvent::Entered {
                    position: pos,
                    paths: ExternalPaths(smallvec::smallvec![std::path::PathBuf::from("/tmp/x")]),
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(FileDropEvent::Submit { position: pos }.to_platform_input(), cx);
        });
        assert!(*dropped.borrow(), "裸 div 的 on_drop 应当被触发");
    }

    /// 拖一张 PNG 落在舞台中央：应当被识别为图片并进 `Stage::Image`。
    #[gpui::test]
    fn dropped_image_loads(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let path = temp_png("drop.png");

        let pos = point(px(500.), px(300.));
        cx.update(|window, cx| {
            window.dispatch_event(
                FileDropEvent::Entered {
                    position: pos,
                    paths: ExternalPaths(smallvec::smallvec![path.clone()]),
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(FileDropEvent::Submit { position: pos }.to_platform_input(), cx);
        });

        let app = held.borrow().as_ref().unwrap().clone();
        let (active, is_image) = cx.update(|_, cx| {
            let a = app.read(cx);
            (a.active_path.clone(), matches!(a.stage, Stage::Image(_)))
        });
        assert_eq!(
            active.as_deref(),
            Some(path.to_str().unwrap()),
            "拖放的文件应当被加载"
        );
        assert!(is_image, "PNG 应当进图片舞台");

        std::fs::remove_file(&path).ok();
    }

    /// 拖一张 PNG 落在侧栏列表上：同样要加载（这是用户报的另一个落点）。
    #[gpui::test]
    fn dropped_image_on_sidebar_loads(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let path = temp_png("drop2.png");

        let pos = point(px(120.), px(300.));
        cx.update(|window, cx| {
            window.dispatch_event(
                FileDropEvent::Entered {
                    position: pos,
                    paths: ExternalPaths(smallvec::smallvec![path.clone()]),
                }
                .to_platform_input(),
                cx,
            );
            window.dispatch_event(FileDropEvent::Submit { position: pos }.to_platform_input(), cx);
        });

        let app = held.borrow().as_ref().unwrap().clone();
        let active = cx.update(|_, cx| app.read(cx).active_path.clone());
        assert_eq!(
            active.as_deref(),
            Some(path.to_str().unwrap()),
            "落在侧栏上的拖放也应当被加载"
        );

        std::fs::remove_file(&path).ok();
    }
}

/// 侧栏「清空」这一组按钮的回归 —— 原版有、GPUI 版一度漏掉的两个入口。
#[cfg(test)]
mod clear_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn sample(name: &str, kind: &str) -> MediaFile {
        MediaFile {
            name: name.to_string(),
            path: format!("/demo/{name}"),
            ext: name.rsplit_once('.').map_or(String::new(), |(_, e)| e.to_string()),
            size: 1024,
            modified: 0,
            kind: kind.to_string(),
        }
    }

    /// 摆好列表（可选预先写入搜索词）后重画一帧，返回 App 实体。
    fn stage(
        cx: &mut VisualTestContext,
        held: &Rc<RefCell<Option<Entity<App>>>>,
        query: &str,
    ) -> Entity<App> {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, window, cx| {
            view.files = vec![sample("a.mp4", "video"), sample("b.mp3", "audio")];
            view.folder = Some(PathBuf::from("/demo"));
            view.search
                .update(cx, |st, cx| st.set_value(query, window, cx));
            view.refresh_view(cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        app
    }

    /// 侧栏的「×」必须真的清空列表，并把手里的文件夹一起放下。
    #[gpui::test]
    fn clear_list_empties_the_sidebar(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = stage(cx, &held, "");

        let before = cx.update(|_, cx| app.read(cx).view.len());
        assert_eq!(before, 2, "准备阶段：列表里应当有 2 项");

        let btn = cx.debug_bounds("clear-list").expect("清空按钮应当被渲染");
        cx.simulate_click(btn.center(), Modifiers::default());

        let (files, view, folder) = cx.update(|_, cx| {
            let app = app.read(cx);
            (app.files.len(), app.view.len(), app.folder.clone())
        });
        assert_eq!(files, 0, "点清空后 files 应当为空");
        assert_eq!(view, 0, "点清空后列表视图应当为空");
        assert!(folder.is_none(), "清空时应当忘掉当前文件夹");
    }

    /// 搜索框的清除按钮：有输入才出现，点了要清关键字并把整份列表还回来。
    #[gpui::test]
    fn search_clear_button_restores_the_list(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = stage(cx, &held, "mp3");

        let filtered = cx.update(|_, cx| app.read(cx).view.len());
        assert_eq!(filtered, 1, "搜索 mp3 应当只剩 1 项");

        let btn = cx
            .debug_bounds("clear-search")
            .expect("有输入时清除按钮应当可见");
        cx.simulate_click(btn.center(), Modifiers::default());

        let (query, view) = cx.update(|_, cx| {
            let app = app.read(cx);
            (
                app.search.read(cx).value().to_string(),
                app.view.len(),
            )
        });
        assert!(query.is_empty(), "点 × 之后搜索框应当清空，实际 {query:?}");
        assert_eq!(view, 2, "清空搜索后列表应当恢复成 2 项");
    }

    /// 没有输入时那颗 × 不该占位（原版靠 `.hidden` 类切换）。
    #[gpui::test]
    fn search_clear_button_is_hidden_when_empty(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let _ = stage(cx, &held, "");
        assert!(
            cx.debug_bounds("clear-search").is_none(),
            "搜索框为空时不应渲染清除按钮"
        );
    }
}

/// 侧栏列表行的回归：文件名只能占一行，超出部分交给 gpui 的省略号收尾，
/// 不能折行把行高撑开（也就不会把每行的元信息往下挤）。
#[cfg(test)]
mod row_name_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Pixels, TestAppContext, VisualTestContext};

    use super::*;

    /// `Pixels` 的字段是私有的，量几何一律先换成 f32。
    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    /// 摆一个只有一条记录、文件夹路径也很长的列表，然后重画一帧。
    fn one_row(cx: &mut VisualTestContext, held: &Rc<RefCell<Option<Entity<App>>>>, name: &str) {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        let row = MediaFile {
            name: name.to_string(),
            path: format!("/demo/{name}"),
            ext: "mp4".to_string(),
            size: 1024,
            modified: 0,
            kind: "video".to_string(),
        };
        app.update_in(cx, |view, _window, cx| {
            view.files = vec![row];
            view.folder = Some(PathBuf::from("/demo/一个特别长的文件夹名字/再往里一层"));
            view.refresh_view(cx);
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui::test]
    fn long_file_name_takes_exactly_one_line(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let long = format!("{}.mp4", "一个特别长的视频文件名".repeat(5));
        one_row(cx, &held, &long);

        let name = cx.debug_bounds("row-name-0").expect("文件名应当被渲染");
        // 行高写死 17px：一旦折行就是 34 / 51 …
        assert!(
            (n(name.size.height) - 17.).abs() < 1.5,
            "超长文件名应当只占一行，实际高度 {}",
            n(name.size.height)
        );
        assert!(
            n(name.size.width) > 0. && n(name.right()) <= 252.,
            "文件名不该顶出 252px 宽的侧栏（right = {}）",
            n(name.right())
        );
    }

    /// 短名字也应当是一行 —— 别把正常情况改坏。
    #[gpui::test]
    fn short_file_name_still_one_line(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        one_row(cx, &held, "a.mp4");

        let name = cx.debug_bounds("row-name-0").expect("文件名应当被渲染");
        assert!(
            (n(name.size.height) - 17.).abs() < 1.5,
            "短名字同样只占一行，实际高度 {}",
            n(name.size.height)
        );
    }
}

/// 右上角"画面角度"这一组动作的回归：按钮能开面板、四个动作真的作用到画面上、
/// 「恢复原始」能回去、没有文件时不瞎改状态。
#[cfg(test)]
mod orient_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, Pixels, TestAppContext, VisualTestContext};

    use super::*;

    /// `Pixels` 的字段是私有的，量几何一律先换成 f32。
    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    /// 造一张 w×h 的 PNG：R 通道存 x、G 通道存 y，方便核对像素有没有搬错。
    fn temp_png(name: &str, w: u32, h: u32) -> PathBuf {
        let dir = std::env::temp_dir().join("iplayer-orient-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut src = image::RgbaImage::new(w, h);
        for (x, y, px_) in src.enumerate_pixels_mut() {
            *px_ = image::Rgba([(x * 60) as u8, (y * 90) as u8, 0, 255]);
        }
        src.save(&path).unwrap();
        path
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn click(cx: &mut VisualTestContext, sel: &'static str) {
        let b = cx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} 应当被渲染"));
        cx.simulate_click(b.center(), Modifiers::default());
        redraw(cx);
    }

    /// 展开面板。
    ///
    /// 不能靠 `simulate_click` 去点工具栏那颗按钮：标题栏整条挂着
    /// `start_window_move()`（拖窗口用），无头平台上它 `unimplemented!()`，
    /// 鼠标事件冒泡上去就 panic。所以面板开关直接走方法，
    /// 按钮本身只断言"渲染出来了"。
    fn open_panel(cx: &mut VisualTestContext, held: &Rc<RefCell<Option<Entity<App>>>>) -> Entity<App> {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, _window, cx| view.toggle_orient_panel(cx));
        redraw(cx);
        app
    }

    fn open(cx: &mut VisualTestContext, held: &Rc<RefCell<Option<Entity<App>>>>, path: PathBuf) -> Entity<App> {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, _window, cx| view.open_file(path, cx));
        redraw(cx);
        app
    }

    /// 舞台纹理的宽高与左上角那一个像素（BGRA）。
    fn stage_px(cx: &mut VisualTestContext, app: &Entity<App>) -> (u32, u32, [u8; 4]) {
        cx.update(|_, cx| match &app.read(cx).stage {
            Stage::Image(img) => {
                let w = img.size(0).width.0 as u32;
                let h = img.size(0).height.0 as u32;
                let b = img.as_bytes(0).unwrap();
                (w, h, [b[0], b[1], b[2], b[3]])
            }
            _ => (0, 0, [0; 4]),
        })
    }

    /// 右转 90° → 纹理宽高互换，且左上角来自原图的**左下角**（这就是"顺时针"）。
    #[gpui::test]
    fn orient_rotates_a_still_image(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = open(cx, &held, temp_png("cw.png", 4, 2));

        let (w, h, px_) = stage_px(cx, &app);
        assert_eq!((w, h), (4, 2), "打开时应当是原始尺寸");
        assert_eq!(px_, [0, 0, 0, 255], "原图左上角是 (x=0, y=0)");

        assert!(
            cx.debug_bounds("ctl-orient").is_some(),
            "控制条上应当有画面角度这颗按钮（已从标题栏挪过来）"
        );

        open_panel(cx, &held);
        click(cx, "orient-cw");

        let (w, h, px_) = stage_px(cx, &app);
        let o = cx.update(|_, cx| app.read(cx).orient);
        assert_eq!(o.rot, 90, "右转 90° 应当记在朝向里");
        assert_eq!((w, h), (2, 4), "旋转 90° 后宽高互换");
        assert_eq!(px_, [0, 90, 0, 255], "顺时针后左上角应当来自原图左下角");

        // 再左转 90° 转回来
        click(cx, "orient-ccw");
        let (w, h, px_) = stage_px(cx, &app);
        assert!(cx.update(|_, cx| app.read(cx).orient).is_identity(), "一转一还应当复位");
        assert_eq!((w, h, px_), (4, 2, [0, 0, 0, 255]));
    }

    /// 左右翻转 / 上下翻转 / 恢复原始都作用在**当前看到的画面**上。
    #[gpui::test]
    fn orient_flips_and_reset(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = open(cx, &held, temp_png("flip.png", 4, 2));

        open_panel(cx, &held);
        click(cx, "orient-fliph");
        // 左右翻转：左上角来自原图右上角（x=3 → R=180）
        let (w, h, px_) = stage_px(cx, &app);
        assert_eq!((w, h), (4, 2));
        assert_eq!(px_, [0, 0, 180, 255], "左右翻转后左上角应当是原图右上角");
        assert!(cx.update(|_, cx| app.read(cx).orient.flip_h));

        click(cx, "orient-flipv");
        let px_ = stage_px(cx, &app).2;
        assert_eq!(px_, [0, 90, 180, 255], "再上下翻转 → 来自原图右下角");

        click(cx, "orient-reset");
        let (w, h, px_) = stage_px(cx, &app);
        assert!(cx.update(|_, cx| app.read(cx).orient).is_identity());
        assert_eq!((w, h, px_), (4, 2, [0, 0, 0, 255]), "恢复原始应当回到原图");
    }

    /// 没打开任何文件时点它只弹提示，不该把朝向悄悄改掉、更不该崩。
    #[gpui::test]
    fn orient_needs_a_file(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = open_panel(cx, &held);
        click(cx, "orient-cw");

        let o = cx.update(|_, cx| app.read(cx).orient);
        assert!(o.is_identity(), "空舞台时不该改画面角度");
        assert!(
            cx.debug_bounds("orient-cw").is_some(),
            "面板应当还开着，方便用户再点"
        );
    }

    /// 角度面板贴在舞台**右下角**（用户指定，从右上角挪下来），
    /// 且与导出面板同位 —— 右缘、下缘到舞台边的距离应当一致。
    #[gpui::test]
    fn orient_panel_docks_bottom_right(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let _app = open_panel(cx, &held);

        let panel = cx
            .debug_bounds("orient-panel")
            .expect("展开开关后应当渲染角度面板");
        let stage = cx.debug_bounds("stage").expect("舞台应当被渲染");

        let right_gap = n(stage.origin.x + stage.size.width)
            - n(panel.origin.x + panel.size.width);
        let bottom_gap = n(stage.origin.y + stage.size.height) - n(panel.origin.y + panel.size.height);
        // 四个边距是同一个设计值（导出面板也是 12px），两条边得一致
        assert!(
            (right_gap - bottom_gap).abs() < 1.0,
            "面板右下内缩应当一致（右 {right_gap:.1} / 下 {bottom_gap:.1}）"
        );
        assert!(
            (8.0..16.0).contains(&right_gap),
            "面板应当贴住右下角（实测内缩 {right_gap:.1}px）"
        );
        assert!(
            n(panel.center().y) > n(stage.center().y),
            "面板应当落在舞台下半区（不再是右上角）"
        );
    }

    /// 面板开关：开一次有、再开一次没有；和导出面板互斥。
    #[gpui::test]
    fn orient_panel_toggles(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);

        let app = open_panel(cx, &held);
        assert!(cx.debug_bounds("orient-cw").is_some(), "应当展开面板");
        assert!(
            cx.update(|_, cx| app.read(cx).orient_panel),
            "开关状态要记在 App 上"
        );

        app.update_in(cx, |view, _window, cx| view.toggle_orient_panel(cx));
        redraw(cx);
        assert!(cx.debug_bounds("orient-cw").is_none(), "再开一次应当收起来");

        // 导出面板打开时，画面角度面板要让位（两个浮层都在右上/右下，别叠着）
        app.update_in(cx, |view, _window, cx| {
            view.toggle_orient_panel(cx);
            view.toggle_export_panel(cx);
        });
        let (o, e) = cx.update(|_, cx| {
            let a = app.read(cx);
            (a.orient_panel, a.export_panel)
        });
        assert!(!o && e, "开导出面板应当把画面角度面板收起来");
    }
}

/// 控制条布局的两态回归：侧栏展开时播放组贴着音量组靠右，
/// 收起时回到**整行**正中 —— 原版 CSS 就是这么切的
/// （`.ctrl-group.center { justify-self: end }` ↔ `sidebar-hidden` 的 `1fr auto 1fr`）。
#[cfg(test)]
mod control_bar_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Pixels, TestAppContext, VisualTestContext};

    use super::*;

    /// `Pixels` 的字段是私有的，量几何一律先换成 f32。
    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    /// 侧栏展开：播放组也固定在整行正中（不再贴着音量组靠右）。
    #[gpui::test]
    fn play_group_stays_centered_with_sidebar_open(cx: &mut TestAppContext) {
        let (cx, _held) = harness(cx);

        let row = cx.debug_bounds("ctrl-row").expect("控制条那一行应当被渲染");
        let play = cx.debug_bounds("play-group").expect("播放组应当被渲染");
        let vol = cx.debug_bounds("vol-cluster").expect("右侧控制组应当被渲染");

        let rel = (n(play.center().x) - n(row.origin.x)) / n(row.size.width);
        assert!(
            (0.35..0.65).contains(&rel),
            "播放组应当落在整行正中，实际在整行的 {:.0}% 处",
            rel * 100.0
        );

        let drift = (n(play.center().x) - n(row.center().x)).abs();
        assert!(
            drift < 8.0,
            "播放组偏离整行中心 {drift:.1}px，应当居中"
        );

        // 不再贴着音量组
        let gap = n(vol.origin.x) - (n(play.origin.x) + n(play.size.width));
        assert!(
            gap > 60.0,
            "播放组应当与音量组保持距离，实际间隔 {gap:.1}px"
        );
    }

    /// 侧栏收起：播放组位置不动，仍然整行居中。
    #[gpui::test]
    fn play_group_stays_centered_when_sidebar_collapsed(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = held.borrow().as_ref().expect("App 实体").clone();

        let open_play = cx.debug_bounds("play-group").expect("播放组应当被渲染");

        app.update_in(cx, |view, _, cx| {
            view.sidebar = false;
            cx.notify();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let row = cx.debug_bounds("ctrl-row").expect("控制条那一行应当被渲染");
        let play = cx.debug_bounds("play-group").expect("播放组应当被渲染");

        let drift = (n(play.center().x) - n(row.center().x)).abs();
        assert!(
            drift < 8.0,
            "收起侧栏后播放组仍应居中，实际偏离中心 {drift:.1}px"
        );

        let moved = (n(play.center().x) - n(open_play.center().x)).abs();
        assert!(
            moved < 8.0,
            "收起侧栏不应挪动播放组，实际水平位移 {moved:.1}px"
        );
    }
}

/// 保存面板这一层曾经是崩溃源头，这里把它跟帧循环的契约钉死：
/// `poll()` 必须**非阻塞**（还开着就立刻返回 `None`），选完了才交出路径。
///
/// 之所以要有这套异步写法的回归：以前用的是 rfd 的同步 `save_file()`，
/// 它内部 `runModal` 会在主线程再叠一层事件循环，而 GPUI 的帧源挂在主队列
/// 上、模态期间照样触发 —— 帧回调重入 GPUI 直接 panic，又因为站在
/// `extern "C"` 边界上没法 unwind，进程当场 abort。
#[cfg(test)]
mod export_dialog_tests {
    use std::future::pending;
    use std::path::PathBuf;

    use super::*;

    fn save(fut: impl Future<Output = Option<rfd::FileHandle>> + 'static) -> PendingSave {
        PendingSave {
            kind: export::Kind::Gif,
            src: PathBuf::from("/tmp/clip.mp4"),
            pos: 1.5,
            dur: 20.0,
            fut: Box::pin(fut),
        }
    }

    #[test]
    fn poll_is_non_blocking_while_the_panel_is_up() {
        let mut d = save(pending::<Option<rfd::FileHandle>>());
        // 面板没结束：一帧一次地轮询都必须是 None，绝不能卡住
        for _ in 0..5 {
            assert!(d.poll().is_none(), "面板还开着时 poll() 必须是 None");
        }
    }

    #[test]
    fn poll_hands_back_the_picked_path() {
        let mut d = save(async {
            Some(rfd::FileHandle::from(PathBuf::from("/tmp/out.gif")))
        });
        assert_eq!(d.poll(), Some(Some(PathBuf::from("/tmp/out.gif"))));
    }

    #[test]
    fn poll_reports_cancel() {
        let mut d = save(async { None });
        assert_eq!(d.poll(), Some(None), "取消要能和「还没选完」区分开");
    }
}


#[cfg(test)]
mod export_click_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn click(cx: &mut VisualTestContext, sel: &'static str) {
        let b = cx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} 应当被渲染"));
        cx.simulate_click(b.center(), Modifiers::default());
        redraw(cx);
    }

    fn n(v: gpui::Pixels) -> f32 {
        f32::from(v)
    }

    /// 工具箱面板三行都在、顺序是 截图 · 提取音频 · 转 GIF ——
    /// 控制条那颗截图按钮移除后，这里是截图**唯一**的入口，不能悄悄少一行。
    #[gpui::test]
    fn toolbox_keeps_all_three_export_rows(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |v, _w, cx| v.toggle_export_panel(cx));
        redraw(cx);

        let shot = cx.debug_bounds("export-snapshot").expect("截图行应当存在");
        let audio = cx.debug_bounds("export-audio").expect("提取音频行应当存在");
        let gif = cx.debug_bounds("export-gif").expect("转 GIF 行应当存在");
        assert!(
            n(shot.center().y) < n(audio.center().y) && n(audio.center().y) < n(gif.center().y),
            "三行应当自上而下依次是 截图 / 提取音频 / 转 GIF"
        );
        for other in [audio, gif] {
            assert!(
                (n(other.center().x) - n(shot.center().x)).abs() < 1.0,
                "三行应当左对齐在同一列"
            );
        }
    }

    /// 点"截图"绝不能把进程带走。
    ///
    /// 这里钉的是用户报的"点截图 / 转 GIF / 提取音频就崩"：rfd 的 macOS 后端在
    /// 它认为环境不支持时会**静默退回同步 `runModal`**，而回退路径要么直接
    /// panic（"Fallback Sync Dialog Must Be Spawned On Main Thread"），要么在
    /// 主线程里再套一层事件循环、把外层的 GPUI 事件分发重入 → 撞 ObjC 边界
    /// 无法 unwind → abort。所以 `request_export` 现在先自查环境、再
    /// `catch_unwind` 兜底：无论支持不支持，点击的结果只能是"弹面板"或"给提示"。
    ///
    /// 入口是**工具箱面板**里的那一行（控制条上的截图按钮已按用户要求移除）。
    #[gpui::test]
    fn clicking_snapshot_row_never_panics(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |v, _w, cx| {
            v.open_file(PathBuf::from("/tmp/selftest-audio.mp4"), cx)
        });
        redraw(cx);

        // 控制条上不该再有截图按钮
        assert!(
            cx.debug_bounds("shot").is_none(),
            "控制条上的截图按钮应当已移除（工具箱里保留入口）"
        );

        // 展开工具箱，点里面的"截图"行
        app.update_in(cx, |v, _w, cx| v.toggle_export_panel(cx));
        redraw(cx);
        click(cx, "export-snapshot");
        let (dialog, export, toast) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.dialog.is_some(),
                a.export.is_some(),
                a.toast.as_ref().map(|(m, _)| m.clone()),
            )
        });
        assert!(
            !export,
            "还没选保存位置，不该起 ffmpeg 任务（export={export:?}）"
        );
        if !dialog {
            // 走不了非阻塞面板时，必须留一句提示而不是闷声不动，更不能崩
            let msg = toast.expect("应当给一句提示");
            assert!(
                msg.contains("保存面板"),
                "提示要说明是保存面板的问题，实际是 {msg:?}"
            );
        }

        // 面板开着的话，再点一次也得是提示而不是崩
        if dialog {
            click(cx, "export-snapshot");
        }

        // 走完了还在，能正常读状态就说明没 abort
        let _ = cx.update(|_, cx| app.read(cx).export_panel);
    }

    /// `catch_unwind` 抓到的载荷要能变成一句人话（`panic!` 只会用这两种类型）。
    #[test]
    fn panic_payload_becomes_a_message() {
        let caught = std::panic::catch_unwind(|| panic!("模拟 rfd 内部 panic"));
        assert_eq!(panic_reason(&caught.unwrap_err()), "模拟 rfd 内部 panic");

        let caught = std::panic::catch_unwind(|| {
            panic!("{}", String::from("格式化的消息"))
        });
        assert_eq!(panic_reason(&caught.unwrap_err()), "格式化的消息");
    }
}

/// 舞台「点画面 = 播放 / 暂停」的回归。
///
/// 两个要点，缺一条都不算做完：
/// 1. 点画面真能切换播放状态（这是用户要的功能）；
/// 2. 右上角角度面板 / 右下角导出面板里的按钮**不许**把点击漏给舞台 ——
///    浮层是绝对定位的**兄弟**节点，gpui 的命中测试会把同一个点上的所有
///    hitbox 都收进来（`Window::hit_test` 里按绘制顺序反向遍历、直到有人
///    `BlockMouse`），所以祖先/兄弟都在一条冒泡链上，不 `stop_propagation`
///    就会出现"转个角度顺手把片子暂停了"。
#[cfg(test)]
mod stage_click_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext, point};

    use super::*;

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// 只有真解码出来的 `Player` 才有"播放 / 暂停"可切，所以借
    /// `player::tests` 那份缓存短片（同一目录，谁先跑谁造）；没有 ffmpeg
    /// 就当环境问题跳过，别把缺依赖算成回归。
    fn test_clip() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join("iplayer-scrub-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("clip.mp4");
        if !path.exists() {
            let mut cmd = crate::ffmpeg::base_command().ok()?;
            let ok = cmd
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc=size=160x120:rate=25:duration=4")
                .args(["-pix_fmt", "yuv420p", "-an", "-y"])
                .arg(&path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok || !path.exists() {
                return None;
            }
        }
        Some(path)
    }

    fn open_video(
        cx: &mut VisualTestContext,
        held: &Rc<RefCell<Option<Entity<App>>>>,
        clip: &PathBuf,
    ) -> Entity<App> {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, _window, cx| view.open_file(clip.clone(), cx));
        redraw(cx);
        app
    }

    fn is_playing(cx: &mut VisualTestContext, app: &Entity<App>) -> Option<bool> {
        cx.update(|_, cx| app.read(cx).player.as_ref().map(|p| p.is_playing()))
    }

    /// 点画面：播放 ↔ 暂停，来回都对。
    #[gpui::test]
    fn clicking_the_stage_toggles_playback(cx: &mut TestAppContext) {
        let Some(clip) = test_clip() else {
            return;
        };
        let (cx, held) = harness(cx);
        let app = open_video(cx, &held, &clip);
        let Some(playing) = is_playing(cx, &app) else {
            return; // 打开失败（探针 / 解码环境问题），不当回归
        };
        assert!(playing, "刚打开应当正在播放");

        let stage = cx.debug_bounds("stage").expect("舞台应当被渲染");
        cx.simulate_click(stage.center(), Modifiers::default());
        redraw(cx);
        assert_eq!(
            is_playing(cx, &app),
            Some(false),
            "点一下画面应当暂停（这是用户要的功能）"
        );

        cx.simulate_click(stage.center(), Modifiers::default());
        redraw(cx);
        assert_eq!(is_playing(cx, &app), Some(true), "再点一下应当继续播放");
    }

    /// 浮层面板里的点击要被面板自己吃掉，不能顺手把播放切了。
    #[gpui::test]
    fn clicks_inside_panels_do_not_toggle_playback(cx: &mut TestAppContext) {
        let Some(clip) = test_clip() else {
            return;
        };
        let (cx, held) = harness(cx);
        let app = open_video(cx, &held, &clip);
        if is_playing(cx, &app).is_none() {
            return;
        }

        // ① 角度面板：点「左转 90°」——动作要生效，播放状态要原地不动
        app.update_in(cx, |view, _window, cx| view.toggle_orient_panel(cx));
        redraw(cx);
        let row = cx
            .debug_bounds("orient-ccw")
            .expect("角度面板应当被渲染");
        cx.simulate_click(row.center(), Modifiers::default());
        redraw(cx);
        let (playing, rot) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.player.as_ref().map(|p| p.is_playing()),
                a.orient.rot,
            )
        });
        assert_eq!(rot, 270, "「左转 90°」应当作用到画面上");
        assert_eq!(playing, Some(true), "点角度面板不该顺带暂停播放");

        // ② 导出面板：点卡片顶部的标题区（三行动作之上），面板吃掉点击，
        //    既不弹导出面板、也不切播放状态。
        app.update_in(cx, |view, _window, cx| view.toggle_export_panel(cx));
        redraw(cx);
        let card = cx.debug_bounds("export-panel").expect("导出面板应当被渲染");
        let head = point(card.center().x, card.origin.y + px(15.));
        cx.simulate_click(head, Modifiers::default());
        redraw(cx);
        let (playing, dialog, export) = cx.update(|_, cx| {
            let a = app.read(cx);
            (
                a.player.as_ref().map(|p| p.is_playing()),
                a.dialog.is_some(),
                a.export.is_some(),
            )
        });
        assert_eq!(playing, Some(true), "点导出面板不该顺带暂停播放");
        assert!(
            !dialog && !export,
            "这里点的应当是面板标题（空白），没碰到动作行：dialog={dialog} export={export}"
        );
    }
}

/// 字幕（控制条「工具箱」左侧那颗 + 面板 + 画面上的字幕层）的回归。
///
/// 用户要求的是三件事：图标放在工具箱**左侧**、点开能**导入字幕**、能**隐藏字幕**。
/// 时间轴本身（SRT / VTT / ASS 的解析、`start <= t < end` 的取条）由
/// `crate::subtitle` 的单测钉住，这里只管 UI 这条路：按钮在哪、面板怎么开、
/// 字幕层什么时候出现、几点几何关系。
#[cfg(test)]
mod subtitle_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, Pixels, TestAppContext, VisualTestContext};

    use super::*;

    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    fn click(cx: &mut VisualTestContext, sel: &'static str) {
        let b = cx
            .debug_bounds(sel)
            .unwrap_or_else(|| panic!("{sel} 应当被渲染"));
        cx.simulate_click(b.center(), Modifiers::default());
        redraw(cx);
    }

    fn app_of(held: &Rc<RefCell<Option<Entity<App>>>>) -> Entity<App> {
        held.borrow().as_ref().expect("App 实体").clone()
    }

    /// 借 `player::tests` 那份缓存短片，复制成本测试专用的一份，
    /// 再在它旁边放一份**同名**字幕 —— 这样一次就把"自动找 sidecar"也测到。
    fn clip_with_subtitle() -> Option<(PathBuf, PathBuf)> {
        let src = std::env::temp_dir().join("iplayer-scrub-test").join("clip.mp4");
        if !src.exists() {
            return None; // 没有缓存短片（缺 ffmpeg）就跳过，不算回归
        }
        let dir = std::env::temp_dir().join("iplayer-sub-case");
        std::fs::create_dir_all(&dir).ok()?;
        let media = dir.join("sub-case.mp4");
        let sub = dir.join("sub-case.srt");
        std::fs::copy(&src, &media).ok()?;
        std::fs::write(
            &sub,
            "1\n00:00:00,000 --> 01:00:00,000\n测试字幕\n\n2\n01:00:01,000 --> 01:00:02,000\n很后面的一条\n",
        )
        .ok()?;
        Some((media, sub))
    }

    /// 面板开关 + 三个右下角浮层互斥（都在同一个角上，不能叠着）。
    #[gpui::test]
    fn subtitle_panel_toggles_and_shares_the_corner(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = app_of(&held);

        app.update_in(cx, |v, _w, cx| v.toggle_sub_panel(cx));
        redraw(cx);
        for sel in ["sub-import", "sub-toggle", "sub-size", "sub-remove"] {
            assert!(
                cx.debug_bounds(sel).is_some(),
                "字幕面板里应当有 {sel} 这一行"
            );
        }

        // 与角度 / 导出面板同位：右下内缩应当一致
        let panel = cx.debug_bounds("sub-panel").expect("字幕面板应当被渲染");
        let stage = cx.debug_bounds("stage").expect("舞台应当被渲染");
        let right_gap =
            n(stage.origin.x + stage.size.width) - n(panel.origin.x + panel.size.width);
        let bottom_gap =
            n(stage.origin.y + stage.size.height) - n(panel.origin.y + panel.size.height);
        assert!(
            (right_gap - bottom_gap).abs() < 1.0,
            "面板右下内缩应当一致（右 {right_gap:.1} / 下 {bottom_gap:.1}）"
        );
        assert!(
            (8.0..16.0).contains(&right_gap),
            "面板应当贴住右下角（实测内缩 {right_gap:.1}px）"
        );

        // 开导出面板 → 字幕面板让位；反过来也一样
        app.update_in(cx, |v, _w, cx| v.toggle_export_panel(cx));
        redraw(cx);
        let (sub, export) = cx.update(|_, cx| {
            let a = app.read(cx);
            (a.sub_panel, a.export_panel)
        });
        assert!(!sub && export, "开导出面板应当把字幕面板收起来");

        app.update_in(cx, |v, _w, cx| v.toggle_sub_panel(cx));
        redraw(cx);
        let (sub, export) = cx.update(|_, cx| {
            let a = app.read(cx);
            (a.sub_panel, a.export_panel)
        });
        assert!(sub && !export, "再开字幕面板应当把导出面板收起来");
    }

    /// 点「导入字幕…」绝不能把进程带走（同导出那套：非阻塞面板或一句提示）。
    #[gpui::test]
    fn clicking_import_row_never_panics(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = app_of(&held);
        app.update_in(cx, |v, _w, cx| v.toggle_sub_panel(cx));
        redraw(cx);
        click(cx, "sub-import");

        let (dialog, toast) = cx.update(|_, cx| {
            let a = app.read(cx);
            (a.sub_dialog.is_some(), a.toast.as_ref().map(|(m, _)| m.clone()))
        });
        if !dialog {
            let msg = toast.expect("走不了非阻塞面板时必须给一句提示");
            assert!(
                msg.contains("文件面板"),
                "提示要说明是文件面板的问题，实际是 {msg:?}"
            );
        }
        // 没有字幕时点显示 / 隐藏：只提示，不许崩、也不许把状态改成"已载入"
        click(cx, "sub-toggle");
        assert!(
            cx.update(|_, cx| app.read(cx).subs.is_none()),
            "没导入字幕时不该凭空出现字幕"
        );
    }

    /// 打开带同名字幕的视频：字幕层出现在画面下方；隐藏后消失；
    /// 移除后再（手动）载入又回来。
    #[gpui::test]
    fn subtitle_shows_hides_and_reloads(cx: &mut TestAppContext) {
        let Some((media, sub)) = clip_with_subtitle() else {
            return;
        };
        let (cx, held) = harness(cx);
        let app = app_of(&held);
        app.update_in(cx, |v, _w, cx| v.open_file(media.clone(), cx));
        redraw(cx);

        let has_player = cx.update(|_, cx| app.read(cx).player.is_some());
        if !has_player {
            return; // 探针 / 解码环境问题，不当回归
        }
        assert!(
            cx.update(|_, cx| app.read(cx).subs.is_some()),
            "同名字幕（sub-case.srt）应当被自动载入"
        );

        let bounds = cx
            .debug_bounds("subtitle")
            .expect("当前时刻有字幕，画面上应当叠出字幕层");
        let stage = cx.debug_bounds("stage").expect("舞台应当被渲染");
        // 横向居中
        let dx = (n(bounds.center().x) - n(stage.center().x)).abs();
        assert!(dx < 2.0, "字幕应当横向居中，偏了 {dx:.1}px");
        // 落在画面下半部，且没贴到最底边（下面还有控制条）
        let h = n(stage.size.height);
        let top_frac = (n(bounds.origin.y) - n(stage.origin.y)) / h;
        let bottom_frac = (n(bounds.origin.y) + n(bounds.size.height) - n(stage.origin.y)) / h;
        assert!(
            top_frac > 0.6,
            "字幕应当压在画面下方，实际从 {:.0}% 开始",
            top_frac * 100.0
        );
        assert!(
            bottom_frac < 0.99,
            "字幕不该贴到舞台最底边（实测底缘 {:.0}%）",
            bottom_frac * 100.0
        );

        // 隐藏 → 消失；再按一次 → 回来
        app.update_in(cx, |v, _w, cx| v.toggle_subtitles(cx));
        redraw(cx);
        assert!(
            cx.debug_bounds("subtitle").is_none(),
            "隐藏字幕后画面上不该还有那一层"
        );
        assert!(!cx.update(|_, cx| app.read(cx).sub_visible));
        app.update_in(cx, |v, _w, cx| v.toggle_subtitles(cx));
        redraw(cx);
        assert!(cx.debug_bounds("subtitle").is_some(), "再切一次应当显示回来");

        // 移除 → 层没了、状态也清了
        app.update_in(cx, |v, _w, cx| v.remove_subtitle(cx));
        redraw(cx);
        assert!(cx.debug_bounds("subtitle").is_none(), "移除后不该还有字幕层");
        assert!(cx.update(|_, cx| app.read(cx).subs.is_none()));

        // 手动载入（面板「导入字幕…」最终也是走这条路）→ 层回来
        app.update_in(cx, |v, _w, cx| v.load_subtitle(sub.clone(), cx));
        redraw(cx);
        assert!(
            cx.debug_bounds("subtitle").is_some(),
            "手动载入字幕后应当重新叠出来"
        );
        // 载入会把"显示"重新打开（隐藏状态下导入一份新的，用户是要看的）
        assert!(cx.update(|_, cx| app.read(cx).sub_visible));
    }

    /// 字号三档轮着来，并且真的反映在字幕层的尺寸上。
    #[gpui::test]
    fn subtitle_size_cycles(cx: &mut TestAppContext) {
        let Some((media, _sub)) = clip_with_subtitle() else {
            return;
        };
        let (cx, held) = harness(cx);
        let app = app_of(&held);
        app.update_in(cx, |v, _w, cx| v.open_file(media, cx));
        redraw(cx);
        if cx.update(|_, cx| app.read(cx).player.is_none()) {
            return;
        }
        let before = cx.debug_bounds("subtitle").expect("应当有字幕层");

        app.update_in(cx, |v, _w, cx| v.cycle_sub_size(cx)); // 中 → 大
        redraw(cx);
        let after = cx.debug_bounds("subtitle").expect("应当有字幕层");
        assert!(
            n(after.size.height) > n(before.size.height),
            "调到「大」之后字幕层应当更高：{:.1} → {:.1}",
            n(before.size.height),
            n(after.size.height)
        );
        assert_eq!(cx.update(|_, cx| app.read(cx).sub_size), 2);
        app.update_in(cx, |v, _w, cx| v.cycle_sub_size(cx)); // 大 → 小（转圈）
        assert_eq!(cx.update(|_, cx| app.read(cx).sub_size), 0);
    }

    /// 换文件时字幕跟着走：切到没有同名字幕的文件，旧字幕不许赖在屏幕上。
    #[gpui::test]
    fn switching_files_drops_the_old_subtitle(cx: &mut TestAppContext) {
        let Some((media, _sub)) = clip_with_subtitle() else {
            return;
        };
        let dir = std::env::temp_dir().join("iplayer-sub-case");
        let other = dir.join("no-subs.mp4");
        std::fs::copy(&media, &other).ok();
        let (cx, held) = harness(cx);
        let app = app_of(&held);
        app.update_in(cx, |v, _w, cx| v.open_file(media, cx));
        redraw(cx);
        if cx.update(|_, cx| app.read(cx).player.is_none()) {
            return;
        }
        assert!(cx.debug_bounds("subtitle").is_some());

        app.update_in(cx, |v, _w, cx| v.open_file(other, cx));
        redraw(cx);
        assert!(
            cx.update(|_, cx| app.read(cx).subs.is_none()),
            "新文件没有同名字幕，旧的不该留下"
        );
        assert!(cx.debug_bounds("subtitle").is_none(), "画面上不该还有旧字幕");
    }
}

/// 标题栏 / 控制条按钮排布的回归（用户明确指定的位置）：
/// 右上角从左到右 = 深浅色 · 置顶（最小化 / 最大化 / 关闭交给系统红绿灯）；
/// 循环右侧 = 切换方向 · 字幕 · 工具箱 · 最大化；「链条」（设默认播放器）与
/// 「截图」图标去掉（后者在工具箱面板里有入口）。全屏不再给按钮：
/// 绿色交通灯 + 快捷键 ⌘F 仍在。
#[cfg(test)]
mod toolbar_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Pixels, TestAppContext, VisualTestContext};

    use super::*;

    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn bounds(cx: &mut VisualTestContext, sel: &'static str) -> gpui::Bounds<Pixels> {
        cx.debug_bounds(sel).unwrap_or_else(|| panic!("{sel} 应当被渲染"))
    }

    /// 右上角这一排的成员与顺序：只剩 深浅色 · 置顶 两颗。
    #[gpui::test]
    fn titlebar_right_cluster_order(cx: &mut TestAppContext) {
        let (cx, _held) = harness(cx);

        let order = ["tb-theme", "tb-pin"];
        let mut prev: Option<gpui::Bounds<Pixels>> = None;
        for sel in order {
            let b = bounds(cx, sel);
            if let Some(p) = prev {
                let step = n(b.center().x) - n(p.center().x);
                assert!(
                    step > 20.0,
                    "{sel} 应当排在上一颗按钮右边（间隔 {step:.1}px）"
                );
            }
            prev = Some(b);
        }

        // 挪走的 / 去掉的都不该再出现在标题栏上
        for gone in ["tb-orient", "tb-export", "tb-full", "tb-default"] {
            assert!(
                cx.debug_bounds(gone).is_none(),
                "{gone} 不应该再被渲染（已挪到控制条 / 已删除）"
            );
        }

        // 最小化 / 最大化 / 关闭三颗已按用户要求整个移除：
        // 窗口这三件事交给 macOS 自带的红绿灯（左上角），别再长回去。
        for gone in ["tb-min", "tb-max", "tb-close"] {
            assert!(
                cx.debug_bounds(gone).is_none(),
                "{gone} 应当已从标题栏移除（用户要求：交给系统红绿灯）"
            );
        }
    }

    /// 底部控制条：循环右侧依次是 切换方向 · 字幕 · 工具箱，同一行、与循环键同尺寸。
    #[gpui::test]
    fn tools_sit_right_of_the_loop_button(cx: &mut TestAppContext) {
        let (cx, _held) = harness(cx);

        // 截图按钮已移除（工具箱里有），控制条上不该再有它
        assert!(
            cx.debug_bounds("shot").is_none(),
            "控制条上的截图按钮应当已移除"
        );

        // 字幕按钮（用户要求）夹在「切换方向」和「工具箱」中间；
        // 最右端是「最大化」（用户要求：右下角增加一个最大化图标）
        let loop_btn = bounds(cx, "loop");
        let order = ["ctl-orient", "ctl-sub", "ctl-export", "ctl-max"];
        let mut prev = loop_btn;
        for sel in order {
            let b = bounds(cx, sel);
            let step = n(b.center().x) - n(prev.center().x);
            assert!(step > 20.0, "{sel} 应当排在前一颗右边（间隔 {step:.1}px）");
            let dy = (n(b.center().y) - n(loop_btn.center().y)).abs();
            assert!(dy < 1.0, "{sel} 应当和循环按钮同一行（垂直偏差 {dy:.1}px）");
            prev = b;
        }

        // 工具按钮与循环键同尺寸（同一排工具，不要忽大忽小）
        let loop_b = bounds(cx, "loop");
        for sel in ["ctl-orient", "ctl-sub", "ctl-export", "ctl-max"] {
            let b = bounds(cx, sel);
            let diff = (n(b.size.width) - n(loop_b.size.width)).abs();
            assert!(diff < 1.0, "{sel} 应当与循环键同宽，差 {diff:.1}px");
        }

        // 工具组整体落在右侧音量组那一头，而不是挤到播放组中间去
        let row = bounds(cx, "ctrl-row");
        let last = n(prev.center().x);
        let right_edge = n(row.origin.x) + n(row.size.width);
        assert!(
            (right_edge - last).abs() < n(row.size.width) * 0.2,
            "工具组应当贴近行尾：行宽 {:.0}，最后一颗中心离行尾 {:.0}px",
            n(row.size.width),
            right_edge - last
        );
    }

    /// 「最大化」按钮的两种图标形态：没最大化是方框，最大化后是还原的双层框。
    #[test]
    fn maximize_glyph_follows_the_window_state() {
        assert_eq!(App::maximize_glyph(false), "maximize", "未最大化时应当是放大框");
        assert_eq!(App::maximize_glyph(true), "restore", "已最大化时应当是还原形态");
        // 两个名字都得在图标集里真存在（写错名字要到运行时才 panic）
        for name in [App::maximize_glyph(false), App::maximize_glyph(true)] {
            assert!(icons::source(name).is_some(), "图标集里没有 {name}");
        }
    }

    /// 点「最大化」的动作**无头测不了**：测试平台的 `PlatformWindow::zoom()`
    /// 是 `unimplemented!()`（`gpui-pre` 的 `platform/test/window.rs:451`），
    /// 三个真平台反而都实现了（macOS `zoom:` / Wayland、X11 最大化 / Windows
    /// `SW_MAXIMIZE`）。所以这里只钉两件能在无头环境里钉的事：
    /// 状态查询不炸、按钮真接上了动作。真机上的行为靠 GUI 冒烟截图核对。
    #[gpui::test]
    fn maximize_button_is_wired_and_state_read_is_safe(cx: &mut TestAppContext) {
        let (cx, _held) = harness(cx);

        // 无头平台没有原生窗口：状态查询要老老实实返回 false，而不是炸
        assert!(
            !cx.update(|window, _| native::is_window_zoomed(window)),
            "无头测试里没有 NSWindow，状态位应当为 false"
        );

        // 按钮画出来了，而且真的接上了动作（不是个好看的摆设）
        assert!(cx.debug_bounds("ctl-max").is_some(), "ctl-max 应当被渲染");
        let src = include_str!("app.rs");
        let at = src.find("\"ctl-max\"").expect("控制条上应当有 ctl-max");
        let tail = &src[at..(at + 900).min(src.len())];
        assert!(
            tail.contains(&concat!("this.toggle_maxi", "mize(window, cx)")),
            "ctl-max 的点击回调应当调 toggle_maximize"
        );
    }

    /// 「最大化」必须走 GPUI 的**异步**路径（`Window::zoom_window()`）。
    ///
    /// 踩过的坑：在点击回调里同步调 AppKit 的缩放 —— 那一刻 App 正被借用，
    /// AppKit 改完窗口尺寸回调 `set_frame_size`，GPUI 的 `bounds_changed` 被
    /// `.log_err()` 悄悄吞掉，于是「窗口变大了，但里面的播放区还是小的」。
    /// 这条测试盯住 `toggle_maximize` 只用 GPUI 那条异步路径，且不碰 `native::`
    ///（读窗口状态是渲染路径的事，不在这个函数里）。
    #[test]
    fn maximize_goes_through_the_async_gpui_path() {
        let src = include_str!("app.rs");
        let sig = "    fn toggle_maximize(";
        let start = src.find(sig).expect("toggle_maximize 应当存在");
        let rest = &src[start + 1..];
        let end = match rest.find("\n    fn ") {
            Some(i) => start + 1 + i,
            None => src.len(),
        };
        let body = &src[start..end];
        // 只扫代码：注释里出现关键词（比如解释"为什么不能用 native"）不算违规
        let code: String = body
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        // 拼出来是为了让"要扫的字符串"不出现在这条测试自己的源码里
        let ok = concat!("window.zoom_", "window()");
        assert!(
            code.contains(ok),
            "toggle_maximize 应当调 {ok}（GPUI 内部把 AppKit 调用丢到前台执行器上，异步才安全）"
        );
        assert!(
            !code.contains("native::"),
            "toggle_maximize 里不该出现 native:: —— 同步调 AppKit 会让布局卡在旧尺寸"
        );

        // native 里也不许再有"同步 zoom"这种手艺活儿
        let native_src = include_str!("native.rs");
        let sync_zoom = concat!("win.zoom", "(");
        assert!(
            !native_src.contains(sync_zoom),
            "native.rs 里不该再有同步的 AppKit 缩放调用（{sync_zoom}）"
        );
    }
}

/// 默认窗宽（1180×760）下右侧这排**不能**挤到播放组 —— 1920 的测试窗太宽，
/// 旧断言（间隔 > 60px）在那里是空过的；真窗宽 + 侧栏展开才是挤爆的地方。
#[cfg(test)]
mod toolbar_width_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use std::ops::Deref as _;

    use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, WindowBounds, WindowOptions, size};

    use super::*;

    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    /// 与 `add_window_view` 同款，但用**真实的默认窗宽**开窗。
    fn harness_at_default_size(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Default::default(),
                size: size(px(1180.), px(760.)),
            })),
            ..Default::default()
        };
        let window = cx.update(|cx| {
            cx.open_window(opts, |window, cx| {
                cx.new(|cx| {
                    let app = App::new(window, cx);
                    *slot.borrow_mut() = Some(cx.entity());
                    app
                })
            })
            .expect("测试窗口应当开得出来")
        });
        let mut v = VisualTestContext::from_window(*window.deref(), cx);
        v.update(|window, cx| window.draw(cx).clear(cx));
        let cx = v.into_mut();
        (cx, held)
    }

    #[gpui::test]
    fn right_cluster_never_crowds_the_play_group(cx: &mut TestAppContext) {
        let (cx, _held) = harness_at_default_size(cx);

        let b = |cx: &mut VisualTestContext, sel: &'static str| {
            cx.debug_bounds(sel)
                .unwrap_or_else(|| panic!("{sel} 应当被渲染"))
        };
        let row = b(cx, "ctrl-row");
        let play = b(cx, "play-group");
        let vol = b(cx, "vol-cluster");

        // 播放组仍然整行居中
        let drift = (n(play.center().x) - n(row.center().x)).abs();
        assert!(drift < 8.0, "播放组偏离整行中心 {drift:.1}px");

        // 右侧工具组不得压到播放组，且要留出呼吸空间
        let gap = n(vol.origin.x) - (n(play.origin.x) + n(play.size.width));
        assert!(
            gap >= 24.0,
            "默认窗宽下播放组与右侧工具组只剩 {gap:.1}px —— 右排太宽了"
        );
    }
}

/// 音乐舞台中央那行曲名的回归（用户要求「音乐播放时播放区域中间显示歌曲名称」）：
/// 名字怎么取、什么时候显示、显示在哪儿。
#[cfg(test)]
mod now_playing_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Pixels, TestAppContext, VisualTestContext};

    use super::*;

    fn n(v: Pixels) -> f32 {
        f32::from(v)
    }

    // ── 曲名怎么取 ─────────────────────────────────────────────────────

    /// 容器标签里的 title 优先，文件名兜底。
    #[test]
    fn title_comes_from_tags_then_from_the_file_name() {
        assert_eq!(
            song_title(Some(" 晴天 "), Some("/music/01 - track.mp3")),
            Some("晴天".to_string()),
            "有标签就用标签，且要去掉两端的空白"
        );
        assert_eq!(
            song_title(Some("   "), Some("/music/01 - track.mp3")),
            Some("01 - track".to_string()),
            "标签是空白字符串 = 没标签，退回文件名（去掉扩展名）"
        );
        assert_eq!(
            song_title(None, Some("/music/no-extension")),
            Some("no-extension".to_string()),
            "没有扩展名时文件名原样用"
        );
        assert_eq!(
            song_title(None, Some("/music/.hidden")),
            Some(".hidden".to_string()),
            "点开头的隐藏文件也得有个名字，别退成空"
        );
        assert_eq!(song_title(None, None), None, "路径都没有就没什么可显示的");
        assert_eq!(song_title(Some(""), None), None, "空标签 + 无路径 = 不显示");
    }

    // ── 什么时候显示、显示在哪儿 ───────────────────────────────────────

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    /// 自造一段音频（内置 ffmpeg；机器上没有就跳过）。文件名 `tone.wav`
    /// 同时也是这条测试期望的曲名 —— 它没有标签。
    fn test_tone() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join("iplayer-scrub-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("tone.wav");
        if !path.exists() {
            let mut cmd = crate::ffmpeg::base_command().ok()?;
            let ok = cmd
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg("sine=frequency=440:duration=3")
                .args(["-ac", "2", "-y"])
                .arg(&path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok || !path.exists() {
                return None;
            }
        }
        Some(path)
    }

    /// 自造一段**带标签**的音频（文件名故意叫 track，曲名在标签里）。
    fn test_tagged_tone() -> Option<PathBuf> {
        let dir = std::env::temp_dir().join("iplayer-scrub-test");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("track.m4a");
        if !path.exists() {
            let mut cmd = crate::ffmpeg::base_command().ok()?;
            let ok = cmd
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg("sine=frequency=660:duration=1")
                .args(["-ac", "2", "-c:a", "aac", "-metadata"])
                .arg("title=晴天")
                .arg("-y")
                .arg(&path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok || !path.exists() {
                return None;
            }
        }
        Some(path)
    }

    /// 标签里的曲名要真能经 ffprobe 走到 `MediaInfo.title`（不然后面那条
    /// 「标签优先」的规则只是空转）。
    #[test]
    fn tagged_title_reaches_media_info() {
        let Some(tagged) = test_tagged_tone() else {
            return;
        };
        let Ok(info) = crate::media::probe(&tagged) else {
            return;
        };
        assert_eq!(info.title, "晴天", "容器标签里的 title 应当被读出来");
        assert_eq!(
            song_title(Some(&info.title), Some(&info.path)),
            Some("晴天".to_string()),
            "有标签就显示标签，而不是文件名 track"
        );
    }

    fn open(
        cx: &mut VisualTestContext,
        held: &Rc<RefCell<Option<Entity<App>>>>,
        path: &PathBuf,
    ) -> Entity<App> {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, _window, cx| view.open_file(path.clone(), cx));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        app
    }

    /// 播音乐：中间要有曲名，横向居中、纵向压在频谱柱上方。
    #[gpui::test]
    fn music_shows_the_title_above_the_spectrum(cx: &mut TestAppContext) {
        let Some(tone) = test_tone() else {
            return; // 没有 ffmpeg：跳过，别把环境问题算成回归
        };
        let (cx, held) = harness(cx);
        let app = open(cx, &held, &tone);

        let (stage_kind, title) = cx.update(|_, cx| {
            let a = app.read(cx);
            (matches!(a.stage, Stage::Audio), a.audio_title())
        });
        if !stage_kind {
            return; // 探针 / 解码环境问题
        }
        assert_eq!(
            title.as_deref(),
            Some("tone"),
            "没有标签的音频就用文件名当曲名"
        );

        let bounds = cx
            .debug_bounds("now-playing")
            .expect("音频舞台上应当有曲名浮层");
        let stage = cx.debug_bounds("stage").expect("舞台应当被渲染");

        // 横向居中（整幅铺开的浮层 + 内容居中）
        let dx = (n(bounds.center().x) - n(stage.center().x)).abs();
        assert!(dx < 2.0, "曲名应当横向居中，偏了 {dx:.1}px");

        // 纵向：在播放区上半部（不是贴着顶边），但完全落在频谱柱区（41% 起）之上
        let h = n(stage.size.height);
        let bar_top = n(stage.origin.y) + h * 0.41;
        let title_bottom = n(bounds.origin.y) + n(bounds.size.height);
        assert!(
            title_bottom < bar_top,
            "曲名底缘 {title_bottom:.0} 越过了频谱柱区顶部 {bar_top:.0} —— 会被柱子穿过"
        );
        let top_frac = (n(bounds.center().y) - n(stage.origin.y)) / h;
        assert!(
            (0.20..0.45).contains(&top_frac),
            "曲名应当落在播放区中部偏上（实测 {:.0}%），既不能在正中挨柱子，也不能贴顶",
            top_frac * 100.0
        );
    }

    /// 视频不叠这层：曲名只在纯音频舞台出现。
    #[gpui::test]
    fn video_stage_has_no_title_overlay(cx: &mut TestAppContext) {
        let dir = std::env::temp_dir().join("iplayer-scrub-test");
        let clip = dir.join("clip.mp4");
        if !clip.exists() {
            return; // 借 player::tests 那份缓存短片，没有就跳过
        }
        let (cx, held) = harness(cx);
        let app = open(cx, &held, &clip);
        let stage_kind = cx.update(|_, cx| {
            let a = app.read(cx);
            matches!(a.stage, Stage::Audio)
        });
        if stage_kind {
            return; // 打开失败 / 环境问题
        }
        assert!(
            cx.debug_bounds("now-playing").is_none(),
            "视频舞台上不该出现音乐曲名那一层"
        );
    }
}

/// 图标弹框（角度 / 工具箱 / 字幕）停靠超时的回归（用户要求
/// 「鼠标停止点击 10s 后自动消失，而不是一直停靠」）：
/// 到点自动收、没到点不动、有动作就重新计时。
#[cfg(test)]
mod panel_timeout_tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui::{Modifiers, TestAppContext, VisualTestContext};

    use super::*;

    fn harness(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, Rc<RefCell<Option<Entity<App>>>>) {
        cx.update(gpui_kit::init);
        let held: Rc<RefCell<Option<Entity<App>>>> = Rc::new(RefCell::new(None));
        let slot = held.clone();
        let (_, cx) = cx.add_window_view(move |window, cx| {
            let app = App::new(window, cx);
            *slot.borrow_mut() = Some(cx.entity());
            app
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));
        (cx, held)
    }

    fn redraw(cx: &mut VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// 把"最近活动时刻"拨回 `secs` 秒前，模拟"这么久没人碰过"。
    fn rewind(cx: &mut VisualTestContext, held: &Rc<RefCell<Option<Entity<App>>>>, secs: u64) {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, _, _| {
            view.panel_seen = Some(Instant::now() - Duration::from_secs(secs));
        });
    }

    fn seen_age(cx: &mut VisualTestContext, held: &Rc<RefCell<Option<Entity<App>>>>) -> Option<f32> {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        cx.update(|_, cx| {
            app.read(cx)
                .panel_seen
                .map(|t| Instant::now().saturating_duration_since(t).as_secs_f32())
        })
    }

    /// 三个面板各自的开关方法 + 面板选择器，一把梭测。
    const KINDS: [(&str, fn(&mut App, &mut Context<App>)); 3] = [
        ("orient-panel", |a, cx| a.toggle_orient_panel(cx)),
        ("export-panel", |a, cx| a.toggle_export_panel(cx)),
        ("sub-panel", |a, cx| a.toggle_sub_panel(cx)),
    ];

    fn open(cx: &mut VisualTestContext, held: &Rc<RefCell<Option<Entity<App>>>>, i: usize) -> &'static str {
        let app = held.borrow().as_ref().expect("App 实体").clone();
        app.update_in(cx, |view, _, cx| (KINDS[i].1)(view, cx));
        redraw(cx);
        KINDS[i].0
    }

    /// 停够 `PANEL_TTL` 就自己收 —— 三个面板都得认。
    #[gpui::test]
    fn every_panel_closes_after_the_idle_timeout(cx: &mut TestAppContext) {
        for i in 0..KINDS.len() {
            let (cx, held) = harness(cx);
            let sel = open(cx, &held, i);
            assert!(
                cx.debug_bounds(sel).is_some(),
                "{sel} 应当被渲染（刚点开）"
            );

            rewind(cx, &held, PANEL_TTL.as_secs() + 1);
            redraw(cx);

            assert!(
                cx.debug_bounds(sel).is_none(),
                "{sel} 停靠超过 {} 秒应当自动消失，不该一直挂着",
                PANEL_TTL.as_secs()
            );
            let app = held.borrow().as_ref().expect("App 实体").clone();
            let still_open = cx.update(|_, cx| app.read(cx).panel_open());
            assert!(!still_open, "自动收起后三个面板标记都该是关的");
            let age = seen_age(cx, &held);
            assert_eq!(age, None, "全关之后计时表要停下（否则鼠标一动又记一笔）");
        }
    }

    /// 还没到点不许提前收（否则就成"闪一下就没了"）。
    #[gpui::test]
    fn panel_survives_until_the_timeout(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let sel = open(cx, &held, 1); // 工具箱

        rewind(cx, &held, PANEL_TTL.as_secs() - 2);
        redraw(cx);
        assert!(
            cx.debug_bounds(sel).is_some(),
            "才过了 {} 秒，面板不该提前消失",
            PANEL_TTL.as_secs() - 2
        );
    }

    /// 鼠标一动就把表重新拨回此刻 —— 所以在用的人不会被打断。
    #[gpui::test]
    fn mouse_activity_postpones_the_timeout(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let sel = open(cx, &held, 2); // 字幕面板

        // 先假装已经停靠了 11 秒（再过一帧就会被收掉）……
        rewind(cx, &held, PANEL_TTL.as_secs() + 1);
        redraw(cx);
        assert!(cx.debug_bounds(sel).is_none(), "先确认这个状态下确实会收");

        // ……重新打开，拨到"马上到点"，然后动一下鼠标：应当又从头计时。
        open(cx, &held, 2);
        rewind(cx, &held, PANEL_TTL.as_secs() - 1);
        cx.simulate_mouse_move(gpui::point(px(300.), px(300.)), None, Modifiers::default());
        redraw(cx);

        let age = seen_age(cx, &held).expect("面板还开着，计时表就得有值");
        assert!(
            age < 1.0,
            "鼠标动作应当把停靠计时重新拨到此刻，实测还是 {age:.1}s 前"
        );
        assert!(
            cx.debug_bounds(sel).is_some(),
            "刚动过鼠标，面板不该消失"
        );
    }

    /// 点窗口里任意一处也算动作（面板按钮之外的地方同样刷新）。
    #[gpui::test]
    fn any_click_counts_as_activity(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        open(cx, &held, 2);
        rewind(cx, &held, PANEL_TTL.as_secs() - 1);

        cx.simulate_click(gpui::point(px(300.), px(300.)), Modifiers::default());
        redraw(cx);

        let age = seen_age(cx, &held).expect("面板还开着，计时表就得有值");
        assert!(age < 1.0, "点一下窗口应当重新计时，实测还是 {age:.1}s 前");
    }

    /// 手动关掉面板后计时表要停 —— 否则鼠标一动就重新记，白留一个历史值。
    #[gpui::test]
    fn closing_by_hand_stops_the_clock(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = held.borrow().as_ref().expect("App 实体").clone();
        open(cx, &held, 0);
        assert!(seen_age(cx, &held).is_some(), "开着的时候得有计时");

        app.update_in(cx, |view, _, cx| view.toggle_orient_panel(cx));
        redraw(cx);
        assert_eq!(seen_age(cx, &held), None, "手动关掉后计时表该停");
    }

    /// Esc 仍然是"先收面板"：面板开着时按一下只收面板，不顺手把侧栏也切了。
    #[gpui::test]
    fn escape_closes_the_panel_without_touching_the_sidebar(cx: &mut TestAppContext) {
        let (cx, held) = harness(cx);
        let app = held.borrow().as_ref().expect("App 实体").clone();
        assert!(cx.update(|_, cx| app.read(cx).sidebar), "侧栏默认是展开的");

        open(cx, &held, 1); // 工具箱
        cx.simulate_keystrokes("escape");
        redraw(cx);

        assert!(
            cx.debug_bounds("export-panel").is_none(),
            "Esc 应当收起面板"
        );
        assert!(cx.update(|_, cx| app.read(cx).sidebar), "侧栏不该被一起切掉");
        assert_eq!(seen_age(cx, &held), None, "Esc 收面板后计时表也该停");
    }
}
