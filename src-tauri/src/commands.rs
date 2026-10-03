//! Tauri commands exposed to the frontend.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewWindow};

use crate::ffmpeg::{self, ToolStatus};
use crate::media::{self, MediaFile, MediaInfo};

const MEDIA_PROGRESS: &str = "media-progress";
const TASK_PROGRESS: &str = "task-progress";

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Grant the webview's `asset:` protocol access to a single file so it can be
/// used as a `<video src>`.
fn allow_path(app: &AppHandle, p: &Path) -> Result<(), String> {
    app.asset_protocol_scope()
        .allow_file(p)
        .map_err(|e| format!("无法授权读取 {}: {e}", p.display()))
}

fn allow_dir(app: &AppHandle, p: &Path) -> Result<(), String> {
    app.asset_protocol_scope()
        .allow_directory(p, true)
        .map_err(|e| format!("无法授权读取 {}: {e}", p.display()))
}

fn playback_cache_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_cache_dir()
        .map_err(|e| e.to_string())?
        .join("playback");
    fs::create_dir_all(&dir).map_err(|e| format!("无法创建缓存目录: {e}"))?;
    Ok(dir)
}

#[cfg(target_os = "windows")]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}
#[cfg(not(target_os = "windows"))]
fn hide_console(_cmd: &mut Command) {}

// ---------------------------------------------------------------------------
// tool status
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn tool_status() -> ToolStatus {
    ffmpeg::status()
}

// ---------------------------------------------------------------------------
// file listing
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn scan_folder(app: AppHandle, dir: String, recursive: Option<bool>) -> Result<Vec<MediaFile>, String> {
    let path = PathBuf::from(&dir);
    let files = media::scan_dir(&path, recursive.unwrap_or(false))?;
    if path.is_dir() {
        allow_dir(&app, &path)?;
    }
    Ok(files)
}

// ---------------------------------------------------------------------------
// probing
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn probe_media(path: String) -> Result<MediaInfo, String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    tauri::async_runtime::spawn_blocking(move || media::probe(&p))
        .await
        .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// playback preparation (direct / remux / transcode)
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone)]
pub struct PlaybackSource {
    /// File the webview should load (original or cached derivative).
    pub path: String,
    /// `direct` | `remux` | `transcode`
    pub plan: String,
    /// true when the derivative already existed in the cache
    pub cached: bool,
    /// full description of the *original* file
    pub info: MediaInfo,
}

#[tauri::command]
pub async fn prepare_playback(app: AppHandle, path: String) -> Result<PlaybackSource, String> {
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("文件不存在: {path}"));
    }

    let info = {
        let src = src.clone();
        tauri::async_runtime::spawn_blocking(move || media::probe(&src))
            .await
            .map_err(|e| e.to_string())??
    };

    let base = PlaybackSource {
        path: path.clone(),
        plan: info.plan.clone(),
        cached: false,
        info: info.clone(),
    };

    if info.plan == "direct" {
        allow_path(&app, &src)?;
        return Ok(base);
    }

    // ---- needs a derivative -------------------------------------------------
    let cache = playback_cache_dir(&app)?;
    let key = media::cache_key(&src);
    let ext = target_ext(&info);
    let out = cache.join(format!("{key}.{ext}"));

    if out.is_file() && fs::metadata(&out).map(|m| m.len() > 1024).unwrap_or(false) {
        allow_path(&app, &out)?;
        return Ok(PlaybackSource {
            path: out.to_string_lossy().to_string(),
            cached: true,
            ..base
        });
    }
    let _ = fs::remove_file(&out);

    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());
    let label = if info.plan == "remux" {
        format!("正在转封装 {name}")
    } else {
        format!("正在转换格式 {name}")
    };

    let out_for_task = out.clone();
    let src_for_task = src.clone();
    let info_for_task = info.clone();
    let app_for_task = app.clone();

    tauri::async_runtime::spawn_blocking(move || {
        build_derivative(&app_for_task, &src_for_task, &info_for_task, &out_for_task, &label)
    })
    .await
    .map_err(|e| e.to_string())??;

    allow_path(&app, &out)?;
    Ok(PlaybackSource {
        path: out.to_string_lossy().to_string(),
        cached: false,
        ..base
    })
}

fn target_ext(info: &MediaInfo) -> String {
    if info.plan == "remux" {
        if info.has_video {
            media::remux_container(&info.vcodec).to_string()
        } else {
            "m4a".to_string()
        }
    } else if !info.has_video {
        "m4a".to_string()
    } else {
        "mp4".to_string()
    }
}

fn build_derivative(
    app: &AppHandle,
    src: &Path,
    info: &MediaInfo,
    out: &Path,
    label: &str,
) -> Result<(), String> {
    let mut cmd = ffmpeg::base_command()?;
    cmd.arg("-y").arg("-i").arg(src);

    let is_mp4_like = out
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "mp4" || e == "m4a")
        .unwrap_or(false);

    if info.plan == "remux" {
        // Codecs are already web friendly — only rewrap the container.
        cmd.args(["-c", "copy", "-sn", "-dn"]);
        if is_mp4_like {
            cmd.args(["-movflags", "+faststart"]);
        }
    } else if !info.has_video {
        // Audio-only source in an exotic container -> re-encode to AAC/m4a.
        cmd.args(["-vn", "-sn", "-dn", "-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
        cmd.args(["-movflags", "+faststart"]);
    } else {
        let encoders = ffmpeg::status().encoders;
        let enc = ffmpeg::h264_encoder(&encoders);
        cmd.args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"]);
        cmd.args(["-c:v", enc]);
        if enc == "libx264" {
            cmd.args(["-preset", "veryfast", "-crf", "23"]);
        } else {
            cmd.args(["-b:v", "6000k", "-allow_sw", "1"]);
        }
        cmd.args(["-pix_fmt", "yuv420p"]);
        if info.has_audio {
            cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
        }
        cmd.args(["-movflags", "+faststart"]);
    }

    cmd.arg(out);
    ffmpeg::run_with_progress(cmd, app, MEDIA_PROGRESS, info.duration, label)
}

// ---------------------------------------------------------------------------
// snapshot
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn snapshot(
    app: AppHandle,
    path: String,
    time: f64,
    out: Option<String>,
) -> Result<String, String> {
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let target = match out.filter(|s| !s.trim().is_empty()) {
        Some(o) => PathBuf::from(o),
        None => media::sibling_path(
            &src,
            &format!("_shot_{}.png", media::timestamp()),
        ),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法创建目录: {e}"))?;
    }

    let app2 = app.clone();
    let t = time.max(0.0);
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let mut cmd = ffmpeg::base_command()?;
        cmd.arg("-y")
            .arg("-ss")
            .arg(format!("{t:.3}"))
            .arg("-i")
            .arg(&src)
            .args(["-frames:v", "1", "-an", "-sn", "-dn", "-q:v", "2"]);
        cmd.arg(&target);
        ffmpeg::run_with_progress(cmd, &app2, TASK_PROGRESS, 0.0, "正在截图")?;
        Ok(target.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// audio extraction
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn extract_audio(
    app: AppHandle,
    path: String,
    format: Option<String>,
    out: Option<String>,
) -> Result<String, String> {
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let fmt = format.unwrap_or_else(|| "m4a".to_string());
    let ext = match fmt.as_str() {
        "mp3" => "mp3",
        "wav" => "wav",
        "flac" => "flac",
        "copy" => "mka",
        _ => "m4a",
    };
    let target = match out.filter(|s| !s.trim().is_empty()) {
        Some(o) => PathBuf::from(o),
        None => media::sibling_path(&src, &format!(".{ext}")),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法创建目录: {e}"))?;
    }

    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let mut cmd = ffmpeg::base_command()?;
        cmd.args(["-y", "-i"]).arg(&src).args(["-vn", "-sn", "-dn"]);
        match fmt.as_str() {
            "mp3" => {
                cmd.args(["-c:a", "libmp3lame", "-q:a", "2"]);
            }
            "wav" => {
                cmd.args(["-c:a", "pcm_s16le"]);
            }
            "flac" => {
                cmd.args(["-c:a", "flac"]);
            }
            "copy" => {
                cmd.args(["-c:a", "copy"]);
            }
            _ => {
                cmd.args(["-c:a", "aac", "-b:a", "256k", "-movflags", "+faststart"]);
            }
        }
        cmd.arg(&target);
        ffmpeg::run_with_progress(cmd, &app2, TASK_PROGRESS, 0.0, "正在提取音频")?;
        Ok(target.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// GIF export
// ---------------------------------------------------------------------------

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn make_gif(
    app: AppHandle,
    path: String,
    start: f64,
    end: f64,
    fps: Option<u32>,
    width: Option<u32>,
    dither: Option<bool>,
    out: Option<String>,
) -> Result<String, String> {
    let src = PathBuf::from(&path);
    if !src.is_file() {
        return Err(format!("文件不存在: {path}"));
    }
    let start = start.max(0.0);
    if end <= start {
        return Err("结束时间必须大于开始时间".to_string());
    }
    let dur = end - start;
    let fps = fps.unwrap_or(12).clamp(1, 50);
    let width = width.unwrap_or(480).clamp(32, 1920);
    let dither = dither.unwrap_or(true);

    let target = match out.filter(|s| !s.trim().is_empty()) {
        Some(o) => PathBuf::from(o),
        None => media::sibling_path(&src, &format!("_{}s.gif", start.round() as i64)),
    };
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("无法创建目录: {e}"))?;
    }

    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<String, String> {
        let tmp = std::env::temp_dir().join(format!(
            "iplayer_palette_{}.png",
            std::process::id()
        ));

        // Pass 1 — build an optimal palette from the selection.
        let mut p1 = ffmpeg::base_command()?;
        p1.arg("-y")
            .arg("-ss")
            .arg(format!("{start:.3}"))
            .arg("-t")
            .arg(format!("{dur:.3}"))
            .arg("-i")
            .arg(&src)
            .args([
                "-vf",
                &format!("fps={fps},scale={width}:-1:flags=lanczos,palettegen=stats_mode=diff"),
                "-an",
            ])
            .arg(&tmp);
        ffmpeg::run_with_progress(p1, &app2, TASK_PROGRESS, dur, "正在分析调色板")?;

        // Pass 2 — render the GIF with the generated palette.
        let dither_expr = if dither {
            "dither=bayer:bayer_scale=5:diff_mode=rectangle"
        } else {
            "dither=none"
        };
        let filter = format!(
            "fps={fps},scale={width}:-1:flags=lanczos[x];[x][1:v]paletteuse={dither_expr}"
        );
        let mut p2 = ffmpeg::base_command()?;
        p2.arg("-y")
            .arg("-ss")
            .arg(format!("{start:.3}"))
            .arg("-t")
            .arg(format!("{dur:.3}"))
            .arg("-i")
            .arg(&src)
            .arg("-i")
            .arg(&tmp)
            .args(["-lavfi", &filter, "-loop", "0"])
            .arg(&target);
        let result = ffmpeg::run_with_progress(p2, &app2, TASK_PROGRESS, dur, "正在生成 GIF");

        let _ = fs::remove_file(&tmp);
        result?;
        Ok(target.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------------------------------------------------------------------------
// filesystem / shell niceties
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn reveal_in_finder(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在: {path}"));
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg("-R").arg(&p);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("explorer");
        c.arg(format!("/select,{}", p.display()));
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(p.parent().unwrap_or(Path::new(".")));
        c
    };

    hide_console(&mut cmd);
    cmd.spawn().map_err(|e| format!("无法打开访达: {e}"))?;
    Ok(())
}

#[tauri::command]
pub fn open_path(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在: {path}"));
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(&p);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", &path]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(&p);
        c
    };

    hide_console(&mut cmd);
    cmd.spawn().map_err(|e| format!("无法打开: {e}"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// window behaviour
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn set_always_on_top(window: WebviewWindow, value: bool) -> Result<(), String> {
    window.set_always_on_top(value).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_resizable(window: WebviewWindow, value: bool) -> Result<(), String> {
    window.set_resizable(value).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_fullscreen(window: WebviewWindow, value: bool) -> Result<bool, String> {
    window.set_fullscreen(value).map_err(|e| e.to_string())?;
    window.is_fullscreen().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn set_theme(window: WebviewWindow, theme: Option<String>) -> Result<(), String> {
    let t = match theme.as_deref() {
        Some("dark") => Some(tauri::Theme::Dark),
        Some("light") => Some(tauri::Theme::Light),
        _ => None, // follow the system
    };
    window.set_theme(t).map_err(|e| e.to_string())
}
