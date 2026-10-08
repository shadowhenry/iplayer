//! 播放内核：把一个媒体文件的画面与声音播出来。
//!
//! 结构上分三块：
//! - [`video`]：ffmpeg 解码画面 -> GPUI 纹理；
//! - [`audio`]：ffmpeg 解码声音 -> cpal 输出；
//! - [`timeline`]：唯一的权威时钟（有音频时是音频钟）。
//!
//! 对外只需要 `open / play / pause / seek / set_speed / frame` 这几个动词。

mod audio;
pub mod spectrum;
pub(crate) mod svg;
mod timeline;
mod video;

pub use timeline::Timeline;
pub use video::{Orientation, VideoTrack};

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame as ImgFrame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use crate::media::MediaInfo;

/// 解码画面的长边上限。4K 片源会先缩到这个尺寸，省内存也省管道带宽；
/// 实测全分辨率 1080p 原始帧管道能跑到 ~570fps，远远够用。
const MAX_LONG_SIDE: u32 = 1920;

/// 最终呈现尺寸：先按元数据里的 rotate 摆正并缩到上限，再叠上用户调的角度。
/// 纯音频没有画面可言，别报一个假的 2x2 出去。
fn display_size(info: &MediaInfo, orient: Orientation) -> (u32, u32) {
    if !info.has_video {
        return (0, 0);
    }
    let (w, h) = video::fit_size(
        info.width,
        info.height,
        info.rotate,
        MAX_LONG_SIDE,
        MAX_LONG_SIDE,
    );
    orient.dims(w, h)
}

/// 暂停时该不该"冻结"在现有画面上（不再往后挑帧）。
///
/// 抽出来是因为这条规则踩过两次坑：
/// - 手上还没画面（刚起播就暂停）时冻结，画面会一直空着；
/// - 刚 seek / 换过角度、正等新画面时冻结，画面会一直停在旧位置上（拖进度条的黑屏/卡帧）。
fn freeze_current(playing: bool, has_current: bool, awaiting: bool) -> bool {
    !playing && has_current && !awaiting
}

pub struct Player {
    pub info: MediaInfo,
    /// 画面的显示尺寸（已按元数据旋转角摆正，且算上了用户调的角度）
    pub size: (u32, u32),
    /// 用户手动调的画面朝向（旋转 + 翻转）
    pub orient: Orientation,
    video: Option<VideoTrack>,
    audio: Option<audio::AudioTrack>,
    timeline: Timeline,
    /// 音频起不来时的原因，信息面板会显示，播放照常进行
    pub audio_note: Option<String>,
    speed: f64,
    volume: f32,
}

impl Player {
    pub fn open(info: MediaInfo) -> Result<Self, String> {
        let path = PathBuf::from(&info.path);
        let timeline = Timeline::new(info.duration);

        let size = display_size(&info, Orientation::IDENTITY);

        let video = if info.has_video {
            Some(VideoTrack::start(&path, size.0, size.1, info.fps, 0.0)?)
        } else {
            None
        };

        let mut audio_note = None;
        let audio = if info.has_audio {
            match audio::AudioTrack::start(&path, 0.0, 1.0) {
                Ok(a) => {
                    timeline.attach_audio(a.position_source());
                    Some(a)
                }
                Err(e) => {
                    audio_note = Some(e);
                    None
                }
            }
        } else {
            None
        };

        Ok(Self {
            info,
            size,
            orient: Orientation::IDENTITY,
            video,
            audio,
            timeline,
            audio_note,
            speed: 1.0,
            volume: 1.0,
        })
    }

    pub fn duration(&self) -> f64 {
        self.info.duration
    }

    pub fn position(&self) -> f64 {
        self.timeline.position()
    }

    pub fn is_playing(&self) -> bool {
        self.timeline.is_playing()
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        if let Some(a) = &self.audio {
            a.set_volume(self.volume);
        }
    }

    pub fn play(&mut self) {
        if let Some(a) = &self.audio {
            a.set_paused(false);
        }
        self.timeline.play();
    }

    pub fn pause(&mut self) {
        self.timeline.pause();
        if let Some(a) = &self.audio {
            a.set_paused(true);
        }
    }

    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause();
        } else {
            self.play();
        }
    }

    /// 跳到 `pos`。音视频一起重开，是"落定"的那一次（松手 / 快捷键 / 循环）。
    pub fn seek(&mut self, pos: f64) {
        self.timeline.seek(pos);
        let target = self.timeline.position();
        self.seek_video(target);
        if let Some(a) = &mut self.audio {
            a.seek_to(target, self.speed);
        }
    }

    /// 拖动进度条过程中的"预览"：**只重开视频**，音频不动。
    ///
    /// 拖动时每 90ms 就会来一次 seek，若连音频解码进程一起重开，一秒里要起停十几个
    /// 进程 —— 声音会碎成一片，进程调度也会把画面拖垮。松手时才走完整的 [`seek`]。
    /// 时钟不用另行处理：`timeline.seek` 已经把音频钟重新落在 `pos` 上。
    pub fn scrub(&mut self, pos: f64) {
        self.timeline.seek(pos);
        let target = self.timeline.position();
        self.seek_video(target);
    }

    fn seek_video(&mut self, target: f64) {
        if let Some(v) = &mut self.video {
            if let Err(e) = v.seek(target) {
                eprintln!("[player] seek 失败: {e}");
            }
        }
    }

    pub fn set_speed(&mut self, speed: f64) {
        let pos = self.position();
        self.speed = speed.clamp(0.25, 4.0);
        self.timeline.set_rate(self.speed, pos);
        if let Some(a) = &mut self.audio {
            a.seek_to(pos, self.speed);
        }
    }

    /// 换一个画面朝向（旋转 / 翻转）。
    ///
    /// 视频是把新角度塞进 ffmpeg 滤镜链、从当前位置重开一次解码（等价于一次 seek）；
    /// 音频完全不受影响，播放位置也不会断。
    pub fn set_orientation(&mut self, orient: Orientation) {
        if self.orient.same_effect(orient) {
            self.orient = orient;
            return;
        }
        self.orient = orient;
        if self.info.has_video {
            self.size = display_size(&self.info, orient);
        }
        let pos = self.timeline.position();
        if let Some(v) = &mut self.video {
            if let Err(e) = v.set_orientation(orient, pos) {
                eprintln!("[player] 换画面角度失败: {e}");
            }
        }
    }

    /// 推进到当前时间点应有的画面，返回它（没变化时返回上一帧）。
    pub fn frame(&mut self) -> Option<Arc<RenderImage>> {
        let pos = self.timeline.position();
        match &mut self.video {
            Some(v) => {
                // 暂停时冻结在当前画面上：不再往后挑帧，否则暂停瞬间会往前跳一帧。
                // 但如果手上根本没画面（刚起播就暂停），或者刚 seek / 换过角度、
                // 正等着新画面，就得允许挑一次 —— 否则画面会一直空着 / 一直停在旧位置上。
                if freeze_current(self.timeline.is_playing(), v.current.is_some(), v.awaiting()) {
                    return v.current.clone();
                }
                v.advance(pos);
                v.current.clone()
            }
            None => None,
        }
    }

    /// 是否正在等 seek / 换角度之后的第一帧（此时画面还是旧的，主循环要继续要帧）。
    pub fn awaiting(&self) -> bool {
        self.video.as_ref().is_some_and(|v| v.awaiting())
    }

    /// 播放是否已经走完（画面解码结束且末帧已呈现）。
    pub fn ended(&self) -> bool {
        let pos = self.timeline.position();
        match &self.video {
            Some(v) => v.ended(pos),
            None => self.info.duration > 0.0 && pos >= self.info.duration - 0.08,
        }
    }

    /// 可视化数据源：音频采样窗 + 设备采样率。
    ///
    /// 没有音轨（或音频设备起不来）时给 `None`，调用方退化成合成谱。
    pub fn spectrum(&self) -> Option<(Arc<spectrum::Tap>, f32)> {
        self.audio.as_ref().map(|a| (a.tap(), a.rate as f32))
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // 显式停掉，确保子进程与音频线程都收干净
        if let Some(v) = &mut self.video {
            v.kill();
        }
        if let Some(a) = &mut self.audio {
            a.stop();
        }
    }
}

/// 静态图片解码成 GPUI 纹理。长边超过 [`IMAGE_MAX_SIDE`] 先缩小，省显存。
///
/// 三条路：
/// - SVG 是矢量图，交给 [`svg`]；
/// - 常规位图交给 `image` crate（快，不用起进程）；
/// - `image` 认不出来的（AVIF、EXR、PSD…）退回 ffmpeg 解一帧。
pub fn load_image(path: &Path) -> Result<Arc<RenderImage>, String> {
    const IMAGE_MAX_SIDE: u32 = 3840;

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "svg" || ext == "svgz" {
        return svg::load(path);
    }

    match image::open(path) {
        Ok(img) => {
            let img = if img.width() > IMAGE_MAX_SIDE || img.height() > IMAGE_MAX_SIDE {
                img.thumbnail(IMAGE_MAX_SIDE, IMAGE_MAX_SIDE)
            } else {
                img
            };
            Ok(image_from_rgba(img.to_rgba8()))
        }
        Err(e) if ext == "avif" || ext == "exr" || ext == "psd" => {
            load_still_via_ffmpeg(path, IMAGE_MAX_SIDE).map_err(|f| format!("{e}；ffmpeg 也不行: {f}"))
        }
        Err(e) => Err(format!("读取图片失败: {e}")),
    }
}

/// 让 ffmpeg 解出第一帧，直接作为画面。走 `rawvideo`，出来就是 BGRA，零转换。
fn load_still_via_ffmpeg(path: &Path, max_side: u32) -> Result<Arc<RenderImage>, String> {
    use std::process::Stdio;

    let info = crate::media::probe(path)?;
    if !info.has_video {
        return Err("ffmpeg 没在这份文件里找到画面".to_string());
    }
    let (w, h) = video::fit_size(info.width, info.height, info.rotate, max_side, max_side);

    let mut cmd = crate::ffmpeg::base_command()?;
    cmd.arg("-i")
        .arg(path)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(format!("scale={w}:{h}:flags=lanczos"))
        .arg("-pix_fmt")
        .arg("bgra")
        .arg("-f")
        .arg("rawvideo")
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let out = cmd.output().map_err(|e| format!("无法运行 ffmpeg: {e}"))?;
    let need = (w as usize) * (h as usize) * 4;
    if out.stdout.len() < need {
        return Err("ffmpeg 没有吐出完整的画面".to_string());
    }
    let mut buf = out.stdout;
    buf.truncate(need);
    Ok(image_from_bgra(buf, w, h))
}

/// BGRA 字节 -> 纹理。ffmpeg 直接给这个顺序，GPUI 也按这个解释，中间不用动。
fn image_from_bgra(buf: Vec<u8>, w: u32, h: u32) -> Arc<RenderImage> {
    let im = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, buf)
        .expect("BGRA 缓冲长度与尺寸一致");
    Arc::new(RenderImage::new(SmallVec::from_vec(vec![ImgFrame::new(im)])))
}

/// RGBA -> GPUI 纹理。macOS 上 GPUI 按 **BGRA** 解释缓冲里的字节，
/// 而 video 轨喂进来的正是 ffmpeg 的 `bgra`，所以这里把 R/B 对调保持一致。
fn image_from_rgba(rgba: image::RgbaImage) -> Arc<RenderImage> {
    let (w, h) = rgba.dimensions();
    image_from_straight_rgba(rgba.into_raw(), w, h)
}

/// 非预乘 RGBA 字节 -> 纹理（内部做 R/B 对调）。SVG 那条路也用这个。
pub(crate) fn image_from_straight_rgba(mut buf: Vec<u8>, w: u32, h: u32) -> Arc<RenderImage> {
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let im = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, buf)
        .expect("RGBA 缓冲长度与尺寸一致");
    Arc::new(RenderImage::new(SmallVec::from_vec(vec![ImgFrame::new(im)])))
}

/// 给一张**已经解好的**静态图片套上朝向（旋转 / 翻转）。
///
/// 视频那条路是 ffmpeg 滤镜直接转，静态图片没有解码进程可挂，就在 CPU 上倒一遍：
/// 输出第 (ox, oy) 个像素取自 [`Orientation::map_back`] 指到的输入像素。
/// 映射与 ffmpeg 的 `transpose/hflip/vflip` 完全同义 —— `--selftest` 会真跑一遍
/// ffmpeg 来钉死这件事。
pub fn orient_image(base: &RenderImage, orient: Orientation) -> Option<Arc<RenderImage>> {
    // `RenderImage::size` 给的是 i32，纹理尺寸恒为正，这里直接当 u32 用
    let (w, h) = (base.size(0).width.0 as u32, base.size(0).height.0 as u32);
    let src = base.as_bytes(0)?;
    let (ow, oh) = orient.dims(w, h);
    let mut dst = vec![0u8; (ow as usize) * (oh as usize) * 4];
    for oy in 0..oh {
        for ox in 0..ow {
            let (x, y) = orient.map_back(ox, oy, w, h);
            let si = ((y as usize) * (w as usize) + (x as usize)) * 4;
            let di = ((oy as usize) * (ow as usize) + (ox as usize)) * 4;
            dst[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    Some(image_from_bgra(dst, ow, oh))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    /// 拖进度条**不能黑屏**：seek 之后新画面还没解出来之前，旧画面必须还在屏上；
    /// 而随后又必须换成新画面（不然就是卡死在旧帧上）。
    ///
    /// 这条是用户报的"拖动播放条时黑屏、屏闪"的回归钉子。要真解码才能验，
    /// 所以用内置 ffmpeg 现造一段无声短片；机器上找不到 ffmpeg 就跳过。
    #[test]
    fn scrub_keeps_the_old_frame_until_the_new_one_arrives() {
        use std::time::{Duration, Instant};

        let dir = std::env::temp_dir().join("iplayer-scrub-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("clip.mp4");
        if !path.exists() {
            let Ok(mut cmd) = crate::ffmpeg::base_command() else {
                return; // 没有 ffmpeg：跳过，不把环境问题算成回归
            };
            let ok = cmd
                .args(["-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc=size=160x120:rate=25:duration=4")
                .args(["-pix_fmt", "yuv420p", "-an", "-y"])
                .arg(&path)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !ok || !path.exists() {
                return;
            }
        }

        let Ok(info) = crate::media::probe(&path) else {
            return;
        };
        let mut p = Player::open(info).expect("打开自造片源");

        // 等第一帧出来
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) && p.frame().is_none() {
            std::thread::sleep(Duration::from_millis(4));
        }
        let before = p.frame().map(|f| f.id.0).expect("起播后应当有画面");

        p.pause();
        assert_eq!(
            p.frame().map(|f| f.id.0),
            Some(before),
            "暂停之后画面必须冻结"
        );

        // 拖进度条（预览 seek）：这一瞬间绝不能变成"没有画面"
        p.scrub(3.0);
        assert_eq!(
            p.frame().map(|f| f.id.0),
            Some(before),
            "seek 瞬间旧画面必须留在屏上 —— 变 None 就是用户看到的黑屏闪"
        );

        // 新画面必须到，否则就是卡在旧帧上再也不动
        let t0 = Instant::now();
        let mut updated = false;
        while t0.elapsed() < Duration::from_secs(3) {
            if p.frame().map(|f| f.id.0) != Some(before) {
                updated = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(updated, "seek 之后新画面应当在几秒内到达");

        // 往回拖（pts 变小）也得能出画：`current_pts` 不归零就会被单调性挡死
        let mid = p.frame().map(|f| f.id.0);
        p.scrub(0.5);
        let t0 = Instant::now();
        let mut back_ok = false;
        while t0.elapsed() < Duration::from_secs(3) {
            if p.frame().map(|f| f.id.0) != mid {
                back_ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(back_ok, "往回拖之后画面必须跟着更新");
    }

    /// 冻结规则的真值表：既要"暂停时画面一动不动"，又不能把刚 seek 完的画面锁死。
    #[test]
    fn freeze_only_when_paused_with_a_ready_frame() {
        assert!(!freeze_current(true, true, false), "播放中不能冻结");
        assert!(!freeze_current(true, false, true), "播放中不能冻结");
        assert!(freeze_current(false, true, false), "暂停且画面就绪 -> 冻结");
        assert!(!freeze_current(false, false, false), "暂停但没画面 -> 得挑一帧");
        assert!(
            !freeze_current(false, true, true),
            "暂停 + 刚 seek 等新画面 -> 也要挑帧，否则停在旧画面上"
        );
    }

    /// GPUI 的 `RenderImage` 是 **BGRA**（见 gpui `assets.rs` 的文档注释），
    /// 而 `image` crate 给的是 RGBA —— 这段就是钉死那个 R/B 对调的动作。
    #[test]
    fn image_bytes_are_bgra_and_opaque() {
        let dir = std::env::temp_dir().join("iplayer-unit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("red.png");

        let mut src = RgbaImage::new(2, 2);
        for px in src.pixels_mut() {
            *px = Rgba([255, 0, 0, 255]);
        }
        src.save(&path).unwrap();

        let rendered = load_image(&path).unwrap();
        let bytes = rendered.as_bytes(0).expect("第一帧存在");
        assert_eq!(bytes.len(), 2 * 2 * 4);
        // 纯红在 BGRA 里是 [0, 0, 255, 255]
        assert_eq!(&bytes[0..4], &[0, 0, 255, 255]);

        std::fs::remove_file(&path).ok();
    }

    /// 超大图会被缩到长边 3840 以内，避免一次吃掉几百 MB 显存。
    #[test]
    fn huge_image_is_thumbnailed() {
        let dir = std::env::temp_dir().join("iplayer-unit");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.png");

        let mut src = RgbaImage::new(6000, 100);
        for px in src.pixels_mut() {
            *px = Rgba([10, 20, 30, 255]);
        }
        src.save(&path).unwrap();

        let rendered = load_image(&path).unwrap();
        let bytes = rendered.as_bytes(0).unwrap();
        assert!(bytes.len() <= 3840 * 100 * 4, "长边应被压到 3840 以内");

        std::fs::remove_file(&path).ok();
    }

    /// 造一张 2x1 的图（左红右蓝），逐个朝向核对像素落到哪儿。
    /// 注意 GPUI 的缓冲是 BGRA：纯红 = [0,0,255,255]，纯蓝 = [255,0,0,255]。
    fn two_by_one() -> Arc<RenderImage> {
        let mut buf = vec![0u8; 2 * 4];
        buf[0..4].copy_from_slice(&[0, 0, 255, 255]); // 左：红
        buf[4..8].copy_from_slice(&[255, 0, 0, 255]); // 右：蓝
        image_from_bgra(buf, 2, 1)
    }

    const RED: [u8; 4] = [0, 0, 255, 255];
    const BLUE: [u8; 4] = [255, 0, 0, 255];

    fn px_at(img: &RenderImage, x: u32, y: u32) -> [u8; 4] {
        let w = img.size(0).width.0 as u32;
        let b = img.as_bytes(0).unwrap();
        let i = ((y * w + x) as usize) * 4;
        [b[i], b[i + 1], b[i + 2], b[i + 3]]
    }

    #[test]
    fn orient_image_rotates_and_flips() {
        let base = two_by_one();

        let flipped = orient_image(&base, Orientation { flip_h: true, ..Default::default() }).unwrap();
        assert_eq!(flipped.size(0).width.0, 2);
        assert_eq!(flipped.size(0).height.0, 1);
        assert_eq!(px_at(&flipped, 0, 0), BLUE, "左右翻转后左边应当是原右边");
        assert_eq!(px_at(&flipped, 1, 0), RED);

        // 顺时针 90°：宽高互换，原左边的红点跑到**上面**
        let cw = orient_image(&base, Orientation { rot: 90, ..Default::default() }).unwrap();
        assert_eq!(cw.size(0).width.0, 1);
        assert_eq!(cw.size(0).height.0, 2);
        assert_eq!(px_at(&cw, 0, 0), RED);
        assert_eq!(px_at(&cw, 0, 1), BLUE);

        // 逆时针 90°：红点跑到**下面**
        let ccw = orient_image(&base, Orientation { rot: 270, ..Default::default() }).unwrap();
        assert_eq!(ccw.size(0).width.0, 1);
        assert_eq!(ccw.size(0).height.0, 2);
        assert_eq!(px_at(&ccw, 0, 0), BLUE);
        assert_eq!(px_at(&ccw, 0, 1), RED);

        // 180°：左右对调，尺寸不变
        let half = orient_image(&base, Orientation { rot: 180, ..Default::default() }).unwrap();
        assert_eq!(px_at(&half, 0, 0), BLUE);
        assert_eq!(px_at(&half, 1, 0), RED);
    }
}
