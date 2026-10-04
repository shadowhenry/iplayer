//! ffmpeg / ffprobe sidecar discovery and process helpers.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// Compile-time target triple (set by build.rs). Sidecars are shipped as
/// `binaries/ffmpeg-<triple>` and Tauri strips the suffix when bundling.
pub const TARGET_TRIPLE: &str = env!("BUILD_TARGET_TRIPLE");

static FFMPEG: OnceLock<Option<PathBuf>> = OnceLock::new();
static FFPROBE: OnceLock<Option<PathBuf>> = OnceLock::new();

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Common Homebrew locations (GUI apps often have a minimal PATH).
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let candidate = Path::new(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Every location a sidecar could live in, most specific first.
fn candidates(name: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Bundled: next to the main binary (Contents/MacOS/…)
            out.push(dir.join(name));
            out.push(dir.join(format!("{name}-{TARGET_TRIPLE}")));
            // Bundled alternative: Contents/Resources/…
            if let Some(contents) = dir.parent() {
                out.push(contents.join("Resources").join(name));
            }
        }
    }

    // Development: src-tauri/binaries/
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries");
    out.push(base.join(format!("{name}-{TARGET_TRIPLE}")));
    out.push(base.join(name));

    out
}

/// Verify a candidate actually runs before we commit to it — a bundled binary
/// can be present but fail to load (e.g. missing dylibs).
fn works(path: &Path) -> bool {
    Command::new(path)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn resolve(name: &str, cache: &'static OnceLock<Option<PathBuf>>) -> Option<PathBuf> {
    cache
        .get_or_init(|| {
            for c in candidates(name) {
                if c.is_file() && works(&c) {
                    return Some(c);
                }
            }
            if let Some(p) = find_in_path(name) {
                if works(&p) {
                    return Some(p);
                }
                // Last resort: trust it even if the probe failed.
                return Some(p);
            }
            None
        })
        .clone()
}

pub fn ffmpeg() -> Result<PathBuf, String> {
    resolve("ffmpeg", &FFMPEG).ok_or_else(|| {
        "未找到 ffmpeg。请将 ffmpeg 放入 src-tauri/binaries/ 或安装到系统 PATH。".to_string()
    })
}

pub fn ffprobe() -> Result<PathBuf, String> {
    resolve("ffprobe", &FFPROBE).ok_or_else(|| {
        "未找到 ffprobe。请将 ffprobe 放入 src-tauri/binaries/ 或安装到系统 PATH。".to_string()
    })
}

#[derive(Serialize, Clone)]
pub struct ToolStatus {
    pub ffmpeg: Option<String>,
    pub ffprobe: Option<String>,
    pub version: Option<String>,
    pub encoders: Vec<String>,
}

pub fn status() -> ToolStatus {
    let ff = ffmpeg().ok();
    let fp = ffprobe().ok();
    let version = ff
        .as_ref()
        .and_then(|p| Command::new(p).arg("-version").output().ok())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()));

    let encoders = ff
        .as_ref()
        .and_then(|p| Command::new(p).arg("-hide_banner").arg("-encoders").output().ok())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| {
            s.lines()
                .filter(|l| l.starts_with(" A") || l.starts_with(" V"))
                .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    ToolStatus {
        ffmpeg: ff.map(|p| p.display().to_string()),
        ffprobe: fp.map(|p| p.display().to_string()),
        version,
        encoders,
    }
}

/// Pick the best H.264 encoder this build ships with.
pub fn h264_encoder(encoders: &[String]) -> &'static str {
    if encoders.iter().any(|e| e == "libx264") {
        "libx264"
    } else if encoders.iter().any(|e| e == "h264_videotoolbox") {
        "h264_videotoolbox"
    } else {
        "libx264"
    }
}

// ---------------------------------------------------------------------------
// cancellation
// ---------------------------------------------------------------------------

/// Error text a cancelled job fails with. The frontend compares against it to
/// stay silent — nobody wants a red toast for work they deliberately left.
pub const CANCELLED: &str = "已取消";

/// A one-shot cancellation flag shared with a long-running ffmpeg job.
///
/// `-progress pipe:1` makes ffmpeg emit a block of `key=value` lines on a
/// wall-clock timer (twice a second), so a job only has to look at the flag
/// between lines to notice a cancel — no signals, no pid bookkeeping, and it
/// reacts in a few hundred milliseconds no matter what the encoder is doing.
#[derive(Default)]
pub struct Cancel {
    flag: AtomicBool,
}

impl Cancel {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
}

#[cfg(target_os = "windows")]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console(_cmd: &mut Command) {}

/// Build a base ffmpeg command with sane, quiet defaults.
pub fn base_command() -> Result<Command, String> {
    let bin = ffmpeg()?;
    let mut cmd = Command::new(bin);
    cmd.arg("-hide_banner").arg("-loglevel").arg("error");
    hide_console(&mut cmd);
    Ok(cmd)
}

pub fn base_ffprobe_command() -> Result<Command, String> {
    let bin = ffprobe()?;
    let mut cmd = Command::new(bin);
    cmd.arg("-hide_banner");
    hide_console(&mut cmd);
    Ok(cmd)
}

/// Run an ffmpeg command synchronously, streaming progress to the frontend.
///
/// `duration` is the media duration in seconds (0 when unknown); progress is
/// emitted on `event` as `{ "percent": f64, "label": String }`.
pub fn run_with_progress(
    cmd: Command,
    app: &AppHandle,
    event: &str,
    duration: f64,
    label: &str,
) -> Result<(), String> {
    run_cancellable(cmd, app, event, duration, label, None)
}

/// `run_with_progress`, but abortable. When `cancel` trips, the encoder is
/// killed and `CANCELLED` comes back so the caller can drop the half-written
/// output instead of caching it.
pub fn run_cancellable(
    mut cmd: Command,
    app: &AppHandle,
    event: &str,
    duration: f64,
    label: &str,
    cancel: Option<&Arc<Cancel>>,
) -> Result<(), String> {
    cmd.arg("-progress").arg("pipe:1").arg("-nostats");
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("无法启动 ffmpeg: {e}"))?;

    let stdout = child.stdout.take().ok_or("无法读取 ffmpeg 输出")?;
    let mut stderr = child.stderr.take();

    // Drain stderr on a separate thread so the pipe buffer can never fill up
    // and deadlock the encoder.
    let err_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut s) = stderr.take() {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });

    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(Result::ok) {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                let _ = err_handle.join();
                return Err(CANCELLED.to_string());
            }
        }
        if let Some(value) = line.strip_prefix("out_time_us=") {
            if let Ok(us) = value.trim().parse::<i64>() {
                if us >= 0 && duration > 0.0 {
                    let pct = ((us as f64 / 1_000_000.0) / duration * 100.0).clamp(0.0, 99.9);
                    let _ = app.emit(
                        event,
                        serde_json::json!({ "percent": pct, "label": label }),
                    );
                }
            }
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let err_text = err_handle.join().unwrap_or_default();

    if !status.success() {
        let detail = err_text.trim();
        return Err(if detail.is_empty() {
            "ffmpeg 处理失败".to_string()
        } else {
            format!("ffmpeg 处理失败: {}", detail.lines().last().unwrap_or(detail))
        });
    }

    let _ = app.emit(event, serde_json::json!({ "percent": 100.0, "label": label }));
    Ok(())
}
