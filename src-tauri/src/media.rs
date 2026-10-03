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

/// Containers a WebKit/Chromium webview can demux on its own.
const NATIVE_CONTAINERS: &[&str] = &[
    "mp4", "m4v", "m4a", "mov", "webm", "mp3", "aac", "wav", "wave", "flac", "ogg", "oga", "opus",
];
/// Video codecs the webview can decode.
const NATIVE_VCODECS: &[&str] = &["h264", "avc1", "hevc", "h265", "vp8", "vp9", "av1"];
/// Audio codecs the webview can decode.
const NATIVE_ACODECS: &[&str] = &["aac", "mp3", "opus", "vorbis", "flac", "alac", "pcm_s16le", "pcm_s24le"];

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
    pub format_name: String,
    pub bitrate: u64,
    pub size: u64,
    pub channels: u32,
    pub sample_rate: u32,
    pub has_video: bool,
    pub has_audio: bool,
    /// How iPlayer will feed this file to the webview.
    /// one of: `direct` | `remux` | `transcode`
    pub plan: String,
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
    let has_video = video.is_some();
    let has_audio = audio.is_some();

    let plan = plan_for(&ext, &vcodec, &acodec, has_video, has_audio).to_string();

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
        format_name,
        bitrate,
        size,
        channels: audio.map(|a| a.channels).unwrap_or(0),
        sample_rate: audio
            .and_then(|a| a.sample_rate.parse::<u32>().ok())
            .unwrap_or(0),
        has_video,
        has_audio,
        plan,
    })
}

/// Decide how to deliver the file to the webview.
pub fn plan_for(ext: &str, vcodec: &str, acodec: &str, has_video: bool, has_audio: bool) -> &'static str {
    let container_ok = NATIVE_CONTAINERS.contains(&ext);
    let v_ok = !has_video || NATIVE_VCODECS.contains(&vcodec);
    let a_ok = !has_audio || NATIVE_ACODECS.contains(&acodec);

    if container_ok && v_ok && a_ok {
        "direct"
    } else if v_ok && a_ok {
        // Codecs are fine, only the container needs rewrapping — fast.
        "remux"
    } else {
        "transcode"
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
