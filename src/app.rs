//! iPlayer 主界面。
//!
//! 布局照抄原版：自定义标题栏 / 左侧文件列表 + 舞台 / 底部控制条。
//! 画面走 [`crate::player`] 的纯原生渲染（ffmpeg 解成 BGRA 直接上屏），
//! 不经过任何 GPU 解码或 HTML 层。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::slider::{Slider, SliderEvent, SliderState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Div, Entity, ExternalPaths, FocusHandle, Focusable,
    ImageSource, InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, ObjectFit,
    ParentElement as _, Render, RenderImage, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, StyledImage as _, Subscription, Window, div, img, px,
};

use crate::export::{self, Export};
use crate::icons;
use crate::media::{self, MediaFile, MediaInfo};
use crate::native;
use crate::player::Player;
use crate::theme::{self, Palette};
use crate::viz;

/// 空舞台的 logo。`include_bytes!` 编进二进制，`.app` 里不需要带 assets/。
fn load_logo() -> Option<Arc<RenderImage>> {
    let img = image::load_from_memory(include_bytes!("../assets/logo.png")).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some(crate::player::image_from_straight_rgba(
        rgba.into_raw(),
        w,
        h,
    ))
}

/// 拖动进度条时两次真正 seek 之间的最小间隔。每次 seek 都要重开 ffmpeg，
/// 不节流的话手一抖就能把解码进程打爆。
const SCRUB_INTERVAL: Duration = Duration::from_millis(90);
/// seek / 换文件之后多渲染几帧，等新画面从解码线程里出来。
const WARMUP_FRAMES: u8 = 24;
/// 提示条存活时长。
const TOAST_TTL: Duration = Duration::from_secs(3);


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

/// 画面在舞台里的摆放方式。对应 GPUI 的 [`ObjectFit`]。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FitMode {
    /// 完整可见，留黑边
    Contain,
    /// 铺满，超出部分裁掉
    Cover,
    /// 拉伸铺满，不管比例
    Fill,
    /// 原始像素大小，不缩放
    Original,
}

impl FitMode {
    fn next(self) -> Self {
        match self {
            FitMode::Contain => FitMode::Cover,
            FitMode::Cover => FitMode::Fill,
            FitMode::Fill => FitMode::Original,
            FitMode::Original => FitMode::Contain,
        }
    }

    fn label(self) -> &'static str {
        match self {
            FitMode::Contain => "适应",
            FitMode::Cover => "裁切",
            FitMode::Fill => "拉伸",
            FitMode::Original => "原始",
        }
    }

    fn object_fit(self) -> ObjectFit {
        match self {
            FitMode::Contain => ObjectFit::Contain,
            FitMode::Cover => ObjectFit::Cover,
            FitMode::Fill => ObjectFit::Fill,
            FitMode::Original => ObjectFit::None,
        }
    }
}

/// 舞台上正在显示的东西。
enum Stage {
    Empty,
    /// 纯音频封面页
    Audio(String),
    /// 静态图片
    Image(Arc<RenderImage>),
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
    /// 舞台上是否叠一层媒体信息
    show_info: bool,
    /// 当前播的是不是动画 GIF（这种文件天生就该循环）
    looping_gif: bool,
    /// 画面摆放方式
    fit: FitMode,
    /// 窗口是否全屏 / 是否置顶
    fullscreen: bool,
    pinned: bool,
    /// 刚点完全屏的时刻。动画期间不去读系统状态，免得来回打架。
    fs_settle: Option<Instant>,
    /// 导出面板是否展开
    export_panel: bool,
    /// 正在跑的导出任务（同一时刻只允许一个）
    export: Option<Export>,
    toast: Option<(String, Instant)>,
    warmup: u8,
    /// 空舞台中央的品牌 logo（编译期嵌进二进制，打包不用带 assets/）
    logo: Option<Arc<RenderImage>>,
    /// 音频舞台的“均衡器”可视化状态
    viz: viz::Viz,
    /// 上一帧 viz.step 的时间，用来算 dt
    viz_last: Option<Instant>,
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
                // 拖动过程中节流，松手时（Release）才精确定位
                if this.last_scrub.elapsed() >= SCRUB_INTERVAL {
                    this.last_scrub = Instant::now();
                    this.seek_to_ratio(v.start() as f64, cx);
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
            show_info: true,
            looping_gif: false,
            fit: FitMode::Contain,
            fullscreen: false,
            pinned: false,
            fs_settle: None,
            export_panel: false,
            export: None,
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
        // 换文件 / 停止：均衡器的柱子和音符一并清场
        self.viz.reset();
        self.viz_last = None;
    }

    fn open_file(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.release();

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
                        self.stage = Stage::Image(image);
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
        let audio_codec = format!(
            "{} · {} Hz · {} 声道",
            info.acodec, info.sample_rate, info.channels
        );

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
                    Stage::Audio(audio_codec)
                };
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

    fn seek_to_ratio(&mut self, ratio: f64, cx: &mut Context<Self>) {
        let Some(p) = &self.player else { return };
        let dur = p.duration();
        if dur > 0.0 {
            self.seek_to((ratio.clamp(0.0, 1.0)) * dur, cx);
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
        // 滑块要跟着回到 0 / 原值
        self.volume
            .update(cx, |st, cx| st.set_value(applied * 100.0, window, cx));
        self.toast(if self.muted { "已静音" } else { "已取消静音" }, cx);
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
        if native::set_always_on_top(window, next) {
            self.pinned = next;
            self.toast(if next { "窗口已置顶" } else { "已取消置顶" }, cx);
        } else {
            self.toast("当前平台不支持窗口置顶", cx);
        }
        cx.notify();
    }

    fn cycle_fit(&mut self, cx: &mut Context<Self>) {
        self.fit = self.fit.next();
        let msg = format!("画面：{}", self.fit.label());
        self.toast(msg, cx);
    }

    // ── 导出 ────────────────────────────────────────────────────────────

    fn toggle_export_panel(&mut self, cx: &mut Context<Self>) {
        self.export_panel = !self.export_panel;
        cx.notify();
    }

    /// 起一个导出任务：先让用户选保存位置，再丢到后台线程去跑。
    fn start_export(&mut self, kind: export::Kind, window: &mut Window, cx: &mut Context<Self>) {
        if self.export.is_some() {
            self.toast("上一个导出还没结束", cx);
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
        if kind == export::Kind::Gif {
            if dur > 0.0 && dur - pos < 0.5 {
                self.toast("已经到结尾了，先把进度往前拖一点", cx);
                return;
            }
        }

        let tag = (kind == export::Kind::Snapshot).then(|| export::safe_tag(&media::format_time(pos)));
        let suggested = export::default_dest(&src, kind, tag.as_deref());
        let (name, exts) = kind.filter();
        let picked = rfd::FileDialog::new()
            .set_directory(suggested.parent().unwrap_or(&src))
            .set_file_name(
                suggested
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default(),
            )
            .add_filter(name, exts)
            .save_file();
        let Some(dest) = picked else {
            return; // 用户点了取消
        };

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

    /// 每帧推进：更新进度、处理播完、驱动下一帧。
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focused_once {
            self.focused_once = true;
            window.focus(&self.focus, cx);
        }

        if let Some((_, at)) = &self.toast {
            if at.elapsed() > TOAST_TTL {
                self.toast = None;
            }
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

        // 只要还在播（或刚 seek 完等着新画面、或后台在编码）就继续要帧。
        // 注意：`cx.notify()` 唤不醒平台帧源，视频必须显式 request。
        let busy = self.player.as_ref().is_some_and(|p| p.is_playing());

        // 音频舞台：推进“均衡器”可视化（真实频谱走 Tap，静默退合成）。
        // 舞台尺寸拿不到精确值就按视口扣掉侧栏/标题栏/控制条估一个 ——
        // 柱子与音符都用占比坐标，估差只影响每根柱的最小高度，可接受。
        if matches!(self.stage, Stage::Audio(_)) && self.player.is_some() {
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
                if self.export_panel {
                    self.export_panel = false;
                } else {
                    self.sidebar = !self.sidebar;
                }
                cx.notify();
            }
            "p" if !m.control => self.step(-1, cx),
            "n" if !m.control => self.step(1, cx),
            "." if m.control => self.stop(window, cx),
            "t" if m.control => {
                self.dark = !self.dark;
                cx.notify();
            }
            "o" if m.control => self.pick_folder(cx),
            "f" if !m.control => self.toggle_fullscreen(window, cx),
            "t" if !m.control => self.toggle_pin(window, cx),
            "a" if !m.control => self.cycle_fit(cx),
            "e" if !m.control => self.toggle_export_panel(cx),
            _ => {}
        }
    }

    fn pick_folder(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
            self.load_dir(dir, cx);
        }
    }

    fn pick_file(&mut self, cx: &mut Context<Self>) {
        let picked = rfd::FileDialog::new()
            .add_filter(
                "媒体文件",
                &[
                    "mp4", "mkv", "mov", "avi", "flv", "wmv", "webm", "ts", "rmvb", "rm", "mpg",
                    "mpeg", "m2ts", "mp3", "flac", "m4a", "wav", "ape", "aac", "ogg", "opus", "jpg",
                    "jpeg", "png", "gif", "webp", "bmp",
                ],
            )
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
            .child(self.icon_el(icon, size * 0.55, color))
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
        div()
            .id(id)
            .flex_none()
            .px(px(8.))
            .py(px(3.))
            .rounded_md()
            .text_size(px(11.))
            .cursor_pointer()
            .when(active, |d| d.bg(pal.accent()).text_color(pal.on_accent()))
            .when(!active, |d| {
                d.text_color(pal.muted()).hover(|s| s.bg(pal.hover()))
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
                    .child(
                        self.icon_btn_c("tb-fit", "crop", 26., pal.muted(), false)
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_fit(cx))),
                    )
                    .child(
                        self.icon_btn_c(
                            "tb-export",
                            "toolbox",
                            26.,
                            self.tb_color(self.export_panel),
                            self.export_panel,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_export_panel(cx))),
                    )
                    .child(
                        self.icon_btn_c("tb-pin", "pin", 26., self.tb_color(self.pinned), self.pinned)
                            .on_click(cx.listener(|this, _, window, cx| this.toggle_pin(window, cx))),
                    )
                    .child(
                        self.icon_btn_c(
                            "tb-full",
                            "expand",
                            26.,
                            self.tb_color(self.fullscreen),
                            self.fullscreen,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_fullscreen(window, cx))),
                    ),
            )
            .child(
                self.icon_btn("tb-theme", if self.dark { "sun" } else { "moon" }, 26.)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.dark = !this.dark;
                        cx.notify();
                    })),
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
                                div()
                                    .flex_1()
                                    .text_size(px(12.5))
                                    .line_height(px(17.))
                                    .text_color(pal.text())
                                    .overflow_hidden()
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
                    .overflow_hidden()
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

    fn render_stage(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let pal = self.pal();
        let fit = self.fit.object_fit();
        let frame = self.player.as_mut().and_then(|p| p.frame());

        let body: AnyElement = if let Some(image) = frame {
            img(ImageSource::Render(image))
                .object_fit(fit)
                .size_full()
                .into_any_element()
        } else {
            match &self.stage {
                Stage::Image(image) => img(ImageSource::Render(image.clone()))
                    .object_fit(fit)
                    .size_full()
                    .into_any_element(),
                Stage::Audio(codec) => {
                    let title = self
                        .active_path
                        .as_deref()
                        .and_then(|p| std::path::Path::new(p).file_name())
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    // 均衡器可视化铺底（音符在下、频谱柱在上），曲名信息再叠其上
                    let viz_el = self.viz.render(pal.text());
                    div()
                        .relative()
                        .size_full()
                        .child(viz_el)
                        .child(
                            div()
                                .absolute()
                                .inset_0()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap(px(10.))
                                .child(self.icon_el("music", 44., pal.text()))
                                .child(
                                    div()
                                        .max_w(px(520.))
                                        .overflow_hidden()
                                        .text_size(px(14.))
                                        .text_color(pal.text())
                                        .child(SharedString::from(title)),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(pal.muted())
                                        .child(SharedString::from(codec.clone())),
                                ),
                        )
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
                                .w(px(76.))
                                .h(px(76.))
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

        let overlay = if self.show_info {
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

        // 导出面板：右下角浮层。有任务在跑时强制显示。
        let export_panel = if self.export_panel || self.export.is_some() {
            Some(
                div()
                    .absolute()
                    .right(px(12.))
                    .bottom(px(12.))
                    .child(self.render_export_panel(cx, pal))
                    .into_any_element(),
            )
        } else {
            None
        };

        div()
            .relative()
            .flex()
            .items_center()
            .justify_center()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .bg(pal.stage())
            .child(body)
            .children(overlay)
            .children(export_panel)
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
            div()
                .id(("export-row", kind as usize))
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
                    cx.listener(move |this, _, window, cx| this.start_export(kind, window, cx)),
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

    fn render_controls(&mut self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
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
                self.icon_btn_c(
                    "mute",
                    if self.muted { "volumeX" } else { "volume" },
                    26.,
                    pal.text(),
                    self.muted,
                )
                .on_click(cx.listener(|this, _, window, cx| this.toggle_mute(window, cx))),
            )
            .child(
                div()
                    .w(px(76.))
                    .debug_selector(|| "vol-box".to_string())
                    .child(
                        Slider::new(&self.volume)
                            .horizontal()
                            .bg(pal.text())
                            .text_color(pal.text()),
                    ),
            )
            .child(
                // 单个 chip 循环 0.5 → 3.0（原 Tauri 版是下拉）；
                // 键盘 [ / ] 仍可逐级微调
                self.chip("speed", &speed_label, false).on_click(
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
                self.icon_btn_c(
                    "loop",
                    match self.loop_mode {
                        LoopMode::One => "repeat1",
                        _ => "repeat",
                    },
                    26.,
                    self.tb_color(self.loop_mode.is_on()),
                    self.loop_mode.is_on(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.loop_mode = this.loop_mode.next();
                    let msg = this.loop_mode.toast();
                    this.toast(msg, cx);
                })),
            )
            .child(
                self.icon_btn_c("info", "info", 26., self.tb_color(self.show_info), self.show_info)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_info = !this.show_info;
                        cx.notify();
                    })),
            )
            .child(
                self.icon_btn_c("shot", "camera", 26., pal.muted(), false).on_click(
                    cx.listener(|this, _, window, cx| {
                        this.start_export(export::Kind::Snapshot, window, cx)
                    },
                )),
            );

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
                    // 左右各留 34px（用户要求再各缩进 10px）
                    .px(px(34.))
                    .pt(px(6.))
                    // 无头测试靠这个选择器找进度条的坐标（非 debug 构建是空操作）
                    .debug_selector(|| "seek-row".to_string())
                    // 明确指定条色 / 滑块色，避免走组件主题那套淡灰 —— 看不清
                    .child(
                        Slider::new(&self.seek)
                            .horizontal()
                            .bg(pal.text())
                            .text_color(pal.text()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .flex_none()
                    // 间距照抄原版（14px），播放组永远整行居中
                    .gap(px(14.))
                    .px(px(24.))
                    .pt(px(4.))
                    .pb(px(10.))
                    .debug_selector(|| "ctrl-row".to_string())
                    // 左：时间。flex_1 吃掉全部余量 —— 侧栏展开时把后面两组挤到右端
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_w(px(0.))
                            .items_center()
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
                                self.icon_btn("ctl-prev", "prev", 30.)
                                    .on_click(cx.listener(|this, _, _, cx| this.step(-1, cx))),
                            )
                            .child(
                                self.icon_btn("ctl-play", if playing { "pause" } else { "play" }, 44.)
                                    .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
                            )
                            .child(
                                self.icon_btn("ctl-next", "next", 30.)
                                    .on_click(cx.listener(|this, _, _, cx| this.step(1, cx))),
                            )
                            .child(
                                self.icon_btn("ctl-stop", "stop", 30.)
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
        let stage = self.render_stage(cx);
        let sidebar = if sidebar_open {
            Some(self.render_sidebar(cx))
        } else {
            None
        };
        let titlebar = self.render_titlebar(cx);
        let controls = self.render_controls(cx);
        let focus = self.focus.clone();

        let mut root = div()
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                this.on_key(ev, window, cx)
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
    use super::load_logo;

    /// 落地页 logo 编译期嵌入，解码必须一直可用；顺便钉住尺寸与 R/B 通路。
    #[test]
    fn embedded_logo_decodes() {
        let logo = load_logo().expect("assets/logo.png 应能随二进制解码");
        let size = logo.size(0);
        assert_eq!((size.width.0, size.height.0), (200, 200));
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
            fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
