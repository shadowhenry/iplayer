//! Tauri commands exposed to the frontend.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};

use crate::ffmpeg::{self, Cancel, ToolStatus};
use crate::media::{self, MediaFile, MediaInfo};
use crate::stream::{StreamStart, StreamStatus};

const MEDIA_PROGRESS: &str = "media-progress";
const TASK_PROGRESS: &str = "task-progress";

// ---------------------------------------------------------------------------
// the one derivative being built right now
// ---------------------------------------------------------------------------

/// A full re-encode takes minutes, and the user is free to wander off to another
/// file while it runs. Only one derivative is ever worth building, so the newest
/// request cancels whatever came before it — and the frontend can cancel too,
/// the instant it decides the file is no longer wanted.
static BUILDING: OnceLock<Mutex<Option<Arc<Cancel>>>> = OnceLock::new();

fn building() -> &'static Mutex<Option<Arc<Cancel>>> {
    BUILDING.get_or_init(|| Mutex::new(None))
}

/// Claim the slot for a new derivative, aborting the previous one.
fn claim_build() -> Arc<Cancel> {
    let token = Cancel::new();
    if let Ok(mut slot) = building().lock() {
        if let Some(prev) = slot.take() {
            if !Arc::ptr_eq(&prev, &token) {
                prev.cancel();
            }
        }
        *slot = Some(token.clone());
    }
    token
}

/// Hand the slot back, but only if it is still ours.
fn release_build(token: &Arc<Cancel>) {
    if let Ok(mut slot) = building().lock() {
        if slot.as_ref().map(|c| Arc::ptr_eq(c, token)).unwrap_or(false) {
            *slot = None;
        }
    }
}

/// Stop building a derivative — the user has moved on to another file. Returns
/// at once: the encoder notices within a fraction of a second and the caller
/// simply drops the (partial) result.
#[tauri::command]
pub fn cancel_playback() {
    if let Ok(slot) = building().lock() {
        if let Some(c) = slot.as_ref() {
            c.cancel();
        }
    }
}

/// Public alias so the app can cancel on exit as well.
pub fn cancel_build() {
    cancel_playback();
}

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
pub async fn prepare_playback(
    app: AppHandle,
    path: String,
    force_transcode: Option<bool>,
) -> Result<PlaybackSource, String> {
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

    // `force_transcode` is the frontend's escape hatch: the webview refused a
    // file we planned to play as-is (10-bit H.264, exotic profile, …). Re-encode
    // it rather than trusting the original plan.
    let force = force_transcode.unwrap_or(false) && info.has_video;
    let plan = if force {
        "transcode".to_string()
    } else {
        info.plan.clone()
    };

    let base = PlaybackSource {
        path: path.clone(),
        plan: plan.clone(),
        cached: false,
        info: info.clone(),
    };

    if plan == "direct" {
        allow_path(&app, &src)?;
        return Ok(base);
    }

    // ---- needs a derivative -------------------------------------------------
    let cache = playback_cache_dir(&app)?;
    // "v3": the planner became container-aware and remux now repairs audio
    // tracks — derivatives from older builds can't be trusted.
    let mut key = format!("v3{}", media::cache_key(&src));
    if force {
        // Keep the forced re-encode apart from the normal derivative.
        key.push('t');
    }
    let ext = target_ext(&info, &plan);
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
    let label = if plan == "remux" {
        format!("正在转封装 {name}")
    } else {
        format!("正在转换格式 {name}")
    };

    // Prime the overlay with *why* this file can't be handed to the webview
    // as-is — a bare percentage on a ten-minute re-encode explains nothing.
    let _ = app.emit(
        MEDIA_PROGRESS,
        serde_json::json!({
            "percent": 0.0,
            "label": label,
            "reason": info.plan_reason,
        }),
    );

    let out_for_task = out.clone();
    let src_for_task = src.clone();
    let info_for_task = info.clone();
    let app_for_task = app.clone();
    let plan_for_task = plan.clone();
    let token = claim_build();
    let token_for_task = token.clone();

    let built = tauri::async_runtime::spawn_blocking(move || {
        build_derivative(
            &app_for_task,
            &src_for_task,
            &info_for_task,
            &plan_for_task,
            &out_for_task,
            &label,
            &token_for_task,
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    release_build(&token);

    if let Err(e) = built {
        // A half-finished re-encode must never be left where the cache check
        // above would happily pick it up next time.
        let _ = fs::remove_file(&out);
        return Err(e);
    }

    allow_path(&app, &out)?;
    Ok(PlaybackSource {
        path: out.to_string_lossy().to_string(),
        cached: false,
        ..base
    })
}

fn target_ext(info: &MediaInfo, plan: &str) -> String {
    if plan == "remux" {
        media::remux_target_container(&info.ext, &info.vcodec, info.has_video, &info.acodec)
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
    plan: &str,
    out: &Path,
    label: &str,
    cancel: &Arc<Cancel>,
) -> Result<(), String> {
    if plan == "remux" {
        let container = out
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4")
            .to_string();
        // Audio the target container can't carry (AC-3, DTS, Opus-in-MP4…) is
        // re-encoded on its own; the picture is still copied untouched.
        let audio_reencode = info.has_audio && !media::audio_fits(&container, &info.acodec);

        let copy_res = run_remux(app, src, info, audio_reencode, out, label, cancel);
        if copy_res.is_err() && cancel.is_cancelled() {
            return copy_res;
        }
        // `ffmpeg -c copy` can exit 0 and still write a truncated/invalid file,
        // so trust only a derivative we can actually probe back.
        if copy_res.is_ok() && verify_derivative(out, info) {
            return Ok(());
        }
        let copy_err = copy_res.err();
        let _ = fs::remove_file(out);
        let retry_label = format!("{label}（转封装失败，改用格式转换）");
        return run_transcode(app, src, info, out, &retry_label, cancel).map_err(|e| match copy_err {
            Some(c) => format!("{c}；重试转码仍失败: {e}"),
            None => format!("转封装产物无法播放；重试转码仍失败: {e}"),
        });
    }

    if info.has_video {
        run_transcode(app, src, info, out, label, cancel)?;
    } else {
        run_audio_transcode(app, src, info, out, label, cancel)?;
    }
    if !verify_derivative(out, info) {
        let _ = fs::remove_file(out);
        return Err("生成的播放副本无法解析，请检查源文件是否损坏".to_string());
    }
    Ok(())
}

/// Make sure the derivative actually parses, carries the streams we expect and
/// would itself be handed to the webview without further work.
fn verify_derivative(out: &Path, info: &MediaInfo) -> bool {
    if !out.is_file() {
        return false;
    }
    match media::probe(out) {
        Ok(p) => {
            p.duration > 0.0
                && p.has_video == info.has_video
                && (!info.has_audio || p.has_audio)
                // A `-c copy` can exit 0 and still produce something we would
                // refuse to play (truncated `hev1`→`hvc1` retag, dropped track…).
                && p.plan == "direct"
        }
        Err(_) => false,
    }
}

/// Rewrap streams without re-encoding (`-c copy`). When the target container
/// cannot carry the audio track as-is, only the audio is re-encoded.
fn run_remux(
    app: &AppHandle,
    src: &Path,
    info: &MediaInfo,
    audio_reencode: bool,
    out: &Path,
    label: &str,
    cancel: &Arc<Cancel>,
) -> Result<(), String> {
    let mut cmd = ffmpeg::base_command()?;
    cmd.arg("-y").arg("-i").arg(src);
    cmd.args(["-map", "0:v:0?", "-map", "0:a:0?", "-sn", "-dn"]);

    if info.has_video {
        cmd.args(["-c:v", "copy"]);
        // WebKit only plays `hvc1`-tagged HEVC inside MP4/MOV, and `-c copy`
        // keeps whatever tag the source carried — force the tag it expects.
        if is_mp4_like(out) && matches!(info.vcodec.as_str(), "hevc" | "h265") {
            cmd.args(["-tag:v", "hvc1"]);
        }
    }
    if info.has_audio {
        if audio_reencode {
            if out.extension().and_then(|e| e.to_str()) == Some("webm") {
                cmd.args(["-c:a", "libopus", "-b:a", "128k", "-ac", "2"]);
            } else {
                cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
            }
        } else {
            cmd.args(["-c:a", "copy"]);
        }
    }
    if is_mp4_like(out) {
        cmd.args(["-movflags", "+faststart"]);
    }
    cmd.arg(out);
    ffmpeg::run_cancellable(cmd, app, MEDIA_PROGRESS, info.duration, label, Some(cancel))
}

fn is_mp4_like(out: &Path) -> bool {
    out.extension()
        .and_then(|e| e.to_str())
        .map(|e| e == "mp4" || e == "m4a")
        .unwrap_or(false)
}

/// Audio-only source in an exotic container -> re-encode to AAC/m4a.
fn run_audio_transcode(
    app: &AppHandle,
    src: &Path,
    info: &MediaInfo,
    out: &Path,
    label: &str,
    cancel: &Arc<Cancel>,
) -> Result<(), String> {
    let mut cmd = ffmpeg::base_command()?;
    cmd.arg("-y").arg("-i").arg(src);
    cmd.args(["-vn", "-sn", "-dn", "-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(out);
    ffmpeg::run_cancellable(cmd, app, MEDIA_PROGRESS, info.duration, label, Some(cancel))
}

/// Re-encode the video to 8-bit H.264 + AAC, which every webview can play.
fn run_transcode(
    app: &AppHandle,
    src: &Path,
    info: &MediaInfo,
    out: &Path,
    label: &str,
    cancel: &Arc<Cancel>,
) -> Result<(), String> {
    let mut cmd = ffmpeg::base_command()?;
    cmd.arg("-y").arg("-i").arg(src);

    let encoders = ffmpeg::status().encoders;
    let enc = ffmpeg::h264_encoder(&encoders);
    cmd.args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"]);
    cmd.args(["-c:v", enc]);
    if enc == "libx264" {
        cmd.args(["-preset", "veryfast", "-crf", "23"]);
    } else {
        cmd.args(["-b:v", "6000k", "-allow_sw", "1"]);
    }
    // 10-bit / 4:4:4 / 4:2:2 sources must come down to 8-bit 4:2:0.
    cmd.args(["-pix_fmt", "yuv420p"]);
    if info.has_audio {
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
    }
    cmd.args(["-movflags", "+faststart"]);
    cmd.arg(out);
    ffmpeg::run_cancellable(cmd, app, MEDIA_PROGRESS, info.duration, label, Some(cancel))
}

// ---------------------------------------------------------------------------
// live transcoding (play a re-encode while it happens)
// ---------------------------------------------------------------------------

/// Start re-encoding `path` from `start` seconds and stream the result.
#[tauri::command]
pub async fn stream_start(path: String, start: Option<f64>) -> Result<StreamStart, String> {
    let start = start.unwrap_or(0.0);
    tauri::async_runtime::spawn_blocking(move || crate::stream::start(&path, start))
        .await
        .map_err(|e| e.to_string())?
}

/// Pull the next slice of the encoded stream (empty when it is finished).
#[tauri::command]
pub async fn stream_read(id: String, max: Option<usize>) -> Result<tauri::ipc::Response, String> {
    let max = max.unwrap_or(crate::stream::PULL_MAX);
    let bytes = tauri::async_runtime::spawn_blocking(move || crate::stream::pull(&id, max))
        .await
        .map_err(|e| e.to_string())??;
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
pub async fn stream_status(id: String) -> Result<StreamStatus, String> {
    tauri::async_runtime::spawn_blocking(move || crate::stream::status(&id))
        .await
        .map_err(|e| e.to_string())?
}

/// Kill a live encoder. Off the UI thread: a synchronous command would run on
/// the main thread, and `kill()` waits for the process to actually go away —
/// exactly the kind of hiccup that shows up as a stutter when switching files.
#[tauri::command]
pub async fn stream_stop(id: String) {
    let _ = tauri::async_runtime::spawn_blocking(move || crate::stream::stop(&id)).await;
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
