//! Media discovery, probing and "can the webview play this?" planning.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

pub const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "avi", "flv", "wmv", "webm", "mpg", "mpeg", "m2v", "ts", "m2ts",
    "mts", "rmvb", "rm", "3gp", "3g2", "ogv", "vob", "asf", "divx", "f4v", "mxf", "dv", "amv",
];
pub const AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "flac", "wav", "wave", "ogg", "oga", "opus", "wma", "ape", "alac", "aif",
    "aiff", "mka", "ac3", "dts", "amr",
];
/// Image formats the webview can render natively in `<img>`.
pub const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "ico", "avif"];

/// Containers a WebKit/Chromium webview can demux on its own. The whole
/// ISO-BMFF family counts: the webview dispatches on the codec inside, not on
/// the brand in the file header.
const NATIVE_CONTAINERS: &[&str] = &[
    "mp4", "m4v", "m4a", "mov", "3gp", "3g2", "f4v", "webm", "mp3", "aac", "wav", "wave", "flac",
    "ogg", "oga", "opus",
];
/// Video codecs the webview can decode.
const NATIVE_VCODECS: &[&str] = &["h264", "avc1", "hevc", "h265", "vp8", "vp9", "av1"];
/// Audio codecs the webview can decode at all.
const NATIVE_ACODECS: &[&str] = &[
    "aac", "mp3", "opus", "vorbis", "flac", "alac", "pcm_s16le", "pcm_s24le", "pcm_s16be",
    "pcm_f32le", "pcm_u8",
];
/// Audio codecs that stay decodable when *copied* into an MP4/MOV container.
const MP4_ACODECS: &[&str] = &["aac", "mp3", "alac"];
/// Audio codecs that stay decodable when *copied* into a WebM container.
const WEBM_ACODECS: &[&str] = &["opus", "vorbis"];

/// Can the webview demux this video codec out of this container?
/// WebKit ships VP8/VP9/AV1 only in WebM and H.264/HEVC only in MP4/MOV.
fn container_fits_video(container: &str, vcodec: &str) -> bool {
    match container {
        "mp4" | "m4v" | "mov" | "3gp" | "3g2" | "f4v" => !matches!(vcodec, "vp8" | "vp9"),
        "webm" => !matches!(vcodec, "h264" | "avc1" | "hevc" | "h265"),
        _ => true,
    }
}

/// Containers that only store one timestamp per sample, i.e. no presentation
/// timestamp at all. ffmpeg therefore *guesses* PTS from DTS when reading them,
/// and the guess is wrong for any stream that reorders frames: copying a
/// B-frame H.264 out of an AVI yields frames in decode order (visible judder),
/// no matter which muxer flags are passed.
fn container_lacks_pts(ext: &str) -> bool {
    matches!(ext, "avi" | "divx")
}

/// Everything the planner needs to know about a probed file.
#[derive(Clone, Copy)]
pub struct StreamTraits<'a> {
    pub ext: &'a str,
    pub vcodec: &'a str,
    pub acodec: &'a str,
    pub pix_fmt: &'a str,
    pub vtag: &'a str,
    pub has_video: bool,
    pub has_audio: bool,
    /// Non-zero when the video stream uses B-frames.
    pub has_b_frames: u32,
}

/// Can the webview decode this video *stream* at all, regardless of container?
///
/// A matching codec *name* is not enough: WebKit only ships 8-bit 4:2:0 H.264,
/// so "H.264 Hi10P" (10-bit, ubiquitous in anime) and 4:4:4/4:2:2 material fail
/// even though ffprobe still reports `codec_name=h264`.
pub fn video_stream_ok(vcodec: &str, pix_fmt: &str) -> bool {
    if !NATIVE_VCODECS.contains(&vcodec) {
        return false;
    }
    // No webview decodes 4:2:2 or 4:4:4 chroma.
    if pix_fmt.contains("444") || pix_fmt.contains("422") {
        return false;
    }
    match vcodec {
        "h264" | "avc1" => matches!(pix_fmt, "" | "yuv420p" | "yuvj420p" | "nv12"),
        _ => true,
    }
}

/// Can the webview play this video straight out of `container`, no rewrapping?
fn video_playable(t: &StreamTraits) -> bool {
    if !video_stream_ok(t.vcodec, t.pix_fmt) || !container_fits_video(t.ext, t.vcodec) {
        return false;
    }
    // WebKit only accepts `hvc1`-tagged HEVC; `hev1` is rejected, but a remux can
    // fix the tag without touching the picture.
    if matches!(t.vcodec, "hevc" | "h265") {
        return t.vtag.is_empty() || t.vtag == "hvc1";
    }
    true
}

/// Must the picture be re-encoded? Either the bitstream itself is undecodable,
/// or its container can't express the frame order (see `container_lacks_pts`).
pub fn needs_reencode_video(t: &StreamTraits) -> bool {
    if !t.has_video {
        return false;
    }
    !video_stream_ok(t.vcodec, t.pix_fmt) || (container_lacks_pts(t.ext) && t.has_b_frames > 0)
}

/// Can this audio stream survive a `-c copy` into `container` and still play?
pub fn audio_fits(container: &str, acodec: &str) -> bool {
    match container {
        "webm" => WEBM_ACODECS.contains(&acodec),
        // QuickTime/MOV carries linear PCM natively, and so does Safari.
        "mp4" | "m4v" | "mov" | "m4a" | "3gp" | "3g2" | "f4v" => {
            MP4_ACODECS.contains(&acodec) || acodec.starts_with("pcm_")
        }
        "ogg" | "oga" => matches!(acodec, "opus" | "vorbis" | "flac"),
        "mp3" => acodec == "mp3",
        "aac" => acodec == "aac",
        "flac" => acodec == "flac",
        "opus" => acodec == "opus",
        "wav" | "wave" => acodec.starts_with("pcm_"),
        other => NATIVE_CONTAINERS.contains(&other) && NATIVE_ACODECS.contains(&acodec),
    }
}

#[derive(Serialize, Clone)]
pub struct MediaFile {
    pub name: String,
    pub path: String,
    pub ext: String,
    pub size: u64,
    pub modified: u64,
    pub kind: String,
}

fn kind_of(ext: &str) -> Option<&'static str> {
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
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
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
// Probing
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: String,
    #[serde(default)]
    codec_name: String,
    #[serde(default)]
    codec_tag_string: String,
    #[serde(default)]
    profile: String,
    #[serde(default)]
    pix_fmt: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
    #[serde(default)]
    avg_frame_rate: String,
    #[serde(default)]
    r_frame_rate: String,
    #[serde(default)]
    channels: u32,
    #[serde(default)]
    sample_rate: String,
    #[serde(default)]
    has_b_frames: u32,
}

#[derive(Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    format_name: String,
    #[serde(default)]
    duration: String,
    #[serde(default)]
    bit_rate: String,
    #[serde(default)]
    size: String,
}

#[derive(Deserialize)]
struct ProbeRoot {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Serialize, Clone)]
pub struct MediaInfo {
    pub path: String,
    pub ext: String,
    pub kind: String,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub vcodec: String,
    pub acodec: String,
    /// e.g. `yuv420p`, `yuv420p10le` — decides whether H.264 is playable at all.
    pub pix_fmt: String,
    /// e.g. `High`, `High 10`, `High 4:4:4 Predictive`.
    pub profile: String,
    /// e.g. `avc1`, `hvc1`, `hev1`.
    pub vtag: String,
    pub format_name: String,
    pub bitrate: u64,
    pub size: u64,
    pub channels: u32,
    pub sample_rate: u32,
    pub has_video: bool,
    pub has_audio: bool,
    /// >0 when the video stream reorders frames (B-frames). Matters for
    /// containers that can't store a presentation timestamp.
    pub has_b_frames: u32,
    /// How iPlayer will feed this file to the webview.
    /// one of: `direct` | `remux` | `transcode`
    pub plan: String,
    /// Why a derivative is needed (empty when the file plays as-is).
    pub plan_reason: String,
}

fn parse_fps(s: &str) -> f64 {
    let mut parts = s.split('/');
    let num = parts.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let den = parts.next().and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
    if den > 0.0 && num > 0.0 {
        num / den
    } else {
        0.0
    }
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

    let out = cmd
        .output()
        .map_err(|e| format!("无法运行 ffprobe: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("无法解析媒体信息: {}", err.trim()));
    }

    let root: ProbeRoot =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("解析媒体信息失败: {e}"))?;

    let video = root.streams.iter().find(|s| s.codec_type == "video");
    let audio = root.streams.iter().find(|s| s.codec_type == "audio");
    let fmt = root.format.as_ref();

    let duration = fmt
        .map(|f| f.duration.parse::<f64>().unwrap_or(0.0))
        .unwrap_or(0.0);
    let bitrate = fmt
        .map(|f| f.bit_rate.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);
    let size = fmt
        .map(|f| f.size.parse::<u64>().unwrap_or(0))
        .unwrap_or(0);

    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let format_name = fmt.map(|f| f.format_name.clone()).unwrap_or_default();

    let vcodec = video.map(|v| v.codec_name.clone()).unwrap_or_default();
    let acodec = audio.map(|a| a.codec_name.clone()).unwrap_or_default();
    let pix_fmt = video.map(|v| v.pix_fmt.clone()).unwrap_or_default();
    let profile = video.map(|v| v.profile.clone()).unwrap_or_default();
    // Matroska/AVI report the tag as "[0][0][0][0]" — meaningless for the webview.
    let vtag = video
        .map(|v| v.codec_tag_string.clone())
        .unwrap_or_default();
    let vtag = if vtag.starts_with('[') { String::new() } else { vtag };
    let has_video = video.is_some();
    let has_audio = audio.is_some();
    let has_b_frames = video.map(|v| v.has_b_frames).unwrap_or(0);

    let traits = StreamTraits {
        ext: &ext,
        vcodec: &vcodec,
        acodec: &acodec,
        pix_fmt: &pix_fmt,
        vtag: &vtag,
        has_video,
        has_audio,
        has_b_frames,
    };
    let plan = plan_for(&traits).to_string();
    let plan_reason = if plan == "direct" {
        String::new()
    } else {
        explain_plan(&traits)
    };

    Ok(MediaInfo {
        path: path.to_string_lossy().to_string(),
        ext,
        kind: if has_video { "video" } else { "audio" }.to_string(),
        duration,
        width: video.map(|v| v.width).unwrap_or(0),
        height: video.map(|v| v.height).unwrap_or(0),
        fps: video
            .map(|v| {
                let a = parse_fps(&v.avg_frame_rate);
                if a > 0.0 {
                    a
                } else {
                    parse_fps(&v.r_frame_rate)
                }
            })
            .unwrap_or(0.0),
        vcodec,
        acodec,
        pix_fmt,
        profile,
        vtag,
        format_name,
        bitrate,
        size,
        channels: audio.map(|a| a.channels).unwrap_or(0),
        sample_rate: audio
            .and_then(|a| a.sample_rate.parse::<u32>().ok())
            .unwrap_or(0),
        has_video,
        has_audio,
        has_b_frames,
        plan,
        plan_reason,
    })
}

/// Decide how to deliver the file to the webview.
///
/// A derivative is only asked for when the webview genuinely cannot take the
/// original. Streams that are already decodable are never re-encoded — an
/// unsupported *audio* track, a `hev1` HEVC tag or an odd container all cost a
/// fast `-c copy` (plus, at most, an audio-only re-encode), not a full
/// video re-encode.
pub fn plan_for(t: &StreamTraits) -> &'static str {
    let direct = NATIVE_CONTAINERS.contains(&t.ext)
        && (!t.has_video || video_playable(t))
        && (!t.has_audio || audio_fits(t.ext, t.acodec));
    if direct {
        return "direct";
    }

    // Keep the picture as-is whenever its bitstream is decodable; only the
    // wrapping (container / tag / audio track) needs work.
    if needs_reencode_video(t) {
        "transcode"
    } else {
        "remux"
    }
}

/// Human-readable "why does this need a derivative?" note, shown in the info
/// panel so a transcoding job is never a black box.
pub fn explain_plan(t: &StreamTraits) -> String {
    let mut parts: Vec<String> = Vec::new();
    let video_fine = !needs_reencode_video(t);

    if t.has_video {
        if !video_stream_ok(t.vcodec, t.pix_fmt) {
            if !NATIVE_VCODECS.contains(&t.vcodec) {
                parts.push(format!("视频编码 {} 无法由播放内核解码，必须重新编码", t.vcodec));
            } else if t.pix_fmt.contains("444") || t.pix_fmt.contains("422") {
                parts.push(format!(
                    "{} 的 {} 色度采样不受支持，必须重新编码",
                    t.vcodec, t.pix_fmt
                ));
            } else {
                parts.push(format!(
                    "{} 的 {} 位深不受支持（仅支持 8-bit 4:2:0），必须重新编码",
                    t.vcodec, t.pix_fmt
                ));
            }
        } else if container_lacks_pts(t.ext) && t.has_b_frames > 0 {
            parts.push(format!(
                ".{} 容器不保存显示时间戳，视频含 {} 帧，重新封装会导致画面错序，必须重新编码",
                t.ext, "B"
            ));
        } else if !NATIVE_CONTAINERS.contains(&t.ext) {
            parts.push(format!("容器 .{} 需转封装为 {}", t.ext, remux_container(t.vcodec)));
        } else if !container_fits_video(t.ext, t.vcodec) {
            parts.push(format!("{} 不宜放在 .{} 中，需更换容器", t.vcodec, t.ext));
        } else if matches!(t.vcodec, "hevc" | "h265") && !(t.vtag.is_empty() || t.vtag == "hvc1") {
            parts.push(format!("HEVC 标签 {} 需改写为 hvc1", t.vtag));
        }
    }

    // Audio-only notes are only useful when the picture is already fine.
    if t.has_audio && video_fine {
        let target = remux_target_container(t.ext, t.vcodec, t.has_video, t.acodec);
        let to = if target == "webm" { "opus" } else { "aac" };
        if !NATIVE_ACODECS.contains(&t.acodec) {
            parts.push(format!("音轨 {} 无法解码，需转换为 {to}", t.acodec));
        } else if !audio_fits(&target, t.acodec) {
            parts.push(format!(
                "音轨 {} 与 {target} 容器不兼容，需重编码为 {to}",
                t.acodec
            ));
        }
    }

    if parts.is_empty() {
        "需要生成可播放副本".to_string()
    } else {
        parts.join("；")
    }
}

/// Container a `remux` plan will target for this file.
pub fn remux_target_container(ext: &str, vcodec: &str, has_video: bool, acodec: &str) -> String {
    if has_video {
        return remux_container(vcodec).to_string();
    }
    if NATIVE_CONTAINERS.contains(&ext) && audio_fits(ext, acodec) {
        ext.to_string()
    } else {
        "m4a".to_string()
    }
}

/// Container to target for a `remux` plan.
pub fn remux_container(vcodec: &str) -> &'static str {
    if matches!(vcodec, "vp8" | "vp9" | "av1") {
        "webm"
    } else {
        "mp4"
    }
}

/// Stable cache key for a source file (path + size + mtime).
pub fn cache_key(path: &Path) -> String {
    let meta = fs::metadata(path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta.as_ref().map(modified_secs).unwrap_or(0);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    for b in size.to_le_bytes().iter().chain(mtime.to_le_bytes().iter()) {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Turn any path into a safe file stem for derived outputs.
pub fn safe_stem(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "output".to_string());
    let cleaned: String = stem
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    if cleaned.trim().is_empty() {
        "output".to_string()
    } else {
        cleaned
    }
}

pub fn sibling_path(src: &Path, suffix: &str) -> PathBuf {
    let dir = src.parent().unwrap_or(Path::new("."));
    let stem = safe_stem(src);
    dir.join(format!("{stem}{suffix}"))
}

/// `clip.mp4` -> `clip_snapshot_20260101-120000.png` (timestamp added by caller)
pub fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert to a local-ish readable stamp without pulling in chrono:
    // days since epoch -> Y-M-D calculation.
    let days = now / 86_400;
    let secs = now % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod planner_tests {
    use super::*;

    fn p(ext: &str, v: &str, pf: &str, tag: &str, a: &str) -> &'static str {
        t(ext, v, pf, tag, a, 0)
    }

    fn t(ext: &str, v: &str, pf: &str, tag: &str, a: &str, bf: u32) -> &'static str {
        plan_for(&StreamTraits {
            ext,
            vcodec: v,
            acodec: a,
            pix_fmt: pf,
            vtag: tag,
            has_video: !v.is_empty(),
            has_audio: !a.is_empty(),
            has_b_frames: bf,
        })
    }

    #[test]
    fn matrix() {
        let cases: &[(&str, &str)] = &[
            // --- 主流 MP4：直接播放 ---
            (p("mp4", "h264", "yuv420p", "avc1", "aac"), "direct"),
            // --- MP4 里是 DivX/Xvid(MPEG-4 Part 2)：必须转码 ---
            (p("mp4", "mpeg4", "yuv420p", "", "aac"), "transcode"),
            // --- 10-bit H.264 (Hi10P)：必须转码 ---
            (p("mp4", "h264", "yuv420p10le", "avc1", "aac"), "transcode"),
            (p("mkv", "h264", "yuv420p10le", "", "aac"), "transcode"),
            // --- 4:4:4：必须转码 ---
            (p("mp4", "h264", "yuv444p", "avc1", "aac"), "transcode"),
            // --- 音频不支持(AC-3/DTS)：视频直接拷贝，仅重编码音频 ---
            (p("mp4", "h264", "yuv420p", "avc1", "ac3"), "remux"),
            (p("mov", "h264", "yuv420p", "avc1", "dts"), "remux"),
            (p("mp4", "h264", "yuv420p", "avc1", "eac3"), "remux"),
            // --- HEVC hev1 标签：只需改写标签 ---
            (p("mp4", "hevc", "yuv420p", "hev1", "aac"), "remux"),
            (p("mp4", "hevc", "yuv420p", "hvc1", "aac"), "direct"),
            // --- MKV / RMVB：转封装 ---
            (p("mkv", "h264", "yuv420p", "", "aac"), "remux"),
            (p("mkv", "h264", "yuv420p", "", "flac"), "remux"),
            (p("mkv", "h264", "yuv420p", "", "opus"), "remux"),
            (p("rmvb", "rv40", "yuv420p", "", "cook"), "transcode"),
            // --- AVI：无 B 帧可转封装，有 B 帧必须转码（AVI 存不了 PTS）---
            (t("avi", "h264", "yuv420p", "H264", "mp3", 0), "remux"),
            (t("avi", "h264", "yuv420p", "H264", "mp3", 2), "transcode"),
            (t("avi", "h264", "yuv420p", "H264", "pcm_s16le", 0), "remux"),
            (p("avi", "mpeg4", "yuv420p", "FMP4", "mp3"), "transcode"),
            (p("avi", "mjpeg", "yuvj444p", "MJPG", "pcm_s16le"), "transcode"),
            // --- FLV：h264 可转封装（FLV 能存显示时间戳），老式 flv1 必须转码 ---
            (t("flv", "h264", "yuv420p", "", "aac", 2), "remux"),
            (p("flv", "flv1", "yuv420p", "", "mp3"), "transcode"),
            // --- WMV / VC-1：一律转码 ---
            (p("wmv", "wmv2", "yuv420p", "WMV2", "wmav2"), "transcode"),
            (p("wmv", "vc1", "yuv420p", "", "wmav2"), "transcode"),
            (p("asf", "msmpeg4v2", "yuv420p", "MP42", "wmav2"), "transcode"),
            // --- VP9/AV1 在 MP4 里要换到 WebM ---
            (p("mp4", "vp9", "yuv420p", "", "opus"), "remux"),
            (p("webm", "vp9", "yuv420p", "", "opus"), "direct"),
            (p("webm", "h264", "yuv420p", "", "aac"), "remux"),
            // --- ISO-BMFF 家族（3gp/3g2/f4v）与 mp4 同等待遇 ---
            (p("3gp", "h264", "yuv420p", "avc1", "aac"), "direct"),
            (p("3g2", "h264", "yuv420p", "avc1", "aac"), "direct"),
            (p("f4v", "h264", "yuv420p", "avc1", "aac"), "direct"),
            // 3GP 常见的老编码与 AMR 音轨：转封装（音轨转 aac）
            (p("3gp", "h263", "yuv420p", "s263", "samr"), "transcode"),
            (p("3gp", "mpeg4", "yuv420p", "", "samr"), "transcode"),
            (t("3gp", "h264", "yuv420p", "avc1", "samr", 1), "remux"),
            // --- MPEG-TS / 老容器：一律转封装起步 ---
            (p("ts", "h264", "yuv420p", "", "aac"), "remux"),
            (p("m2ts", "h264", "yuv420p", "", "ac3"), "remux"),
            (p("mpg", "mpeg2video", "yuv420p", "", "mp2"), "transcode"),
            (p("vob", "mpeg2video", "yuv420p", "", "ac3"), "transcode"),
            (p("dv", "dvvideo", "yuv420p", "", "pcm_s16le"), "transcode"),
            (p("ogv", "theora", "yuv420p", "", "vorbis"), "transcode"),
            (p("amv", "amv", "yuv420p", "", "adpcm_ima_amv"), "transcode"),
            (p("mxf", "mpeg2video", "yuv420p", "", "pcm_s16le"), "transcode"),
            (p("rm", "rv30", "yuv420p", "", "cook"), "transcode"),
            // --- 仅音频 ---
            (p("mp3", "", "", "", "mp3"), "direct"),
            (p("flac", "", "", "", "flac"), "direct"),
            (p("ape", "", "", "", "ape"), "remux"),
            (p("mka", "", "", "", "opus"), "remux"),
        ];
        let mut bad = Vec::new();
        for (got, want) in cases {
            if got != want {
                bad.push(format!("want {want}, got {got}"));
            }
        }
        assert!(bad.is_empty(), "{bad:#?}");
    }

    #[test]
    fn audio_container_fit() {
        assert!(audio_fits("mp4", "aac"));
        assert!(audio_fits("mov", "pcm_s16le"));
        assert!(!audio_fits("mp4", "ac3"));
        assert!(!audio_fits("mp4", "opus"));
        assert!(!audio_fits("mp4", "flac"));
        assert!(audio_fits("webm", "opus"));
        assert!(!audio_fits("webm", "aac"));
        assert!(audio_fits("flac", "flac"));
        assert_eq!(remux_target_container("mkv", "h264", true, "aac"), "mp4");
        assert_eq!(remux_target_container("mp4", "vp9", true, "opus"), "webm");
        assert_eq!(remux_target_container("ape", "", false, "ape"), "m4a");
    }

    fn why(ext: &str, v: &str, pf: &str, tag: &str, a: &str, bf: u32) -> String {
        explain_plan(&StreamTraits {
            ext,
            vcodec: v,
            acodec: a,
            pix_fmt: pf,
            vtag: tag,
            has_video: !v.is_empty(),
            has_audio: !a.is_empty(),
            has_b_frames: bf,
        })
    }

    #[test]
    fn reasons() {
        let r = why("mp4", "mpeg4", "yuv420p", "", "aac", 0);
        assert!(r.contains("mpeg4"), "{r}");
        let r = why("mp4", "h264", "yuv420p", "avc1", "ac3", 0);
        assert!(r.contains("ac3"), "{r}");
        assert!(!r.contains("视频"), "{r}");
        let r = why("mkv", "h264", "yuv420p", "", "aac", 0);
        assert!(r.contains("转封装"), "{r}");
        // AVI + B 帧的解释必须点到「不保存显示时间戳」
        let r = why("avi", "h264", "yuv420p", "", "mp3", 2);
        assert!(r.contains("显示时间戳"), "{r}");
        assert!(r.contains("必须重新编码"), "{r}");
    }
}

