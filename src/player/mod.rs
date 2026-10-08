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
pub use video::VideoTrack;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui_kit::RenderImage;
use image::{Frame as ImgFrame, ImageBuffer, Rgba};
use smallvec::SmallVec;

use crate::media::MediaInfo;

/// 解码画面的长边上限。4K 片源会先缩到这个尺寸，省内存也省管道带宽；
/// 实测全分辨率 1080p 原始帧管道能跑到 ~570fps，远远够用。
const MAX_LONG_SIDE: u32 = 1920;

pub struct Player {
    pub info: MediaInfo,
    /// 画面的显示尺寸（已按旋转角摆正）
    pub size: (u32, u32),
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

        let size = if info.has_video {
            video::fit_size(
                info.width,
                info.height,
                info.rotate,
                MAX_LONG_SIDE,
                MAX_LONG_SIDE,
            )
        } else {
            // 纯音频没有画面可言，别报一个假的 2x2 出去
            (0, 0)
        };

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

    pub fn seek(&mut self, pos: f64) {
        self.timeline.seek(pos);
        let target = self.timeline.position();
        if let Some(v) = &mut self.video {
            if let Err(e) = v.seek(target) {
                eprintln!("[player] seek 失败: {e}");
            }
        }
        if let Some(a) = &mut self.audio {
            a.seek_to(target, self.speed);
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

    /// 推进到当前时间点应有的画面，返回它（没变化时返回上一帧）。
    pub fn frame(&mut self) -> Option<Arc<RenderImage>> {
        let pos = self.timeline.position();
        match &mut self.video {
            Some(v) => {
                // 暂停时冻结在当前画面上：不再往后挑帧，否则暂停瞬间会往前跳一帧。
                // 但如果手上根本没画面（刚 seek 完 / 刚起播就暂停），得允许挑一次，
                // 否则画面会一直空着 —— 后面几帧靠 app 侧的 warmup 重试。
                if !self.timeline.is_playing() && v.current.is_some() {
                    return v.current.clone();
                }
                v.advance(pos);
                v.current.clone()
            }
            None => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

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
}
