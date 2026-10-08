//! Media discovery and probing.
//!
//! Playback is fully native (ffmpeg decodes straight to frames), so there is no
//! "can the webview take this?" planning step any more — every format ffmpeg
//! can open is playable as-is. What is left here is scanning folders and asking
//! ffprobe what a file actually contains.

use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde::Serialize;

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "avi", "flv", "wmv", "webm", "mpg", "mpeg", "m2v", "ts", "m2ts",
    "mts", "rmvb", "rm", "3gp", "3g2", "ogv", "vob", "asf", "divx", "f4v", "mxf", "dv", "amv",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "flac", "wav", "wave", "ogg", "oga", "opus", "wma", "ape", "alac", "aif",
    "aiff", "mka", "ac3", "dts", "amr",
];
pub const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "svgz", "ico", "tif", "tiff", "avif", "exr",
    "psd",
];

#[derive(Serialize, Clone)]
pub struct MediaFile {
    pub name: String,
    pub path: String,
    pub ext: String,
    pub size: u64,
    pub modified: u64,
    /// one of: `video` | `audio` | `image`
    pub kind: String,
}

pub fn kind_of(ext: &str) -> Option<&'static str> {
    if VIDEO_EXTS.contains(&ext) {
        Some("video")
    } else if AUDIO_EXTS.contains(&ext) {
        Some("audio")
    } else if IMAGE_EXTS.contains(&ext) {
        Some("image")
    } else {
        None
    }
}

fn modified_secs(meta: &fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn scan_dir(dir: &Path, recursive: bool) -> Result<Vec<MediaFile>, String> {
    if !dir.is_dir() {
        return Err(format!("不是一个文件夹: {}", dir.display()));
    }
    let mut out = Vec::new();
    collect(dir, recursive, &mut out, 0)?;
    out.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    Ok(out)
}

fn collect(dir: &Path, recursive: bool, out: &mut Vec<MediaFile>, depth: u32) -> Result<(), String> {
    if depth > 6 {
        return Ok(());
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            if recursive {
                collect(&path, true, out, depth + 1)?;
            }
            continue;
        }
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if let Some(kind) = kind_of(&ext) {
            out.push(MediaFile {
                name,
                path: path.to_string_lossy().to_string(),
                ext,
                size: meta.len(),
                modified: modified_secs(&meta),
                kind: kind.to_string(),
            });
        }
    }
    Ok(())
}

/// "ep2" sorts before "ep10".
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.char_indices().peekable();
    let mut bi = b.char_indices().peekable();
    loop {
        match (ai.peek(), bi.peek()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some((_, ac)), Some((_, bc))) => {
                let (ac, bc) = (*ac, *bc);
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let mut an = String::new();
                    while let Some((_, c)) = ai.peek() {
                        if c.is_ascii_digit() {
                            an.push(*c);
                            ai.next();
                        } else {
                            break;
                        }
                    }
                    let mut bn = String::new();
                    while let Some((_, c)) = bi.peek() {
                        if c.is_ascii_digit() {
                            bn.push(*c);
                            bi.next();
                        } else {
                            break;
                        }
                    }
                    let a_num: u128 = an.parse().unwrap_or(0);
                    let b_num: u128 = bn.parse().unwrap_or(0);
                    match a_num.cmp(&b_num) {
                        std::cmp::Ordering::Equal => continue,
                        other => return other,
                    }
                } else {
                    let al = ac.to_lowercase().next().unwrap_or(ac);
                    let bl = bc.to_lowercase().next().unwrap_or(bc);
                    match al.cmp(&bl) {
                        std::cmp::Ordering::Equal => {
                            ai.next();
                            bi.next();
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// GIF 结构解析
// ---------------------------------------------------------------------------

/// GIF 的几个关键指标。只走块头、不碰 LZW 数据，所以再大的图也是微秒级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GifInfo {
    /// 逻辑屏幕尺寸
    pub width: u32,
    pub height: u32,
    /// 图像描述符个数 = 帧数。1 表示静态图。
    pub frames: usize,
}

impl GifInfo {
    pub fn is_animated(&self) -> bool {
        self.frames > 1
    }
}

/// 解析 GIF 头与所有块的长度字段，数出一共几帧。
///
/// GIF 的布局是：6 字节签名 + 7 字节逻辑屏幕描述符 + 可选全局调色板，
/// 然后是一串块 —— `0x2C` 图像描述符、`0x21` 扩展、`0x3B` 结束。
/// 每个块的长度都在块头里写着，所以顺序跳过就行。
pub fn gif_info(path: &Path) -> Option<GifInfo> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let file = fs::File::open(path).ok()?;
    let mut r = std::io::BufReader::new(file);
    let mut head = [0u8; 13];
    r.read_exact(&mut head).ok()?;
    if &head[0..3] != b"GIF" {
        return None;
    }
    let width = u16::from_le_bytes([head[6], head[7]]) as u32;
    let height = u16::from_le_bytes([head[8], head[9]]) as u32;
    let packed = head[10];

    // 全局调色板：3 字节一项，2^(N+1) 项
    if packed & 0x80 != 0 {
        let entries = 1u64 << ((packed & 0x07) + 1);
        r.seek(SeekFrom::Current((3 * entries) as i64)).ok()?;
    }

    let mut frames = 0usize;
    let mut one = [0u8; 1];
    loop {
        if r.read_exact(&mut one).is_err() {
            break;
        }
        match one[0] {
            // 图像描述符
            0x2C => {
                let mut desc = [0u8; 9];
                if r.read_exact(&mut desc).is_err() {
                    break;
                }
                frames += 1;
                let local = desc[8];
                if local & 0x80 != 0 {
                    let entries = 1u64 << ((local & 0x07) + 1);
                    if r.seek(SeekFrom::Current((3 * entries) as i64)).is_err() {
                        break;
                    }
                }
                // LZW 最小码长 1 字节 + 数据子块
                if r.read_exact(&mut one).is_err() || skip_sub_blocks(&mut r).is_err() {
                    break;
                }
            }
            // 扩展块：先吃掉标签字节，再按子块跳过
            0x21 => {
                if r.read_exact(&mut one).is_err() || skip_sub_blocks(&mut r).is_err() {
                    break;
                }
            }
            // 结束符
            0x3B => break,
            // 认不出来的字节：放弃解析，当作静态图
            _ => break,
        }
        // 帧数够多就够判断"是不是动画"了，不必读完
        if frames > 64 {
            break;
        }
    }

    if frames == 0 {
        return None;
    }
    Some(GifInfo {
        width,
        height,
        frames,
    })
}

/// 子块序列：`长度 + 数据` 重复，直到长度为 0。
fn skip_sub_blocks<R: std::io::Read + std::io::Seek>(r: &mut R) -> std::io::Result<()> {
    // 方法由 trait bound 提供，不用再 use 一次
    use std::io::SeekFrom;
    let mut len = [0u8; 1];
    loop {
        r.read_exact(&mut len)?;
        if len[0] == 0 {
            return Ok(());
        }
        r.seek(SeekFrom::Current(len[0] as i64))?;
    }
}

// ---------------------------------------------------------------------------
// probing
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    codec_long_name: String,
    #[serde(default)]
    pix_fmt: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    avg_frame_rate: String,
    #[serde(default)]
    r_frame_rate: String,
    #[serde(default)]
    sample_aspect_ratio: String,
    #[serde(default)]
    channels: u32,
    #[serde(default)]
    channel_layout: String,
    #[serde(default)]
    sample_rate: String,
    #[serde(default)]
    bit_rate: String,
    #[serde(default)]
    has_b_frames: u32,
    /// 视频旋转角度（手机竖拍常见 90/270），来自 side data 的 displaymatrix。
    #[serde(default)]
    tags: StreamTags,
}

#[derive(serde::Deserialize, Default)]
struct StreamTags {
    #[serde(default)]
    rotate: String,
}

#[derive(serde::Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    format_long_name: String,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    bit_rate: String,
    #[serde(default)]
    size: String,
}

#[derive(serde::Deserialize)]
struct ProbeRoot {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

/// Everything the info panel and the playback pipeline need to know.
#[derive(Serialize, Clone, Default)]
pub struct MediaInfo {
    pub path: String,
    pub ext: String,
    /// one of: `video` | `audio`
    pub kind: String,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: String,
    pub vcodec_long: String,
    pub acodec: String,
    pub acodec_long: String,
    /// e.g. `yuv420p`, `yuv420p10le`
    pub pix_fmt: String,
    /// e.g. `High`, `High 10`, `High 4:4:4 Predictive`
    pub profile: String,
    pub format_name: String,
    pub bitrate: u64,
    pub size: u64,
    pub channels: u32,
    pub channel_layout: String,
    pub sample_rate: u32,
    pub audio_bitrate: u64,
    pub has_video: bool,
    pub has_audio: bool,
    /// >0 when the video stream reorders frames (B-frames).
    pub has_b_frames: u32,
    /// 旋转角度（0/90/180/270）。渲染时需要交换宽高。
    pub rotate: u32,
    /// 像素宽高比，`1:1` 表示方形像素。
    pub sar: String,
    /// 帧数。只有 GIF 会填（自己数的），其它一律 0 = 未知。
    pub frames: u32,
}

fn parse_fps(s: &str) -> f64 {
    let mut parts = s.split('/');
    let num = parts.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let den = parts.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
    if den > 0.0 && num > 0.0 { num / den } else { 0.0 }
}

pub fn probe(path: &Path) -> Result<MediaInfo, String> {
    let mut cmd = super::ffmpeg::base_ffprobe_command()?;
    cmd.arg("-v")
        .arg("error")
        .arg("-print_format")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(path);

    let out = cmd.output().map_err(|e| format!("无法运行 ffprobe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("无法解析媒体信息: {}", err.trim()));
    }

    let root: ProbeRoot =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("解析媒体信息失败: {e}"))?;

    let video = root.streams.iter().find(|s| s.codec_type == "video");
    let audio = root.streams.iter().find(|s| s.codec_type == "audio");
    let fmt = root.format.as_ref();

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let has_video = video.is_some();
    let has_audio = audio.is_some();

    // GIF：ffprobe 报的帧率对动图不太可靠，自己数一遍帧数最踏实。
    let gif = (ext == "gif").then(|| gif_info(path)).flatten();
    let frames = gif.map(|g| g.frames as u32).unwrap_or(0);
    let duration = fmt
        .map(|f| f.duration.parse::<f64>().unwrap_or(0.0))
        .unwrap_or(0.0);
    let mut fps = video
        .map(|v| {
            let a = parse_fps(&v.avg_frame_rate);
            if a > 0.0 { a } else { parse_fps(&v.r_frame_rate) }
        })
        .unwrap_or(0.0);
    if fps <= 0.0 && duration > 0.0 && frames > 1 {
        // 兜底：动画 GIF 的平均帧率 = 帧数 / 总时长
        fps = frames as f64 / duration;
    }

    Ok(MediaInfo {
        path: path.to_string_lossy().to_string(),
        ext,
        kind: if has_video { "video" } else { "audio" }.to_string(),
        duration,
        width: video.map(|v| v.width).unwrap_or(0),
        height: video.map(|v| v.height).unwrap_or(0),
        fps,
        vcodec: video.map(|v| v.codec_name.clone()).unwrap_or_default(),
        vcodec_long: video.map(|v| v.codec_long_name.clone()).unwrap_or_default(),
        acodec: audio.map(|a| a.codec_name.clone()).unwrap_or_default(),
        acodec_long: audio.map(|a| a.codec_long_name.clone()).unwrap_or_default(),
        pix_fmt: video.map(|v| v.pix_fmt.clone()).unwrap_or_default(),
        profile: video.map(|v| v.profile.clone()).unwrap_or_default(),
        format_name: fmt.map(|f| f.format_long_name.clone()).unwrap_or_default(),
        bitrate: fmt
            .map(|f| f.bit_rate.parse::<u64>().unwrap_or(0))
            .unwrap_or(0),
        size: fmt
            .map(|f| f.size.parse::<u64>().unwrap_or(0))
            .unwrap_or(0),
        channels: audio.map(|a| a.channels).unwrap_or(0),
        channel_layout: audio
            .map(|a| a.channel_layout.clone())
            .unwrap_or_default(),
        sample_rate: audio
            .and_then(|a| a.sample_rate.parse::<u32>().ok())
            .unwrap_or(0),
        audio_bitrate: audio
            .and_then(|a| a.bit_rate.parse::<u64>().ok())
            .unwrap_or(0),
        has_video,
        has_audio,
        has_b_frames: video.map(|v| v.has_b_frames).unwrap_or(0),
        rotate: video
            .and_then(|v| v.tags.rotate.parse::<u32>().ok())
            .filter(|r| matches!(r, 90 | 180 | 270))
            .unwrap_or(0),
        sar: video
            .map(|v| v.sample_aspect_ratio.clone())
            .unwrap_or_default(),
        frames,
    })
}


/// `3725.4` -> `01:02:05`
pub fn format_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "00:00".to_string();
    }
    let total = secs.floor() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `"1.5 GB"` — for the info panel.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.2} GB", b / (KB * KB * KB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["ep10".to_string(), "ep2".to_string(), "ep1".to_string()];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["ep1", "ep2", "ep10"]);
    }

    #[test]
    fn fps_parse() {
        assert_eq!(parse_fps("30000/1001"), 30000.0 / 1001.0);
        assert_eq!(parse_fps("0/0"), 0.0);
        // 只有分子时按分母 1 处理
        assert_eq!(parse_fps("25"), 25.0);
    }

    #[test]
    fn time_and_size() {
        assert_eq!(format_time(65.9), "01:05");
        assert_eq!(format_time(3725.0), "01:02:05");
        assert_eq!(format_time(-1.0), "00:00");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_of("mkv"), Some("video"));
        assert_eq!(kind_of("flac"), Some("audio"));
        assert_eq!(kind_of("webp"), Some("image"));
        assert_eq!(kind_of("svg"), Some("image"));
        assert_eq!(kind_of("txt"), None);
    }

    /// 写一个真 · 两帧 GIF 出来，确认我们自己数的帧数和尺寸都对。
    #[test]
    fn gif_frames_are_counted() {
        use image::codecs::gif::GifEncoder;
        use image::{Frame as ImgFrame, Rgba, RgbaImage};

        let dir = std::env::temp_dir().join("iplayer-media-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("anim.gif");

        let mut a = RgbaImage::new(4, 3);
        for px in a.pixels_mut() {
            *px = Rgba([255, 0, 0, 255]);
        }
        let mut b = RgbaImage::new(4, 3);
        for px in b.pixels_mut() {
            *px = Rgba([0, 0, 255, 255]);
        }

        {
            let file = fs::File::create(&path).unwrap();
            let mut enc = GifEncoder::new(file);
            enc.encode_frame(ImgFrame::new(a)).unwrap();
            enc.encode_frame(ImgFrame::new(b)).unwrap();
        }

        let info = gif_info(&path).expect("应当解析成功");
        assert_eq!(info.frames, 2);
        assert_eq!((info.width, info.height), (4, 3));
        assert!(info.is_animated());

        // 单帧 GIF 不算动画
        let still_path = dir.join("still.gif");
        {
            let file = fs::File::create(&still_path).unwrap();
            let mut enc = GifEncoder::new(file);
            enc.encode_frame(ImgFrame::new(RgbaImage::new(4, 3))).unwrap();
        }
        let still = gif_info(&still_path).expect("应当解析成功");
        assert_eq!(still.frames, 1);
        assert!(!still.is_animated());

        // 非 GIF 直接放弃
        assert!(gif_info(Path::new("/definitely/not/here.png")).is_none());

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&still_path).ok();
    }
}
